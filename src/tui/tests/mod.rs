mod control;
mod lend;
mod modal;
mod rewind;
mod run_loop;

use super::*;
use crate::client::{ClientConfig, Model};
use crate::config::{AppState, Input, Session, SessionScope, WorkingMode};
use crate::events::{Event, EventSink, Origin};
use crate::prompt::{PromptRequest, Prompter};
use crate::repl::EXIT_CANCEL_WAIT;
use crate::supervisor::notification::NotificationQueue;
use crate::testing::TestConfigDirGuard;
use crate::tui::control::{
    TurnOutcome, late_output, spawn_control_request, spawn_turn, spawn_work,
};
use crate::tui::events::SystemClock;
use crate::tui::queued::{carry_over, reconcile_queue, take_user_messages};
use crate::tui::sink::{TuiPrompter, TuiSink};
use crate::utils::create_abort_signal;
use bottom_pane::RewindState;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};
use events::ManualClock;
use keys::ModalKind;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serial_test::serial;
use std::future::Future;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

fn plain_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        highlight: false,
        save_shell_history: false,
        ..Default::default()
    })
}

fn chat_ctx() -> RequestContext {
    let mut app = AppState::test_default();
    app.config = Arc::new(AppConfig {
        dry_run: true,
        highlight: false,
        save_shell_history: false,
        clients: vec![ClientConfig::default()],
        memory: Some(false),
        workspace_instructions: Some(false),
        stream: true,
        ..Default::default()
    });
    let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
    ctx.model = Model::new("openai", "test-model");
    ctx
}

struct Harness {
    clock: Arc<ManualClock>,
    app: App,
    terminal: Terminal<TestBackend>,
}

impl Harness {
    fn new(width: u16, height: u16, config: Arc<AppConfig>, footer: FooterInfo) -> Self {
        let clock = Arc::new(ManualClock::new());
        let app = App::new(
            Arc::clone(&clock) as Arc<dyn Clock>,
            config,
            Arc::new(PromptMemory::default()),
            footer,
        )
        .unwrap();
        let terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        Self {
            clock,
            app,
            terminal,
        }
    }

    fn standalone(width: u16, height: u16) -> Self {
        let footer = FooterInfo {
            model: "openai:test-model".to_string(),
            ..FooterInfo::default()
        };
        Self::new(width, height, plain_config(), footer)
    }

    fn for_ctx(ctx: &RequestContext) -> Self {
        Self::new(
            80,
            24,
            Arc::clone(&ctx.app.config),
            FooterInfo::from_ctx(ctx),
        )
    }

    fn render(&mut self) {
        self.terminal.draw(|frame| self.app.render(frame)).unwrap();
    }

    /// `TestBackend` cannot print a zero-width buffer, so the tiny
    /// screens go through `render` instead.
    fn draw(&mut self) -> String {
        self.render();
        self.terminal.backend().to_string()
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.terminal.backend_mut().resize(width, height);
        self.app.handle_input(TermInput::Resize);
    }

    fn press(&mut self, code: KeyCode) -> Option<Command> {
        self.press_with_ctx(code, None)
    }

    /// A key followed by the palette refresh the loop runs after
    /// every input, with the context in hand when `ctx` is given.
    fn press_with_ctx(&mut self, code: KeyCode, ctx: Option<&RequestContext>) -> Option<Command> {
        let command = self.app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        self.app.refresh_palette(ctx);
        command
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }
}

/// Seats the UI's sink and prompter on `ctx` the way `run` does and
/// hands back the loop's inbox.
fn seat_tui(ctx: &mut RequestContext) -> (UnboundedSender<UiMsg>, UnboundedReceiver<UiMsg>) {
    let (tx, rx) = unbounded_channel();
    ctx.ui = Arc::new(TuiSink::new(tx.clone()));
    ctx.prompter = Arc::new(TuiPrompter::new(
        tx.clone(),
        Arc::new(AtomicBool::new(false)),
    ));
    (tx, rx)
}

fn confirm_request(tool: Option<&str>) -> PromptRequest {
    PromptRequest {
        tool: tool.map(str::to_string),
        ..PromptRequest::new(
            PromptAction::Confirm,
            "Proceed? tui-x7x",
            Origin::detached(),
        )
    }
}

/// How long a test waits for an asker's answer before it is declared
/// stuck; the alternative is a test that never ends.
const ASKER_WAIT: Duration = Duration::from_secs(5);

/// A prompt asked on its own thread; `join` is bounded by
/// `ASKER_WAIT`.
struct Asker {
    answer: mpsc::Receiver<Result<PromptAnswer>>,
    thread: std::thread::JoinHandle<()>,
}

impl Asker {
    fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    fn join(self) -> std::thread::Result<Result<PromptAnswer>> {
        match self.answer.recv_timeout(ASKER_WAIT) {
            Ok(answer) => self.thread.join().map(|()| answer),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("the asker got no answer within {ASKER_WAIT:?}")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self
                .thread
                .join()
                .expect_err("a thread that sent no answer panicked")),
        }
    }
}

/// Asks through the prompter on its own thread, the way a tool would,
/// and delivers the request to the app.
fn ask_on_a_thread(harness: &mut Harness, req: PromptRequest) -> Asker {
    let (tx, mut rx) = unbounded_channel();
    let prompter = TuiPrompter::new(tx, Arc::new(AtomicBool::new(false)));
    let (answer_tx, answer) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = answer_tx.send(prompter.prompt(req));
    });
    let msg = rx.blocking_recv().expect("the prompter sends the request");
    harness.app.handle_msg(msg);
    Asker { answer, thread }
}

fn duplicated_adjacent_rows(rendered: &str) -> usize {
    let rows: Vec<&str> = rendered
        .lines()
        .map(str::trim_end)
        .filter(|row| !row.trim_matches(['"', ' ']).is_empty())
        .collect();
    rows.windows(2).filter(|pair| pair[0] == pair[1]).count()
}

/// Snapshots a frame that shows the welcome block. The version in
/// its wordmark changes with every release, so the row is folded to
/// a fixed text together with the padding that follows it, which
/// would otherwise shift with the version's length.
fn assert_welcome_snapshot(name: &str, frame: &str) {
    insta::with_settings!(
        { filters => vec![(r"coyote \S+ +", "coyote X.Y.Z ")] },
        { insta::assert_snapshot!(name, frame) }
    );
}

/// How long a `run_async` body may take before the test is declared
/// stuck; a turn that waits on a reply that never comes would
/// otherwise hang the whole binary.
const RUN_ASYNC_WAIT: Duration = Duration::from_secs(60);

/// Drives a turn on a runtime whose workers have the stack headroom
/// `run_repl_command` needs in debug builds; a default 2 MiB worker
/// overflows and aborts the whole test binary. The body is bounded
/// by `RUN_ASYNC_WAIT`.
fn run_async<F>(f: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    const STACK: usize = 8 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(scope, || {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_stack_size(STACK)
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        tokio::time::timeout(RUN_ASYNC_WAIT, f)
                            .await
                            .unwrap_or_else(|_| {
                                panic!(
                                    "the async test body did not finish within {RUN_ASYNC_WAIT:?}"
                                )
                            })
                    })
            })
            .unwrap()
            .join()
            .unwrap()
    })
}

/// Tool output that never breaks its line keeps only its newest
/// text, the same bound the late-output cell has.
#[test]
fn a_newline_free_tool_stream_keeps_a_bounded_tail() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness
        .app
        .append_tool_output(Some("c1".to_string()), b"head-m3z-");
    harness.app.append_tool_output(
        Some("c1".to_string()),
        "y".repeat(LATE_OUTPUT_LIMIT).as_bytes(),
    );
    harness
        .app
        .append_tool_output(Some("c1".to_string()), "tail-m3z".as_bytes());
    let Some(Cell::Text { lines, .. }) = harness.app.transcript.cells().last() else {
        panic!("the tool output did not land in a text cell");
    };
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), LATE_OUTPUT_LIMIT);
    assert!(
        lines[0].ends_with("tail-m3z"),
        "the newest text was dropped"
    );
    assert!(!lines[0].contains("head-m3z"), "the oldest text was kept");
}

#[test]
fn text_deltas_grow_the_live_reply_in_place() {
    let mut harness = Harness::standalone(80, 24);
    harness
        .app
        .handle_msg(UiMsg::UsageUpdated("\x1b[36m[low] \x1b[0m".to_string()));
    harness.app.begin_turn("tell me a story", false);
    for (i, delta) in [
        "Once upon a **time**",
        " there was a paragraph.\n\nThen a second one,",
        " still arriving.",
    ]
    .into_iter()
    .enumerate()
    {
        harness.app.handle_msg(UiMsg::TextDelta(delta.to_string()));
        let rendered = harness.draw();
        insta::assert_snapshot!(format!("text_delta_{}", i + 1), rendered);
    }
    assert_eq!(harness.app.transcript.len(), 2);
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Assistant { source, done: false, .. }
            if source.ends_with("second one, still arriving.")
    ));
    harness.app.handle_msg(UiMsg::StreamDone);
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Assistant { done: true, .. }
    ));
}

/// The harness reports from other test threads land in the fd capture
/// too, so the captured text is dropped before the final frame.
#[test]
#[serial]
fn a_dry_run_turn_streams_into_the_transcript() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-stream");
        let mut ctx = chat_ctx();
        let (tx, mut rx) = seat_tui(&mut ctx);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.begin_turn("hello", false);
        let mut handle = spawn_turn(ctx, create_abort_signal(), "hello".to_string(), tx);

        let mut first_delta = None;
        let (mut returned, outcome) = loop {
            tokio::select! {
                biased;
                Some(msg) = rx.recv() => {
                    let delta = matches!(msg, UiMsg::TextDelta(_));
                    harness.app.handle_msg(msg);
                    if delta {
                        let rendered = harness.draw();
                        if first_delta.is_none() {
                            first_delta = Some(rendered);
                        }
                    }
                }
                joined = &mut handle => break joined.unwrap(),
            }
        };
        while let Ok(msg) = rx.try_recv() {
            harness.app.handle_msg(msg);
        }
        let first_delta = first_delta.expect("the dry run streamed at least one delta");
        insta::assert_snapshot!("stream_first_delta", first_delta);

        assert!(!outcome.exit);
        assert_eq!(outcome.error, None);
        harness.app.end_turn(
            false,
            &TurnOutcome {
                captured: String::new(),
                ..outcome
            },
        );
        harness.app.sync_from_ctx(&mut returned);
        let final_frame = harness.draw();
        insta::assert_snapshot!("stream_final", &final_frame);
        // What tells the two frames apart: the status row shows the
        // running turn under the first delta and is empty once the
        // turn has ended.
        assert!(first_delta.contains("Working"), "{first_delta}");
        assert!(first_delta.contains("Esc to interrupt"), "{first_delta}");
        assert!(!final_frame.contains("Working"), "{final_frame}");
        assert!(!final_frame.contains("Esc to interrupt"), "{final_frame}");
        assert!(matches!(
            harness.app.transcript.cells().last(),
            Some(Cell::Assistant { source, done: true, .. }) if source.contains("hello")
        ));
    });
}

/// Under the test harness `print!` is diverted into libtest's own
/// buffer before it reaches fd 1, so the command's text cannot be seen
/// here; the pty test on the binary pins that. This pins the turn's
/// shape: it runs the REPL command, ends clean and leaves one cell.
#[test]
#[serial]
fn a_command_turn_becomes_one_preformatted_cell() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-info");
        let mut ctx = chat_ctx();
        let (tx, mut rx) = seat_tui(&mut ctx);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.begin_turn(".info", true);
        let (mut returned, outcome) =
            spawn_turn(ctx, create_abort_signal(), ".info".to_string(), tx)
                .await
                .unwrap();
        while let Ok(msg) = rx.try_recv() {
            harness.app.handle_msg(msg);
        }
        assert_eq!(outcome.error, None);
        harness.app.end_turn(true, &outcome);
        harness.app.sync_from_ctx(&mut returned);

        let commands: Vec<&Cell> = harness
            .app
            .transcript
            .cells()
            .iter()
            .filter(|cell| matches!(cell, Cell::Command { .. }))
            .collect();
        assert_eq!(commands.len(), 1);
        let Cell::Command { name, .. } = commands[0] else {
            unreachable!()
        };
        assert_eq!(name, ".info");
        assert!(harness.app.turn.is_none());
        let rendered = harness.draw();
        assert!(rendered.contains("> .info"), "{rendered}");
    });
}

/// The `.info` listing reaches its cell as printed: label column,
/// value column, model row first. (Under libtest the turn's `print!`
/// never reaches the fd capture, so the listing is taken from the
/// context directly; the pty case pins the capture on the binary.)
#[test]
#[serial]
fn the_info_listing_keeps_its_rows_in_the_command_cell() {
    let _guard = TestConfigDirGuard::new("tui-info-rows");
    let ctx = chat_ctx();
    let info = ctx.info(&ctx.app.config).unwrap();
    let model_row = info
        .lines()
        .find(|row| row.starts_with("model "))
        .expect("the listing has a model row");
    assert!(model_row.contains("openai:test-model"), "{model_row:?}");

    // Tall enough to hold the whole listing, so no row scrolls off.
    let mut harness = Harness::new(
        100,
        80,
        Arc::clone(&ctx.app.config),
        FooterInfo::from_ctx(&ctx),
    );
    harness.app.begin_turn(".info", true);
    harness.app.end_turn(
        true,
        &TurnOutcome {
            captured: info.clone(),
            ..TurnOutcome::default()
        },
    );
    let Some(Cell::Command { output, .. }) = harness.app.transcript.cells().last() else {
        panic!("the command turn left no Command cell");
    };
    assert_eq!(output, &info);
    let rendered = harness.draw();
    assert!(rendered.contains(model_row), "{rendered}");
}

/// A command's output is painted as it came: an aligned column stays
/// aligned and a row wider than the screen is clipped, not wrapped.
#[test]
fn command_output_keeps_its_columns_and_clips() {
    let mut harness = Harness::standalone(100, 30);
    harness.app.begin_turn(".info", true);
    let wide = "x".repeat(120);
    let output = format!(
        "model                         openai:test-model\n\
         temperature                   -\n\
         \x1b[1mbold_key\x1b[0m                      value\n\
         {wide}\n"
    );
    harness.app.end_turn(
        true,
        &TurnOutcome {
            exit: false,
            error: None,
            captured: output,
        },
    );
    let rendered = harness.draw();
    insta::assert_snapshot!("info_command_cell_100x30", &rendered);
    assert!(rendered.contains("\"model                         openai:test-model"));
    assert_eq!(duplicated_adjacent_rows(&rendered), 0);
}

#[test]
#[serial]
fn exit_session_asks_to_save_through_the_modal() {
    run_async(async {
        let guard = TestConfigDirGuard::new("tui-exit");
        let mut ctx = chat_ctx();
        let (tx, mut rx) = seat_tui(&mut ctx);
        let mut session = Session::default();
        session.set_name("tui-exit-x7x".to_string());
        session.set_save_session(None);
        session.mark_dirty();
        ctx.session = Some(session);
        let sessions_dir = ctx.sessions_dir_for(SessionScope::Global);
        assert!(sessions_dir.starts_with(&guard.path));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.begin_turn(".exit session", true);
        let mut handle = spawn_turn(ctx, create_abort_signal(), ".exit session".to_string(), tx);

        loop {
            tokio::select! {
                biased;
                Some(msg) = rx.recv() => {
                    let prompt = matches!(msg, UiMsg::Prompt { .. });
                    harness.app.handle_msg(msg);
                    if prompt {
                        break;
                    }
                }
                joined = &mut handle => panic!("the turn ended without asking: {:?}", joined.unwrap().1),
            }
        }
        let prompt = harness.app.prompts.front().unwrap();
        assert_eq!(prompt.req.question, "Save session?");
        assert_eq!(
            prompt.kind(),
            ModalKind::Confirm {
                default: false,
                tool: false
            }
        );
        insta::assert_snapshot!("exit_session_save_modal", harness.draw());

        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert!(harness.app.prompts.is_empty());
        let (_returned, outcome) = handle.await.unwrap();
        assert!(!outcome.exit);
        assert_eq!(outcome.error, None);
        assert!(
            sessions_dir.join("tui-exit-x7x.yaml").is_file(),
            "{outcome:?}"
        );
    });
}

/// A tool sleeping on the work task never stalls the composer, even on
/// a runtime with a single worker: the frame timer fires, and the keys
/// and a resize handled meanwhile are drawn, while the tool is still
/// running. The tool goes through `spawn_work` like a real turn; had
/// it sat on the lone worker, the time driver would wait out the whole
/// sleep and the frame with it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial]
async fn the_composer_stays_responsive_while_a_tool_sleeps() {
    use crate::function::{ToolIo, run_llm_function};
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const SLEEP: Duration = Duration::from_secs(2);
    let _guard = TestConfigDirGuard::new("tui-sleep");
    let bin = crate::config::paths::functions_bin_dir();
    fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("sleep_x7x");
    fs::write(
        &tool,
        format!("#!/usr/bin/env bash\nsleep {}\n", SLEEP.as_secs()),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    // The frame timer is what is under test, so the app runs on the
    // real clock rather than the harness's manual one.
    let footer = FooterInfo {
        model: "openai:test-model".to_string(),
        ..FooterInfo::default()
    };
    let mut app = App::new(
        Arc::new(SystemClock),
        plain_config(),
        Arc::new(PromptMemory::default()),
        footer,
    )
    .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let (tx, _rx) = unbounded_channel();
    app.begin_turn("run the tool", false);
    let started = Instant::now();
    let handle = spawn_work(async move {
        run_llm_function(
            "sleep_x7x".to_string(),
            vec![],
            HashMap::from([("TERM".to_string(), "dumb".to_string())]),
            None,
            None,
            ToolIo::new(Arc::new(TuiSink::new(tx)), None, 0),
            None,
        )
    });

    for c in "hello".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    terminal.backend_mut().resize(60, 20);
    app.handle_input(TermInput::Resize);
    app.schedule_frame();
    app.frames.next_frame().await;
    let frame_at = started.elapsed();
    assert!(
        frame_at < SLEEP / 4,
        "the frame waited {frame_at:?} on the sleeping tool"
    );
    assert!(!handle.is_finished(), "the tool finished in {frame_at:?}");
    terminal.draw(|frame| app.render(frame)).unwrap();
    app.frames.frame_drawn();
    let rendered = terminal.backend().to_string();
    assert_eq!(app.composer_text(), "hello");
    // The app runs on the real clock here, so the elapsed seconds in
    // the status row depend on the host's speed.
    // Width-preserving: the clock's digit becomes `N`, so the row keeps
    // its length and the snapshot its columns.
    insta::with_settings!({ filters => vec![(r"Working \. \d+s", "Working . Ns")] }, {
        insta::assert_snapshot!("sleeping_tool_composer_responsive_60x20", rendered);
    });

    handle.await.unwrap().unwrap();
    assert!(started.elapsed() >= SLEEP);
}

/// A paste keeps its line breaks whichever line end the terminal
/// used: CR, CRLF and LF all split the composer the same way.
#[test]
fn a_paste_with_carriage_returns_lands_line_by_line() {
    let mut harness = Harness::standalone(80, 24);
    harness
        .app
        .handle_input(TermInput::Paste("a\rb\r\nc\nd".to_string()));
    assert_eq!(harness.app.composer.lines(), ["a", "b", "c", "d"]);
    assert!(
        !harness.app.composer_text().contains('\r'),
        "{:?}",
        harness.app.composer_text()
    );
}

/// A prompt entered during a turn is held and handed to the loop,
/// whose step puts it on the turn's queue and announces it; a command
/// is only held. Alt+Up takes the newest back, newest first.
#[test]
#[serial]
fn lines_entered_during_a_turn_are_queued_and_prompts_reach_the_model() {
    let sink = crate::events::RecordingSink::install();
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.type_text("hello q9d-q");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Queue("hello q9d-q".to_string()))
    );
    assert_eq!(harness.app.composer_text(), "");
    let frame = harness.draw();
    assert!(frame.contains("pending: 1"), "{frame}");
    assert!(frame.contains("hello q9d-q"), "{frame}");

    let queue = NotificationQueue::new();
    queued::queue_user_message(&queue, "hello q9d-q");
    assert_eq!(
        sink.drain(),
        vec![Event::QueuedInput {
            text: "hello q9d-q".to_string()
        }]
    );
    let drained = queue.drain();
    assert_eq!(drained.len(), 1);
    assert!(matches!(
        &drained[0],
        crate::supervisor::notification::SystemNotification::UserMessage { text }
            if text == "hello q9d-q"
    ));

    harness.type_text(".info");
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        harness.app.queued,
        vec![
            QueuedLine {
                text: "hello q9d-q".to_string(),
                command: false
            },
            QueuedLine {
                text: ".info".to_string(),
                command: true
            },
        ]
    );
    assert!(sink.drain().is_empty());
    let frame = harness.draw();
    assert!(frame.contains("pending: 2"), "{frame}");

    let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(harness.app.handle_key(alt_up), None);
    assert_eq!(harness.app.composer_text(), ".info");
    // The second Alt+Up finds `.info` as the draft and holds it again.
    assert_eq!(
        harness.app.handle_key(alt_up),
        Some(Command::Unqueue {
            held: Some("hello q9d-q".to_string()),
            draft: None,
        })
    );
    assert_eq!(harness.app.composer_text(), "hello q9d-q");
    assert_eq!(
        harness.app.queued,
        vec![QueuedLine {
            text: ".info".to_string(),
            command: true
        }]
    );
    assert!(harness.draw().contains("pending: 1"));
}

/// Alt+Up over a draft trades places with it: the held line comes
/// out, the draft goes to the end of the queue, and the loop is told
/// to withdraw the one and offer the other.
#[test]
fn editing_a_queued_line_keeps_the_draft_on_the_queue() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.type_text("a");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Queue("a".to_string()))
    );
    harness.type_text("d");
    let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(
        harness.app.handle_key(alt_up),
        Some(Command::Unqueue {
            held: Some("a".to_string()),
            draft: Some("d".to_string()),
        })
    );
    assert_eq!(harness.app.composer_text(), "a");
    assert_eq!(
        harness.app.queued,
        vec![QueuedLine {
            text: "d".to_string(),
            command: false
        }]
    );
}

/// The strip follows the model: a line it was handed leaves the queue
/// the moment the cell is committed, so the count is right and Alt+Up
/// cannot pull a sent line back into the composer.
#[test]
fn a_line_sent_mid_turn_leaves_the_queue_at_once() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    for text in ["a", "b"] {
        harness.type_text(text);
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Queue(text.to_string()))
        );
    }
    assert!(harness.draw().contains("pending: 2"));
    harness
        .app
        .handle_msg(UiMsg::QueuedInputSent("a".to_string()));
    assert_eq!(
        harness.app.queued,
        vec![QueuedLine {
            text: "b".to_string(),
            command: false
        }]
    );
    let frame = harness.draw();
    assert!(frame.contains("pending: 1"), "{frame}");
    assert!(frame.contains("> a (sent mid-turn)"), "{frame}");
    let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(
        harness.app.handle_key(alt_up),
        Some(Command::Unqueue {
            held: Some("b".to_string()),
            draft: None,
        })
    );
    assert_eq!(harness.app.composer_text(), "b");
    assert!(harness.app.queued.is_empty());
}

/// The moment the model is handed a queued line, the transcript gets
/// a cell for it where it was read, scrubbed like a typed line.
#[test]
fn a_line_sent_mid_turn_is_committed_as_a_scrubbed_cell() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness
        .app
        .handle_msg(UiMsg::QueuedInputSent("\x1b[31mred\x1b[0m".to_string()));
    assert!(matches!(
        harness.app.transcript.cells(),
        [
            Cell::User {
                mid_turn: false,
                ..
            },
            Cell::User {
                text,
                mid_turn: true,
                opened: false,
                ..
            }
        ] if text == "red"
    ));
}

/// A user line, a reply, the line sent mid-turn, the next reply. The
/// mid-turn cell carries its marker in the text itself and is drawn
/// dim, so it reads as sent-by-the-queue with or without colour.
fn mid_turn_transcript(harness: &mut Harness) -> String {
    harness.app.begin_turn("explain", false);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("first reply".to_string()));
    harness
        .app
        .handle_msg(UiMsg::QueuedInputSent("follow-up k4e-m".to_string()));
    harness
        .app
        .handle_msg(UiMsg::AssistantText("second reply".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw()
}

#[test]
fn the_mid_turn_cell_is_dim_with_a_marker() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(false);
    let frame = mid_turn_transcript(&mut harness);
    insta::assert_snapshot!("mid_turn_cell_80x24", &frame);
    let row = screen_row(&harness, "> follow-up k4e-m (sent mid-turn)");
    let buffer = harness.terminal.backend().buffer();
    assert!(buffer[(0, row)].modifier.contains(Modifier::DIM), "{frame}");
    assert!(
        !buffer[(0, row)].modifier.contains(Modifier::BOLD),
        "{frame}"
    );
    let typed = screen_row(&harness, "> explain");
    assert!(
        buffer[(0, typed)].modifier.contains(Modifier::BOLD),
        "{frame}"
    );
    assert!(
        !buffer[(0, typed)].modifier.contains(Modifier::DIM),
        "{frame}"
    );
    let placeholder = &buffer[(1, COMPOSER_TEXT_ROW)];
    assert_eq!(placeholder.symbol(), "T", "{frame}");
    assert!(placeholder.modifier.contains(Modifier::DIM), "{frame}");
    assert_eq!(placeholder.bg, Color::Indexed(236), "{frame}");
}

#[test]
fn the_mid_turn_marker_survives_no_color() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(true);
    let frame = mid_turn_transcript(&mut harness);
    insta::assert_snapshot!("mid_turn_cell_nocolor_80x24", &frame);
    assert!(frame.contains("(sent mid-turn)"), "{frame}");
    let row = screen_row(&harness, "> follow-up k4e-m (sent mid-turn)");
    let buffer = harness.terminal.backend().buffer();
    assert!(buffer[(0, row)].modifier.contains(Modifier::DIM), "{frame}");
    let placeholder = &buffer[(1, COMPOSER_TEXT_ROW)];
    assert_eq!(placeholder.symbol(), "T", "{frame}");
    assert!(placeholder.modifier.contains(Modifier::DIM), "{frame}");
    assert_eq!(placeholder.bg, Color::Reset, "{frame}");
}

/// Esc during a turn drops what was lined up behind it and says so.
/// The second held line went in through the palette, which the queue
/// closes: the Esc that follows reaches the turn, not a band.
#[test]
fn an_interrupt_drops_the_queued_lines_with_a_notice() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.type_text("later");
    harness.press(KeyCode::Enter);
    harness.type_text(".info");
    assert!(palette(&harness).is_some());
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(harness.app.queued.len(), 2);
    assert!(harness.app.composer.is_empty());
    assert!(palette(&harness).is_none());
    assert_eq!(harness.app.vi_mode, None);
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert!(harness.app.queued.is_empty());
    let frame = harness.draw();
    assert!(frame.contains("Dropped 2 queued line(s)"), "{frame}");
    assert!(!frame.contains("pending:"), "{frame}");
}

/// Ctrl+D during a turn quits, and what was lined up behind the turn
/// goes the same way as on an interrupt, with the same notice.
#[test]
fn a_quit_drops_the_queued_lines_with_a_notice() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.type_text("later");
    harness.press(KeyCode::Enter);
    assert_eq!(harness.app.queued.len(), 1);
    let quit = harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(quit, Some(Command::Quit));
    assert!(harness.app.queued.is_empty());
    let frame = harness.draw();
    assert!(frame.contains("Dropped 1 queued line(s)"), "{frame}");
}

/// A line queued after the interrupt, before the turn actually ends,
/// survives the reconcile even when the dropped one is still on the
/// queue the turn hands back.
#[test]
#[serial]
fn a_line_queued_after_an_interrupt_survives_the_reconcile() {
    let mut harness = Harness::standalone(80, 24);
    let queue = NotificationQueue::new();
    harness.app.begin_turn("explain", false);
    harness.type_text("a");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Queue("a".to_string()))
    );
    queued::queue_user_message(&queue, "a");
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert!(harness.app.queued.is_empty());
    harness.type_text("b");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Queue("b".to_string()))
    );
    queued::queue_user_message(&queue, "b");

    reconcile_queue(&mut harness.app.queued, &take_user_messages(&queue));
    let texts: Vec<&str> = harness.app.queued.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["b"]);
}

/// The lines left over at the end of a turn go on the queue of the
/// context that came back, the one the next turn clones. A `.agent`
/// turn hands back a context with a fresh queue, and what the end of
/// the turn put on the old one would never be taken again.
#[test]
fn leftover_lines_are_carried_on_the_returned_contexts_queue() {
    let mut harness = Harness::standalone(80, 24);
    let mut ctx = chat_ctx();
    let finished = Arc::clone(&ctx.notification_queue);
    harness.app.begin_turn(".agent helper", true);
    harness.type_text("first q9d-a");
    harness.press(KeyCode::Enter);
    harness.type_text("second q9d-b");
    harness.press(KeyCode::Enter);
    carry_over(&finished, &harness.app.queued);
    ctx.notification_queue = Arc::new(NotificationQueue::new());
    harness.app.end_turn(true, &TurnOutcome::default());

    reconcile_queue(&mut harness.app.queued, &take_user_messages(&finished));
    assert_eq!(harness.app.queued.len(), 2);
    let next = harness.app.queued.remove(0);
    assert_eq!(next.text, "first q9d-a");
    carry_over(&ctx.notification_queue, &harness.app.queued);
    assert_eq!(
        take_user_messages(&ctx.notification_queue),
        ["second q9d-b"]
    );
    assert!(take_user_messages(&finished).is_empty());
}

/// The readline chords a user reaches for first, sent through the
/// app's key handling, edit the composer and an input question's
/// field alike. Each starts from a line with a cut to paste back and
/// an edit to undo, the cursor inside a word; a marker typed after
/// the chord shows where the cursor went.
#[test]
fn readline_chords_edit_the_composer_and_an_input_field() {
    use ratatui_textarea::CursorMove;
    let key = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
    let ctrl = |c: char| key(KeyCode::Char(c), KeyModifiers::CONTROL);
    let alt = |c: char| key(KeyCode::Char(c), KeyModifiers::ALT);
    let marker = key(KeyCode::Char('|'), KeyModifiers::NONE);
    let chords = [
        alt('f'),
        alt('b'),
        ctrl('w'),
        ctrl('u'),
        ctrl('k'),
        ctrl('y'),
        ctrl('e'),
    ];

    let composer_after = |chord: Option<KeyEvent>| {
        let mut harness = Harness::standalone(80, 24);
        harness.app.composer.set_text("alpha beta gamma delta");
        harness.app.composer.input(ctrl('w'));
        harness.app.composer.move_cursor(keys::ViMotion::Head);
        harness
            .app
            .composer
            .move_cursor(keys::ViMotion::WordForward);
        harness.app.composer.move_cursor(keys::ViMotion::Forward);
        for key in chord.into_iter().chain([marker]) {
            assert_eq!(harness.app.handle_key(key), None, "{key:?}");
        }
        harness.app.composer_text()
    };
    let unmoved = composer_after(None);
    assert_eq!(unmoved, "alpha b|eta gamma ");
    for chord in chords {
        assert_ne!(
            composer_after(Some(chord)),
            unmoved,
            "{chord:?} in the composer"
        );
    }

    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest::new(PromptAction::Input, "Session name:", Origin::detached());
    let asker = ask_on_a_thread(&mut harness, req);
    let mut field_after = |chord: Option<KeyEvent>| {
        let field = harness
            .app
            .prompts
            .front_mut()
            .unwrap()
            .field
            .as_mut()
            .unwrap();
        *field = new_field(false);
        field.insert_str("alpha beta gamma delta");
        field.delete_word();
        field.move_cursor(CursorMove::Head);
        field.move_cursor(CursorMove::WordForward);
        field.move_cursor(CursorMove::Forward);
        for key in chord.into_iter().chain([marker]) {
            assert_eq!(harness.app.key_context().modal, Some(ModalKind::Input));
            assert_eq!(harness.app.handle_key(key), None, "{key:?}");
        }
        let field = harness.app.prompts.front().unwrap().field.as_ref().unwrap();
        field.lines().join("\n")
    };
    let unmoved = field_after(None);
    assert_eq!(unmoved, "alpha b|eta gamma ");
    for chord in chords {
        assert_ne!(field_after(Some(chord)), unmoved, "{chord:?} in the field");
    }
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// Ctrl+U cuts from the cursor back to the start of its own line of a
/// multi-line draft, leaving the other lines alone, and at the start
/// of a line joins it to the one above; Ctrl+Y pastes the cut back.
#[test]
fn ctrl_u_cuts_back_to_the_start_of_the_cursor_line() {
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let mut harness = Harness::standalone(80, 24);
    harness
        .app
        .composer
        .set_text("first line\nsecond line\nthird");
    harness.app.composer.move_cursor(keys::ViMotion::Up);
    harness.app.composer.move_cursor(keys::ViMotion::Head);
    harness
        .app
        .composer
        .move_cursor(keys::ViMotion::WordForward);
    assert_eq!(harness.app.handle_key(ctrl('u')), None);
    assert_eq!(harness.app.composer_text(), "first line\nline\nthird");
    assert_eq!(harness.app.handle_key(ctrl('u')), None);
    assert_eq!(harness.app.composer_text(), "first lineline\nthird");
    harness.type_text("|");
    assert_eq!(harness.app.composer_text(), "first line|line\nthird");
}

/// Ctrl+U in a question's text field cuts back to the start of the
/// field and leaves the composer alone.
#[test]
fn ctrl_u_cuts_back_to_the_start_of_an_input_field() {
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    let req = PromptRequest::new(PromptAction::Input, "Name:", Origin::detached());
    let asker = ask_on_a_thread(&mut harness, req);
    harness.type_text("alpha beta");
    harness
        .app
        .prompts
        .front_mut()
        .unwrap()
        .field
        .as_mut()
        .unwrap()
        .move_cursor(ratatui_textarea::CursorMove::WordBack);
    assert_eq!(harness.app.handle_key(ctrl('u')), None);
    let field = harness.app.prompts.front().unwrap().field.as_ref().unwrap();
    assert_eq!(field.lines(), ["beta"]);
    assert_eq!(harness.app.composer_text(), "draft");
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// Ctrl+_, as a legacy terminal sends it (one byte that also stands
/// for Ctrl+/ and Ctrl+7) or a kitty-protocol one does (Ctrl+Shift+-),
/// undoes the last edit in the composer and in a question's text field.
#[test]
fn ctrl_underscore_undoes_in_the_composer_and_the_text_field() {
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let kitty = KeyEvent::new(
        KeyCode::Char('-'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    for undo in [ctrl('_'), ctrl('/'), ctrl('7'), kitty] {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("alpha beta");
        assert_eq!(harness.app.handle_key(ctrl('w')), None);
        assert_eq!(harness.app.composer_text(), "alpha ");
        assert_eq!(harness.app.handle_key(undo), None, "{undo:?}");
        assert_eq!(harness.app.composer_text(), "alpha beta", "{undo:?}");

        let req = PromptRequest::new(PromptAction::Input, "Name:", Origin::detached());
        let asker = ask_on_a_thread(&mut harness, req);
        harness.type_text("gamma delta");
        assert_eq!(harness.app.handle_key(ctrl('w')), None);
        assert_eq!(harness.app.handle_key(undo), None, "{undo:?}");
        let field = harness.app.prompts.front().unwrap().field.as_ref().unwrap();
        assert_eq!(field.lines(), ["gamma delta"], "{undo:?}");
        assert_eq!(harness.app.composer_text(), "alpha beta", "{undo:?}");
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    }
}

/// Alt+D cuts the word after the cursor in the composer, in Vim insert
/// mode, and in a question's text field.
#[test]
fn alt_d_cuts_the_next_word_in_the_composer_vim_insert_and_the_text_field() {
    let alt_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT);
    for (mut harness, mode) in [
        (Harness::standalone(80, 24), None),
        (vi_harness(), Some(ViMode::Insert)),
    ] {
        harness.type_text("alpha beta gamma");
        harness.app.composer.move_cursor(keys::ViMotion::Head);
        assert_eq!(harness.app.handle_key(alt_d), None);
        assert_eq!(harness.app.composer_text(), " beta gamma");
        assert_eq!(harness.app.vi_mode, mode);
    }

    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest::new(PromptAction::Input, "Name:", Origin::detached());
    let asker = ask_on_a_thread(&mut harness, req);
    harness.type_text("alpha beta");
    let field = harness
        .app
        .prompts
        .front_mut()
        .unwrap()
        .field
        .as_mut()
        .unwrap();
    field.move_cursor(ratatui_textarea::CursorMove::Head);
    assert_eq!(harness.app.handle_key(alt_d), None);
    let field = harness.app.prompts.front().unwrap().field.as_ref().unwrap();
    assert_eq!(field.lines(), [" beta"]);
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

fn list_request(action: PromptAction, tool: Option<&str>) -> PromptRequest {
    PromptRequest {
        options: ["alpha", "beta", "gamma"].map(str::to_string).to_vec(),
        tool: tool.map(str::to_string),
        ..PromptRequest::new(action, "Pick one q9d-s", Origin::detached())
    }
}

/// A background job still writing after its turn ended shares one
/// trailing cell per call, bounded in bytes, instead of a cell per
/// pipe read.
#[test]
fn out_of_turn_tool_output_shares_one_bounded_cell() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.app.end_turn(false, &TurnOutcome::default());
    let cells_before = harness.app.transcript.len();
    harness
        .app
        .append_tool_output(Some("bg".to_string()), b"first-q7k\n");
    harness
        .app
        .append_tool_output(Some("bg".to_string()), b"second-q7k\n");
    assert_eq!(harness.app.transcript.len(), cells_before + 1);
    let Some(Cell::Text { lines, .. }) = harness.app.transcript.cells().last() else {
        panic!("the output did not land in a text cell");
    };
    assert_eq!(lines, &["first-q7k", "second-q7k"]);

    let line = format!("{}\n", "z".repeat(LATE_OUTPUT_LIMIT / 4));
    for _ in 0..12 {
        harness
            .app
            .append_tool_output(Some("bg".to_string()), line.as_bytes());
    }
    assert_eq!(harness.app.transcript.len(), cells_before + 1);
    let Some(Cell::Text { lines, .. }) = harness.app.transcript.cells().last() else {
        panic!("the output did not stay in its text cell");
    };
    let total: usize = lines.iter().map(|line| line.len() + 1).sum();
    assert!(total <= LATE_OUTPUT_LIMIT, "{total} bytes kept");
    assert!(
        !lines.iter().any(|line| line.contains("q7k")),
        "the oldest lines were kept"
    );
}

#[test]
fn a_resize_and_back_reflows_without_duplicating_rows() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.app.handle_msg(UiMsg::TextDelta(
        "A first paragraph long enough to wrap when the screen is narrow, which is \
         what a resize does to it.\n\n- one bullet\n- two bullets\n\nA closing line."
            .to_string(),
    ));
    harness.app.handle_msg(UiMsg::StreamDone);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_msg(UiMsg::Notice {
        kind: NoticeKind::Checkpoint,
        text: "checkpoint saved".to_string(),
    });

    let wide = harness.draw();
    insta::assert_snapshot!("resize_80x24", &wide);
    harness.resize(40, 24);
    let narrow = harness.draw();
    insta::assert_snapshot!("resize_40x24", &narrow);
    harness.resize(80, 24);
    let wide_again = harness.draw();

    assert_eq!(wide, wide_again);
    assert_ne!(wide, narrow);
    assert_eq!(duplicated_adjacent_rows(&wide), 0, "{wide}");
    assert_eq!(duplicated_adjacent_rows(&narrow), 0, "{narrow}");
}

#[test]
fn every_widget_survives_a_tiny_screen() {
    for (width, height) in [(1, 1), (0, 0), (3, 2), (10, 3)] {
        let mut harness = Harness::standalone(width, height);
        harness.render();
        harness.app.begin_turn("hi", false);
        narrate(
            &mut harness,
            "call-1",
            "Call fs_read \u{2714}\u{fe0f} {\"path\":\"x\"}",
        );
        start_tool(&mut harness, "call-1", "fs_read", None);
        tool_output(&mut harness, "call-1", "\u{2714}\u{fe0f} done\n");
        harness.render();
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.handle_key(ctrl(KeyCode::Up));
        harness.render();
        harness
            .app
            .handle_msg(UiMsg::TextDelta("reply".to_string()));
        harness
            .app
            .handle_msg(UiMsg::UsageUpdated("12 tokens".to_string()));
        harness.app.handle_msg(UiMsg::TodoUpdated(todo_items()));
        harness.app.handle_key(ctrl(KeyCode::Char('x')));
        harness.render();
        harness.app.handle_key(ctrl(KeyCode::Char('x')));
        harness.press(KeyCode::PageUp);
        harness.app.transcript.push(Cell::user("late"));
        harness.render();
        let _asker = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
        harness.render();
        harness.press(KeyCode::Esc);
    }
}

#[test]
fn the_footer_strips_the_right_prompt_colours() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.handle_msg(UiMsg::UsageUpdated(
        "\x1b[36m[low] \x1b[35m$0.0012 \x1b[0m".to_string(),
    ));
    let rendered = harness.draw();
    assert!(rendered.contains("[low] $0.0012"), "{rendered}");
    assert!(!rendered.contains('\x1b'), "{rendered:?}");
}

#[test]
fn a_pending_prefill_lands_in_the_composer_on_idle_return() {
    let mut ctx = chat_ctx();
    ctx.pending_prefill = Some("edited line".to_string());
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    assert_eq!(harness.app.composer_text(), "edited line");
    assert_eq!(ctx.pending_prefill, None);
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit("edited line".to_string()))
    );
    assert_eq!(harness.app.composer_text(), "");
}

/// The elapsed-time row keeps ticking: the tick is asked for after the
/// scheduler forgets the frame just drawn, so it is not wiped with it.
#[test]
fn the_status_row_ticks_once_a_second_while_a_turn_runs() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("work", false);
    harness.render();
    harness.app.frame_drawn();
    let due = harness.app.frames.due_at().expect("a tick is pending");
    harness.clock.advance(STATUS_TICK);
    assert!(due <= harness.clock.now());
    let rendered = harness.draw();
    assert!(
        rendered.contains("Working . 1s  Esc to interrupt"),
        "{rendered}"
    );

    harness.app.end_turn(false, &TurnOutcome::default());
    harness.render();
    harness.app.frame_drawn();
    assert_eq!(harness.app.frames.due_at(), None);
}

/// An open roster asks for a read every tick, but not while the pager
/// covers it and nothing of it is drawn.
#[test]
fn the_roster_ticks_only_while_no_pager_covers_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.render();
    harness.app.frame_drawn();
    assert!(harness.app.frames.due_at().is_some(), "no roster tick");

    harness.app.pager = Some(Pager::root());
    harness.render();
    harness.app.frame_drawn();
    assert_eq!(harness.app.frames.due_at(), None);
}

/// Ctrl+G is bound to nothing: no roster, and the draft stays as typed.
#[test]
fn ctrl_g_neither_opens_the_roster_nor_types() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    harness.app.handle_key(ctrl(KeyCode::Char('g')));
    assert!(!matches!(harness.app.view, View::Roster(_)));
    assert_eq!(harness.app.composer_text(), "draft");
}

#[test]
fn the_status_row_names_thinking_and_the_running_tool() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("work", false);
    harness.app.handle_msg(UiMsg::ThinkingStarted);
    harness.clock.advance(Duration::from_secs(3));
    let rendered = harness.draw();
    assert!(rendered.contains("Thinking for 3s"), "{rendered}");

    harness
        .app
        .handle_msg(UiMsg::ToolRunning(Some("fs_write".to_string())));
    let rendered = harness.draw();
    assert!(rendered.contains("Running fs_write"), "{rendered}");
    harness.app.handle_msg(UiMsg::ToolRunning(None));
    harness.app.handle_msg(UiMsg::ThinkingStopped);
    let rendered = harness.draw();
    assert!(rendered.contains("Working . 3s"), "{rendered}");
}

/// Once the teardown task owns the context the keys that would start
/// something new (a line, the editor, an interrupt, another quit) are
/// inert, typing still works, and the strip says what is happening.
#[test]
fn teardown_keys_do_nothing_new_and_the_strip_says_leaving() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    harness.app.begin_exit();
    assert!(harness.app.key_context().turn_running);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(harness.press(KeyCode::Esc), None);
    for c in ['o', 'd', 'c'] {
        assert_eq!(
            harness
                .app
                .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
            None,
            "Ctrl+{c} during teardown"
        );
    }
    harness.type_text("!");
    assert_eq!(harness.app.composer_text(), "draft!");
    let rendered = harness.draw();
    assert!(rendered.contains("Leaving..."), "{rendered}");
}

/// A child agent's tool hooks leave the root's status row alone.
#[test]
fn only_root_tool_hooks_drive_the_status_row() {
    use crate::hooks::HookEvent;
    let (tx, mut rx) = unbounded_channel();
    let sink = TuiSink::new(tx);
    let hook = |origin: Origin| Event::Hook {
        event: HookEvent::ToolStarted,
        envs: vec![("COYOTE_TOOL_NAME".to_string(), "fs_write".to_string())],
        payload: None,
        origin,
    };
    sink.on_event(&hook(Origin {
        agent_id: Some("agent_1".to_string()),
        depth: 1,
    }));
    sink.on_event(&hook(Origin {
        agent_id: None,
        depth: 1,
    }));
    assert!(rx.try_recv().is_err());
    sink.on_event(&hook(Origin::detached()));
    assert!(matches!(
        rx.try_recv(),
        Ok(UiMsg::ToolRunning(Some(tool))) if tool == "fs_write"
    ));
}

/// A child agent's tool output stays out of the root transcript, as
/// the REPL keeps it off the terminal; the root's own still lands.
#[test]
fn only_root_tool_output_reaches_the_transcript() {
    use crate::events::OutputStream;
    use crate::turn::UiSink;
    let (tx, mut rx) = unbounded_channel();
    let sink = TuiSink::new(tx);
    sink.tool_output(Some("child-c3t"), OutputStream::Stdout, 1, b"child line\n");
    sink.tool_output(None, OutputStream::Stderr, 2, b"grandchild line\n");
    assert!(rx.try_recv().is_err(), "a child's output reached the UI");
    sink.tool_output(Some("root-c3t"), OutputStream::Stdout, 0, b"root line\n");
    assert!(matches!(
        rx.try_recv(),
        Ok(UiMsg::ToolOutput { call_id: Some(id), bytes })
            if id == "root-c3t" && bytes == b"root line\n"
    ));
}

/// Thinking events reach the status row through the sink, the same
/// way the tool hooks do.
#[test]
fn thinking_events_pass_through_the_sink() {
    let (tx, mut rx) = unbounded_channel();
    let sink = TuiSink::new(tx);
    sink.on_event(&Event::ThinkingStarted {
        origin: Origin::detached(),
    });
    assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStarted)));
    sink.on_event(&Event::ThinkingStopped {
        origin: Origin::detached(),
    });
    assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStopped)));
    assert!(rx.try_recv().is_err());
}

/// A child agent's reasoning says nothing about what the root is
/// doing, so only root-origin thinking events reach the status row.
#[test]
fn a_child_agents_thinking_never_drives_the_status_row() {
    let (tx, mut rx) = unbounded_channel();
    let sink = TuiSink::new(tx);
    let child = Origin {
        agent_id: Some("agent_c".into()),
        depth: 1,
    };
    sink.on_event(&Event::ThinkingStarted {
        origin: child.clone(),
    });
    sink.on_event(&Event::ThinkingStopped { origin: child });
    assert!(rx.try_recv().is_err(), "a child's thinking reached the UI");
    sink.on_event(&Event::ThinkingStarted {
        origin: Origin::detached(),
    });
    assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStarted)));
}

/// Emitters paint notices for the REPL (`error_text`, a leading
/// newline for spacing); the sink strips that so the cell shows the
/// words on its first row and no escape bytes.
#[test]
fn a_painted_notice_reaches_the_frame_as_plain_words() {
    use crate::turn::UiSink;
    let (tx, mut rx) = unbounded_channel();
    let sink = TuiSink::new(tx);
    sink.notice(
        NoticeKind::Interrupted,
        "\n\x1b[31mResponse interrupted\x1b[0m",
    );
    let mut harness = Harness::standalone(40, 10);
    harness.app.handle_msg(rx.try_recv().unwrap());
    let frame = harness.draw();
    let first_row = frame.lines().next().unwrap();
    assert!(first_row.contains("Response interrupted"), "{frame}");
    assert!(!frame.contains("[31m"), "{frame}");
}

/// Cells drawn as plain lines are scrubbed at the draw whatever path
/// fed them: a user line, an error and a notice pushed straight into
/// the transcript show their words and no escape or format bytes.
#[test]
fn plain_cells_are_scrubbed_when_drawn() {
    let mut harness = Harness::standalone(60, 12);
    harness.app.begin_turn("hi\x1b[2Jthere\u{200b}u7p", false);
    harness.app.transcript.push(Cell::Error {
        text: "\x1b[31mbroken\x1b[0m e7p".to_string(),
        cache: LineCache::default(),
    });
    harness.app.transcript.push(Cell::notice(
        NoticeKind::Routing,
        "\x1b]8;;x\x07note\x1b]8;;\x07 n7p".to_string(),
    ));
    let frame = harness.draw();
    for word in ["> hithereu7p", "broken e7p", "note n7p"] {
        assert!(frame.contains(word), "{word:?} missing:\n{frame}");
    }
    assert!(
        !frame.contains('\x1b') && !frame.contains('\u{200b}'),
        "{frame}"
    );
}

/// The final text of a reply is authoritative: it replaces whatever
/// the stream had accumulated and closes the cell.
#[test]
fn assistant_text_replaces_an_open_stream_cell() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hi", false);
    harness
        .app
        .handle_msg(UiMsg::TextDelta("partial".to_string()));
    harness
        .app
        .handle_msg(UiMsg::AssistantText("the whole reply".to_string()));
    assert_eq!(harness.app.transcript.len(), 2);
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Assistant { source, done: true, .. } if source == "the whole reply"
    ));
    harness
        .app
        .handle_msg(UiMsg::AssistantText("a second reply".to_string()));
    assert_eq!(harness.app.transcript.len(), 3);
}

/// A cell pushed while the reply streams (a background job's output
/// here) lands after the reply, and the next delta still extends the
/// reply rather than opening a second one.
#[test]
fn a_stream_survives_a_cell_pushed_between_deltas() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hi", false);
    harness
        .app
        .handle_msg(UiMsg::TextDelta("Hello".to_string()));
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"job noise\n".to_vec(),
    });
    harness
        .app
        .handle_msg(UiMsg::TextDelta(" world".to_string()));
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 3);
    assert!(matches!(
        &cells[1],
        Cell::Assistant { source, done: false, .. } if source == "Hello world"
    ));
    assert!(matches!(
        &cells[2],
        Cell::Text { call_id: None, lines, .. } if lines == &["job noise"]
    ));
    harness.app.handle_msg(UiMsg::StreamDone);
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Assistant { done: true, .. }
    ));
    harness.app.handle_msg(UiMsg::TextDelta("next".to_string()));
    assert_eq!(harness.app.transcript.len(), 4);
}

/// Undoing past the streaming cell forgets it; a later delta opens a
/// fresh cell instead of writing into whatever took its index.
#[test]
fn truncating_past_the_stream_cell_forgets_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hi", false);
    harness
        .app
        .handle_msg(UiMsg::TextDelta("partial".to_string()));
    harness.app.transcript.truncate(1);
    harness.app.transcript.push(Cell::notice(
        NoticeKind::ToolWarning,
        "took the slot".to_string(),
    ));
    harness
        .app
        .handle_msg(UiMsg::TextDelta("fresh".to_string()));
    let cells = harness.app.transcript.cells();
    assert!(matches!(&cells[1], Cell::Notice { .. }));
    assert!(matches!(
        &cells[2],
        Cell::Assistant { source, done: false, .. } if source == "fresh"
    ));
}

#[test]
fn chrome_colours_are_dropped_under_no_color_but_attributes_stay() {
    for kind in [
        ChromeKind::Notice(NoticeKind::ToolWarning),
        ChromeKind::Error,
    ] {
        assert!(chrome_style(kind, false).fg.is_some(), "{kind:?}");
        assert_eq!(chrome_style(kind, true).fg, None, "{kind:?}");
    }
    let dim = ChromeKind::Notice(NoticeKind::Checkpoint);
    for no_color in [false, true] {
        let style = chrome_style(dim, no_color);
        assert_eq!(style.fg, None);
        assert!(style.add_modifier.contains(Modifier::DIM));
    }
}

/// The transcript's cells take the colour decision from the same
/// switch the composer does, so one flip covers the whole frame.
#[test]
fn a_drawn_error_cell_follows_the_no_color_switch() {
    for (no_color, fg) in [(false, Color::Red), (true, Color::Reset)] {
        let mut harness = Harness::standalone(60, 12);
        harness.app.set_no_color(no_color);
        harness.app.transcript.push(Cell::Error {
            text: "broken q8c".to_string(),
            cache: LineCache::default(),
        });
        let frame = harness.draw();
        let row = screen_row(&harness, "broken q8c");
        let buffer = harness.terminal.backend().buffer();
        assert_eq!(buffer[(0, row)].fg, fg, "{no_color}: {frame}");
    }
}

/// What a prompt turn printed around the modal becomes a notice with
/// its escapes removed; the spinner's glyph is text and stays.
#[test]
fn captured_output_becomes_a_clean_notice() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("ask", false);
    harness.app.end_turn(
        false,
        &TurnOutcome {
            captured: "\x1b[0G\u{280b}\x1b[?25l Thinking\x1b[1m!\x1b[0m\n".to_string(),
            ..TurnOutcome::default()
        },
    );
    assert!(matches!(
        harness.app.transcript.cells().last(),
        Some(Cell::Notice { text, .. }) if text == "\u{280b} Thinking!"
    ));
}

/// Anonymous tool output continues the cell it is streaming into, but
/// never one left over from an earlier turn.
#[test]
fn anonymous_tool_output_never_joins_an_older_turn() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn(".run", true);
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"first".to_vec(),
    });
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b" turn\n".to_vec(),
    });
    assert_eq!(harness.app.transcript.len(), 2);
    harness.app.end_turn(true, &TurnOutcome::default());
    let cells_after_first = harness.app.transcript.len();

    harness.app.begin_turn(".run", true);
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"second turn\n".to_vec(),
    });
    assert_eq!(harness.app.transcript.len(), cells_after_first + 2);
    assert!(matches!(
        harness.app.transcript.cells().last(),
        Some(Cell::Text { call_id: None, lines, .. }) if lines == &["second turn"]
    ));
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Text { lines, .. } if lines == &["first turn"]
    ));
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

fn narrate(harness: &mut Harness, call_id: &str, line: &str) {
    harness.app.handle_msg(UiMsg::ToolNarration {
        call_id: call_id.to_string(),
        line: line.to_string(),
    });
}

/// Sends what the sink sends for a tool's start hook: the status
/// row's name first, then the row's own message.
fn start_tool(harness: &mut Harness, call_id: &str, name: &str, args: Option<Value>) {
    harness
        .app
        .handle_msg(UiMsg::ToolRunning(Some(name.to_string())));
    harness.app.handle_msg(UiMsg::ToolStarted {
        call_id: call_id.to_string(),
        name: name.to_string(),
        args,
    });
}

fn tool_output(harness: &mut Harness, call_id: &str, bytes: &str) {
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: Some(call_id.to_string()),
        bytes: bytes.as_bytes().to_vec(),
    });
}

fn finish_tool(harness: &mut Harness, call_id: &str, ok: bool, error: Option<&str>) {
    harness.app.handle_msg(UiMsg::ToolRunning(None));
    harness.app.handle_msg(UiMsg::ToolFinished {
        call_id: call_id.to_string(),
        ok,
        error: error.map(str::to_string),
        duration_ms: Some(1200),
    });
}

/// A turn with one `fs_read` call under way: narrated, started and
/// two chunks of output in.
fn running_fs_read(harness: &mut Harness) {
    harness.app.begin_turn("read it", false);
    narrate(harness, "call-1", "Call fs_read {\"path\":\"src/main.rs\"}");
    start_tool(harness, "call-1", "fs_read", None);
    tool_output(harness, "call-1", "fn main() {\n");
    tool_output(harness, "call-1", "    println!(\"hi\");\n}\n");
}

fn todo_items() -> Vec<crate::config::todo::TodoItem> {
    use crate::config::todo::TodoItem;
    vec![
        TodoItem {
            id: 1,
            desc: "wire the panel".to_string(),
            done: true,
        },
        TodoItem {
            id: 2,
            desc: "snapshot it narrow".to_string(),
            done: false,
        },
    ]
}

/// The first screen row of the last frame that starts with `prefix`.
fn screen_row(harness: &Harness, prefix: &str) -> u16 {
    let buffer = harness.terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let text: String = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        if text.starts_with(prefix) {
            return y;
        }
    }
    panic!("no row starts with {prefix:?}:\n{buffer:?}");
}

/// The modifiers on the first cell of the first screen row that
/// starts with `prefix`.
fn row_modifier(harness: &Harness, prefix: &str) -> Modifier {
    let row = screen_row(harness, prefix);
    harness.terminal.backend().buffer()[(0, row)].modifier
}

#[test]
fn a_running_tool_row_shows_its_output_tail() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    let frame = harness.draw();
    insta::assert_snapshot!("tool_running_80x24", &frame);
    assert!(frame.contains("fs_read({\"path\":\"src/main.rs\"})  running"));
    assert!(
        frame.contains("Call fs_read {\"path\":\"src/main.rs\"}"),
        "{frame}"
    );
    assert!(frame.contains("println!(\"hi\");"));
}

/// The start hook's compact JSON and the narration's text after the
/// name are the same arguments, so the header reads the same from
/// either.
#[test]
fn the_hooks_arguments_render_the_same_header_as_the_narration() {
    let mut from_narration = Harness::standalone(80, 24);
    running_fs_read(&mut from_narration);
    let mut from_hook = Harness::standalone(80, 24);
    from_hook.app.begin_turn("read it", false);
    narrate(
        &mut from_hook,
        "call-1",
        "Call fs_read {\"path\":\"src/main.rs\"}",
    );
    start_tool(
        &mut from_hook,
        "call-1",
        "fs_read",
        Some(serde_json::json!({"path": "src/main.rs"})),
    );
    tool_output(&mut from_hook, "call-1", "fn main() {\n");
    tool_output(&mut from_hook, "call-1", "    println!(\"hi\");\n}\n");
    let header = |harness: &mut Harness| {
        let frame = harness.draw();
        let row = screen_row(harness, "fs_read(");
        frame.lines().nth(usize::from(row)).unwrap().to_string()
    };
    let expected = header(&mut from_narration);
    assert!(expected.contains("fs_read({\"path\":\"src/main.rs\"})  running"));
    assert_eq!(header(&mut from_hook), expected);
}

#[test]
fn a_finished_tool_row_collapses_and_opens_from_the_keyboard() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    let frame = harness.draw();
    insta::assert_snapshot!("tool_collapsed_ok_80x24", &frame);
    assert!(frame.contains("1.2s  ok"), "{frame}");
    assert!(!frame.contains("fn main()"), "{frame}");
    assert!(!frame.contains("Call fs_read"), "{frame}");

    assert!(harness.app.handle_key(ctrl(KeyCode::Up)).is_none());
    assert!(harness.app.transcript.focused());
    assert!(harness.press(KeyCode::Enter).is_none());
    let frame = harness.draw();
    insta::assert_snapshot!("tool_expanded_80x24", &frame);
    assert!(frame.contains("fn main()"), "{frame}");
    assert!(
        frame.contains("Call fs_read {\"path\":\"src/main.rs\"}"),
        "{frame}"
    );
    assert!(
        row_modifier(&harness, "fs_read(").contains(Modifier::REVERSED),
        "the focused header is not reversed"
    );
}

#[test]
fn a_failed_tool_row_shows_the_first_error_line_until_opened() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    finish_tool(
        &mut harness,
        "call-1",
        false,
        Some("exit 2\nno such file: src/main.rs"),
    );
    harness.app.end_turn(false, &TurnOutcome::default());
    let frame = harness.draw();
    insta::assert_snapshot!("tool_failed_80x24", &frame);
    assert!(frame.contains("1.2s  failed: exit 2"), "{frame}");
    assert!(!frame.contains("no such file"), "{frame}");

    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.press(KeyCode::Enter);
    let frame = harness.draw();
    assert!(frame.contains("no such file: src/main.rs"), "{frame}");
    assert!(frame.contains("fn main()"), "{frame}");
}

/// A folded header wider than the row gives up arguments, not its
/// state: at 40 columns a 48-character argument string is cut to
/// what leaves `failed: exit 2` on screen.
#[test]
fn a_narrow_folded_header_keeps_its_state_and_cuts_the_arguments() {
    let mut harness = Harness::standalone(40, 20);
    harness.app.begin_turn("read it", false);
    let path = format!("src/{}.rs", "x".repeat(30));
    let args = serde_json::json!({ "path": path }).to_string();
    assert_eq!(args.chars().count(), 48);
    narrate(&mut harness, "call-1", &format!("Call fs_read {args}"));
    start_tool(&mut harness, "call-1", "fs_read", None);
    finish_tool(
        &mut harness,
        "call-1",
        false,
        Some("exit 2\nno such file: src/main.rs"),
    );
    harness.app.end_turn(false, &TurnOutcome::default());
    let frame = harness.draw();
    insta::assert_snapshot!("tool_failed_40x20", &frame);
    assert!(frame.contains("1.2s  failed: exit 2"), "{frame}");
    assert!(frame.contains("fs_read({\"path...)  1.2s"), "{frame}");
}

#[test]
fn a_job_takes_its_start_calls_row_and_folds_the_checks_in() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("build it", false);
    narrate(
        &mut harness,
        "call-1",
        "Call job__start {\"tool\":\"execute_command\"}",
    );
    start_tool(&mut harness, "call-1", "job__start", None);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    tool_output(&mut harness, "call-1", "started\n");
    finish_tool(&mut harness, "call-1", true, None);
    narrate(
        &mut harness,
        "call-2",
        "Call job__check {\"id\":\"job_1a2b3c4d\"}",
    );
    start_tool(
        &mut harness,
        "call-2",
        "job__check",
        Some(serde_json::json!({"id": "job_1a2b3c4d"})),
    );
    tool_output(&mut harness, "call-2", "still running\n");
    finish_tool(&mut harness, "call-2", true, None);
    harness.app.handle_msg(UiMsg::JobFinished {
        job_id: "job_1a2b3c4d".to_string(),
        ok: true,
        error: None,
    });
    harness.app.end_turn(false, &TurnOutcome::default());

    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 2, "one user cell and one job row");
    let Cell::Job { status, calls, .. } = &cells[1] else {
        panic!("the job did not take the start call's row");
    };
    assert_eq!(*status, JobStatus::Done);
    let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
    assert_eq!(names, ["job__start", "job__check"]);
    assert_eq!(calls[0].output, ["started"]);
    assert_eq!(calls[1].output, ["still running"]);

    let frame = harness.draw();
    insta::assert_snapshot!("job_row_80x24", &frame);
    assert!(
        frame.contains("job execute_command job_1a2b3c4d  done  started, checked"),
        "{frame}"
    );
    assert!(!frame.contains("still running"), "{frame}");
    harness.app.handle_key(ctrl(KeyCode::Down));
    harness.press(KeyCode::Enter);
    let frame = harness.draw();
    assert!(frame.contains("still running"), "{frame}");
}

/// A check on a job from a later turn folds into the job's row, and
/// its output goes with it rather than into a loose text cell.
#[test]
fn a_later_turns_check_on_a_job_lands_on_the_jobs_row() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("build it", false);
    narrate(
        &mut harness,
        "call-1",
        "Call job__start {\"tool\":\"execute_command\"}",
    );
    start_tool(&mut harness, "call-1", "job__start", None);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    finish_tool(&mut harness, "call-1", true, None);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("Started it.".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    assert_eq!(harness.app.transcript.len(), 3);

    harness.app.begin_turn("how is it going", false);
    narrate(
        &mut harness,
        "call-2",
        "Call job__check {\"id\":\"job_1a2b3c4d\"}",
    );
    start_tool(
        &mut harness,
        "call-2",
        "job__check",
        Some(serde_json::json!({"id": "job_1a2b3c4d"})),
    );
    tool_output(&mut harness, "call-2", "still running\n");
    finish_tool(&mut harness, "call-2", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());

    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 4, "the check added only its user line");
    let Cell::Job { calls, .. } = &cells[1] else {
        panic!("the job row moved");
    };
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].name, "job__check");
    assert_eq!(calls[1].output, ["still running"]);
    assert!(matches!(&cells[3], Cell::User { .. }));
}

#[test]
fn an_agent_takes_its_spawn_calls_row_and_folds_the_collect_in() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("delegate it", false);
    narrate(
        &mut harness,
        "call-1",
        "Call agent__spawn {\"agent\":\"coder\"}",
    );
    start_tool(&mut harness, "call-1", "agent__spawn", None);
    harness.app.handle_msg(UiMsg::AgentStarted {
        agent_id: "agent_coder_1a2b3c4d".to_string(),
        name: "coder".to_string(),
    });
    finish_tool(&mut harness, "call-1", true, None);
    narrate(
        &mut harness,
        "call-2",
        "Call agent__collect {\"id\":\"agent_coder_1a2b3c4d\"}",
    );
    start_tool(
        &mut harness,
        "call-2",
        "agent__collect",
        Some(serde_json::json!({"id": "agent_coder_1a2b3c4d"})),
    );
    finish_tool(&mut harness, "call-2", true, None);
    harness.app.handle_msg(UiMsg::AgentFinished {
        agent_id: "agent_coder_1a2b3c4d".to_string(),
        outcome: AgentOutcome::Completed,
    });
    harness.app.end_turn(false, &TurnOutcome::default());

    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 2, "one user cell and one agent row");
    let Cell::Agent { status, calls, .. } = &cells[1] else {
        panic!("the agent did not take the spawn call's row");
    };
    assert_eq!(*status, AgentStatus::Completed);
    let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
    assert_eq!(names, ["agent__spawn", "agent__collect"]);

    let frame = harness.draw();
    insta::assert_snapshot!("agent_row_80x24", &frame);
    assert!(
        frame.contains("agent coder agent_coder_1a2b3c4d  completed  spawned, collected"),
        "{frame}"
    );
}

#[test]
fn a_bookkeeping_call_is_one_dim_line_that_never_takes_the_focus() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("remember", false);
    narrate(
        &mut harness,
        "call-1",
        "Call memory__search {\"query\":\"tui\"}",
    );
    start_tool(&mut harness, "call-1", "memory__search", None);
    tool_output(&mut harness, "call-1", "3 hits\n");
    let frame = harness.draw();
    assert!(
        !frame.contains("3 hits"),
        "a running bookkeeping call showed output:\n{frame}"
    );
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    let frame = harness.draw();
    insta::assert_snapshot!("bookkeeping_line_80x24", &frame);
    assert!(
        frame.contains("memory__search({\"query\":\"tui\"})  1.2s  ok"),
        "{frame}"
    );
    assert!(!frame.contains("3 hits"), "{frame}");

    harness.app.handle_key(ctrl(KeyCode::Up));
    assert!(!harness.app.transcript.focused());
    harness.press(KeyCode::Enter);
    let frame = harness.draw();
    assert!(!frame.contains("3 hits"), "{frame}");
}

#[test]
fn a_todo_call_leaves_no_row_in_the_transcript() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("plan it", false);
    let before = harness.app.transcript.len();
    narrate(&mut harness, "call-1", "Call todo__list {}");
    start_tool(&mut harness, "call-1", "todo__list", None);
    finish_tool(&mut harness, "call-1", true, None);
    assert_eq!(harness.app.transcript.len(), before);
    assert!(!harness.draw().contains("todo__list"));
}

#[test]
fn tool_output_routes_by_call_id_past_newer_rows() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("two reads", false);
    narrate(&mut harness, "call-1", "Call fs_read {\"path\":\"a\"}");
    start_tool(&mut harness, "call-1", "fs_read", None);
    narrate(&mut harness, "call-2", "Call fs_read {\"path\":\"b\"}");
    start_tool(&mut harness, "call-2", "fs_read", None);
    tool_output(&mut harness, "call-1", "from a\n");
    tool_output(&mut harness, "call-2", "from b\n");
    let cells = harness.app.transcript.cells();
    assert!(matches!(
        &cells[1],
        Cell::Tool { call_id, output, .. } if call_id == "call-1" && output == &["from a"]
    ));
    assert!(matches!(
        &cells[2],
        Cell::Tool { call_id, output, .. } if call_id == "call-2" && output == &["from b"]
    ));
}

/// A multi-byte character the pipe split across two reads renders
/// whole; a lone byte no sequence can hold still shows as U+FFFD.
#[test]
fn tool_output_split_mid_character_decodes_whole() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("accents", false);
    narrate(
        &mut harness,
        "call-1",
        "Call execute_command {\"command\":\"menu\"}",
    );
    start_tool(&mut harness, "call-1", "execute_command", None);
    for chunk in [
        &b"caf"[..],
        &[0xC3][..],
        &[0xA9, b'\n'][..],
        &[0xFF, b'\n'][..],
    ] {
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: Some("call-1".to_string()),
            bytes: chunk.to_vec(),
        });
    }
    let Some(Cell::Tool {
        output, pending, ..
    }) = harness.app.transcript.cells().get(1)
    else {
        panic!("the tool row is gone");
    };
    assert_eq!(output, &["caf\u{e9}", "\u{FFFD}"]);
    assert!(pending.is_empty());
}

/// A model that names no call id leaves the hooks with an empty one;
/// it pairs with the newest running row that has none, and once that
/// row finishes the next empty id finds the next row.
#[test]
fn an_empty_call_id_pairs_with_the_newest_running_anonymous_row() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("anonymous", false);
    narrate(&mut harness, "", "Call fs_read {\"path\":\"a\"}");
    start_tool(&mut harness, "", "fs_read", None);
    narrate(&mut harness, "", "Call fs_write {\"path\":\"b\"}");
    start_tool(&mut harness, "", "fs_write", None);
    finish_tool(&mut harness, "", false, Some("denied"));
    finish_tool(&mut harness, "", true, None);
    let cells = harness.app.transcript.cells();
    assert!(matches!(
        &cells[1],
        Cell::Tool { name, status: ToolStatus::Ok { .. }, .. } if name == "fs_read"
    ));
    assert!(matches!(
        &cells[2],
        Cell::Tool { name, status: ToolStatus::Failed { error, .. }, .. }
            if name == "fs_write" && error == "denied"
    ));
}

/// The output pump of a call without an id reports `None` rather
/// than an empty id; the bytes still reach the newest running row
/// without one, and a row that has finished takes no more of them.
#[test]
fn anonymous_tool_output_lands_in_the_newest_running_anonymous_row() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("anonymous", false);
    narrate(&mut harness, "", "Call fs_read {\"path\":\"a\"}");
    start_tool(&mut harness, "", "fs_read", None);
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"from read\n".to_vec(),
    });
    narrate(&mut harness, "", "Call fs_write {\"path\":\"b\"}");
    start_tool(&mut harness, "", "fs_write", None);
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"from write\n".to_vec(),
    });
    finish_tool(&mut harness, "", true, None);
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"late read\n".to_vec(),
    });
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 3, "the output took no cell of its own");
    assert!(matches!(
        &cells[1],
        Cell::Tool { name, output, status: ToolStatus::Running { .. }, .. }
            if name == "fs_read" && output == &["from read", "late read"]
    ));
    assert!(matches!(
        &cells[2],
        Cell::Tool { name, output, status: ToolStatus::Ok { .. }, .. }
            if name == "fs_write" && output == &["from write"]
    ));
}

/// Once a call without an id has folded into the job row it started,
/// its output follows it there.
#[test]
fn anonymous_tool_output_follows_its_call_into_a_job_row() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("background", false);
    narrate(
        &mut harness,
        "",
        "Call job__start {\"tool\":\"execute_command\"}",
    );
    start_tool(&mut harness, "", "job__start", None);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"started\n".to_vec(),
    });
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 2, "the output took no cell of its own");
    let Cell::Job { calls, .. } = &cells[1] else {
        panic!("the job did not take the start call's row");
    };
    assert_eq!(calls[0].output, ["started"]);
}

/// Output without an id that arrives after the job has finished is
/// not the job's: it takes a cell of its own rather than reopening
/// the finished row.
#[test]
fn anonymous_tool_output_never_joins_a_finished_job_row() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("background", false);
    narrate(
        &mut harness,
        "",
        "Call job__start {\"tool\":\"execute_command\"}",
    );
    start_tool(&mut harness, "", "job__start", None);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    harness.app.handle_msg(UiMsg::JobFinished {
        job_id: "job_1a2b3c4d".to_string(),
        ok: true,
        error: None,
    });
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: None,
        bytes: b"late\n".to_vec(),
    });
    let cells = harness.app.transcript.cells();
    let Cell::Job { calls, .. } = &cells[1] else {
        panic!("the job did not take the start call's row");
    };
    assert!(calls[0].output.is_empty(), "{:?}", calls[0].output);
    let Some(Cell::Text {
        call_id: None,
        lines,
        ..
    }) = cells.last()
    else {
        panic!("the late output took no cell of its own");
    };
    assert_eq!(lines, &["late"]);
}

#[test]
fn an_end_for_an_unknown_job_or_agent_changes_nothing() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("background", false);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    harness.app.handle_msg(UiMsg::AgentStarted {
        agent_id: "agent_coder_1a2b3c4d".to_string(),
        name: "coder".to_string(),
    });
    harness.app.handle_msg(UiMsg::JobFinished {
        job_id: "job_other".to_string(),
        ok: false,
        error: Some("boom".to_string()),
    });
    harness.app.handle_msg(UiMsg::AgentFinished {
        agent_id: "agent_other".to_string(),
        outcome: AgentOutcome::Failed("boom".to_string()),
    });
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 3);
    assert!(matches!(
        &cells[1],
        Cell::Job {
            status: JobStatus::Running,
            ..
        }
    ));
    assert!(matches!(
        &cells[2],
        Cell::Agent {
            status: AgentStatus::Running,
            ..
        }
    ));

    harness.app.handle_msg(UiMsg::JobCancelled {
        job_id: "job_1a2b3c4d".to_string(),
    });
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Job {
            status: JobStatus::Cancelled,
            ..
        }
    ));
}

/// One exchange plus a tool call, for the undo cases.
fn exchange(harness: &mut Harness) {
    harness.app.begin_turn("first question", false);
    narrate(harness, "call-1", "Call fs_read {\"path\":\"a\"}");
    finish_tool(harness, "call-1", true, None);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("first answer".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
}

/// A turn's worth of filler, enough to overflow the 80x24 view.
fn fill_the_view(harness: &mut Harness) {
    harness.app.begin_turn("fill", false);
    for i in 0..20 {
        harness
            .app
            .transcript
            .push(Cell::user(&format!("filler line {i}")));
    }
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
}

#[test]
fn cells_arriving_below_a_scrolled_view_are_counted_on_a_chip() {
    let mut harness = Harness::standalone(80, 24);
    fill_the_view(&mut harness);
    assert!(harness.press(KeyCode::PageUp).is_none());
    assert!(harness.app.transcript.scrolled());
    let held = harness.draw();
    for i in 0..3 {
        harness
            .app
            .transcript
            .push(Cell::user(&format!("late line {i}")));
    }
    let frame = harness.draw();
    insta::assert_snapshot!("follow_chip_80x24", &frame);
    let chip = "3 new below  End = follow";
    let buffer = harness.terminal.backend().buffer();
    let row_text = |y: u16| -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    };
    let row = (0..buffer.area.height)
        .find(|y| row_text(*y).ends_with(chip))
        .unwrap_or_else(|| panic!("no right-aligned chip:\n{frame}"));
    let column = buffer.area.width - u16::try_from(chip.len()).unwrap();
    let modifier = |x: u16| buffer[(x, row)].modifier;
    assert!(modifier(column).contains(Modifier::REVERSED), "{frame}");
    assert!(
        !modifier(column - 1).contains(Modifier::REVERSED),
        "{frame}"
    );
    assert!(!frame.contains("late line 2"), "the view moved:\n{frame}");
    // The chip sits on the transcript's last row; everything above it
    // must be the held view, unchanged.
    let above_chip = usize::from(row);
    assert_eq!(
        frame.lines().take(above_chip).collect::<Vec<_>>(),
        held.lines().take(above_chip).collect::<Vec<_>>(),
        "the view slid under the new cells:\n{frame}"
    );

    assert!(harness.press(KeyCode::End).is_none());
    assert!(!harness.app.transcript.scrolled());
    let frame = harness.draw();
    assert!(!frame.contains("new below"), "{frame}");
    assert!(!frame.contains("End = follow"), "{frame}");
    assert!(frame.contains("late line 2"), "{frame}");
}

/// Scrolled up with nothing new yet, the chip already offers the way
/// back, so a held view is never mistaken for a following one.
#[test]
fn a_scrolled_view_offers_the_way_back_before_anything_new_arrives() {
    let mut harness = Harness::standalone(80, 24);
    fill_the_view(&mut harness);
    harness.press(KeyCode::PageUp);
    let frame = harness.draw();
    insta::assert_snapshot!("follow_chip_idle_80x24", &frame);
    assert!(frame.contains("End = follow"), "{frame}");
    assert!(!frame.contains("new below"), "{frame}");
    harness.press(KeyCode::End);
    assert!(!harness.draw().contains("End = follow"));
}

/// Shift+G is the keyboard's other way back to the bottom, but only
/// while the composer is empty: with a draft the letter is typing,
/// and the view stays where it is.
#[test]
fn shift_g_follows_the_bottom_only_from_an_empty_composer() {
    let mut harness = Harness::standalone(80, 24);
    fill_the_view(&mut harness);
    let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);

    harness.press(KeyCode::PageUp);
    harness.type_text("draft");
    assert!(harness.app.handle_key(shift_g).is_none());
    assert!(harness.app.transcript.scrolled());
    assert_eq!(harness.app.composer.lines(), ["draftG"]);
    assert!(harness.draw().contains("End = follow"));

    harness.app.handle_key(ctrl(KeyCode::Char('c')));
    assert!(harness.app.composer.is_empty());
    assert!(harness.app.handle_key(shift_g).is_none());
    assert!(!harness.app.transcript.scrolled());
    assert!(harness.app.composer.is_empty());
    assert!(!harness.draw().contains("End = follow"));
}

/// Every custom widget draws only inside the buffer it is given, so
/// a layout handing one an area the frame does not cover (a resize
/// mid-draw, a zero-height pane) cannot panic or write out of range.
#[test]
fn every_custom_widget_clamps_its_area_to_the_buffer() {
    type Draw<'a> = Box<dyn FnMut(Rect, &mut Buffer) + 'a>;
    let mut welcome = Harness::standalone(80, 24);
    let mut plain = Harness::standalone(80, 24);
    exchange(&mut plain);
    let mut chip = Harness::standalone(80, 24);
    fill_the_view(&mut chip);
    chip.app.transcript.scroll_lines(-5);
    let mut todo = TodoPanel::default();
    todo.set("ship it".to_string(), todo_items());
    let (reply, _answers) = mpsc::channel();
    let confirm = PendingPrompt::new(confirm_request(Some("fs_write")), reply.clone());
    let input = PendingPrompt::new(
        PromptRequest::new(PromptAction::Input, "Name?", Origin::detached()),
        reply.clone(),
    );
    let select = PendingPrompt::new(
        list_request(PromptAction::Select, Some("ask")),
        reply.clone(),
    );
    let checkbox = PendingPrompt::new(list_request(PromptAction::Checkbox, None), reply);
    let dot = crate::config::CommandPrefix::Dot;
    let palette = PaletteState::build(".in", dot, PaletteSnapshot::EMPTY, &[], None)
        .expect("`.in` lists commands");
    let mut history = ComposerHistory::default();
    history.push("one");
    let search = SearchState::new(&history);
    let (rewind, _) = RewindState::from_cells(plain.app.transcript.cells(), dot);
    let rewind = rewind.expect("the exchange is a rewind row");
    let queued = vec![QueuedLine {
        text: "later".to_string(),
        command: false,
    }];
    let mut widgets: Vec<(&str, Draw<'_>)> = vec![
        (
            "transcript (welcome)",
            Box::new(|area, buf| {
                welcome
                    .app
                    .transcript
                    .view(area, &mut welcome.app.renderer)
                    .render(area, buf);
            }),
        ),
        (
            "transcript",
            Box::new(|area, buf| {
                plain
                    .app
                    .transcript
                    .view(area, &mut plain.app.renderer)
                    .render(area, buf);
            }),
        ),
        (
            "transcript (follow chip)",
            Box::new(|area, buf| {
                let view = chip.app.transcript.view(area, &mut chip.app.renderer);
                assert!(chip.app.transcript.scrolled());
                view.render(area, buf);
            }),
        ),
        (
            "status strip",
            Box::new(|area, buf| {
                StatusStrip(Some(Span::styled("thinking for 3s", DIM))).render(area, buf);
            }),
        ),
        (
            "footer bar",
            Box::new(|area, buf| {
                FooterBar {
                    left: "openai:test-model . no session".to_string(),
                    right: Line::from("in 10 out 20"),
                }
                .render(area, buf);
            }),
        ),
        (
            "confirm modal",
            Box::new(|area, buf| PromptModal(&confirm).render(area, buf)),
        ),
        (
            "input modal",
            Box::new(|area, buf| PromptModal(&input).render(area, buf)),
        ),
        (
            "select modal",
            Box::new(|area, buf| PromptModal(&select).render(area, buf)),
        ),
        (
            "checkbox modal",
            Box::new(|area, buf| PromptModal(&checkbox).render(area, buf)),
        ),
        (
            "todo panel",
            Box::new(|area, buf| (&todo).render(area, buf)),
        ),
        (
            "palette band",
            Box::new(|area, buf| {
                PaletteView {
                    state: &palette,
                    no_color: false,
                }
                .render(area, buf)
            }),
        ),
        (
            "search band",
            Box::new(|area, buf| SearchView(&search).render(area, buf)),
        ),
        (
            "rewind band",
            Box::new(|area, buf| RewindView(&rewind).render(area, buf)),
        ),
        (
            "queued preview",
            Box::new(|area, buf| QueuedPreview(&queued).render(area, buf)),
        ),
    ];
    let screen = Rect::new(0, 0, 10, 3);
    for (name, render) in &mut widgets {
        let mut buf = Buffer::empty(screen);
        render(Rect::new(0, 0, 40, 10), &mut buf);
        assert_ne!(buf, Buffer::empty(screen), "{name}: nothing drawn");

        let mut buf = Buffer::empty(screen);
        render(Rect::new(20, 20, 5, 5), &mut buf);
        assert_eq!(
            buf,
            Buffer::empty(screen),
            "{name}: drew outside the buffer"
        );
    }
}

/// A line sent while scrolled up would otherwise land out of sight.
#[test]
fn a_submitted_line_brings_the_view_back_to_the_bottom() {
    let mut harness = Harness::standalone(80, 24);
    fill_the_view(&mut harness);
    harness.press(KeyCode::PageUp);
    assert!(harness.app.transcript.scrolled());
    harness.app.begin_turn("late question", false);
    assert!(!harness.app.transcript.scrolled());
    let frame = harness.draw();
    assert!(frame.contains("> late question"), "{frame}");
    assert!(!frame.contains("End = follow"), "{frame}");
}

/// The wheel moves the view; a left click on a row that opens
/// focuses and opens it, mapped through the rows of the last frame.
#[test]
fn the_mouse_scrolls_the_view_and_opens_the_row_it_clicks() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
    let mouse = |kind: MouseEventKind, column: u16, row: u16| {
        TermInput::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    let row = screen_row(&harness, "fs_read(");
    assert!(
        harness
            .app
            .handle_input(mouse(MouseEventKind::Down(MouseButton::Left), 4, row))
            .is_none()
    );
    assert!(harness.app.transcript.focused());
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Tool { expanded: true, .. }
    ));
    let frame = harness.draw();
    assert!(frame.contains("fn main()"), "{frame}");
    assert!(
        row_modifier(&harness, "fs_read(").contains(Modifier::REVERSED),
        "the clicked header is not reversed"
    );

    fill_the_view(&mut harness);
    assert!(
        harness
            .app
            .handle_input(mouse(MouseEventKind::ScrollUp, 0, 0))
            .is_none()
    );
    assert!(harness.app.transcript.scrolled());
    assert!(harness.draw().contains("End = follow"));
    harness
        .app
        .handle_input(mouse(MouseEventKind::ScrollDown, 0, 0));
    assert!(!harness.app.transcript.scrolled());
}

#[test]
fn end_on_an_unscrolled_view_moves_the_composer_cursor() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("ac");
    harness.press(KeyCode::Left);
    harness.type_text("b");
    assert!(harness.press(KeyCode::End).is_none());
    assert!(!harness.app.transcript.scrolled());
    harness.type_text("d");
    assert_eq!(harness.app.composer_text(), "abcd");
}

/// Where the composer lands at 80x24 with a one-line draft: the rule
/// above its three text rows, with the footer under them.
const COMPOSER_RULE_ROW: u16 = 19;
const COMPOSER_TEXT_ROW: u16 = 20;

fn composer_cell(harness: &Harness, x: u16) -> (Color, bool) {
    let cell = &harness.terminal.backend().buffer()[(x, COMPOSER_TEXT_ROW)];
    (cell.fg, cell.modifier.contains(Modifier::BOLD))
}

/// The symbols of the last drawn frame's row `y`, unquoted.
fn row_text(harness: &Harness, y: u16) -> String {
    let buffer = harness.terminal.backend().buffer();
    (0..buffer.area().width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

#[test]
fn a_typed_command_is_painted_green_on_the_field_under_the_rule() {
    let mut harness = Harness::standalone(80, 24);
    // The test process's stdout is a pipe, which `NO_COLOR` reads as a
    // colourless terminal.
    harness.app.set_no_color(false);
    harness.type_text(".info tools");
    let frame = harness.draw();
    assert_welcome_snapshot("composer_command_80x24", &frame);
    let buffer = harness.terminal.backend().buffer();
    assert_eq!(
        buffer[(0, COMPOSER_RULE_ROW)].symbol(),
        ratatui::symbols::line::HORIZONTAL
    );
    assert!(
        buffer[(0, COMPOSER_RULE_ROW)]
            .modifier
            .contains(Modifier::DIM)
    );
    assert!(
        (0..11).all(|x| composer_cell(&harness, x) == (Color::Green, true)),
        "{frame}"
    );
    assert_eq!(composer_cell(&harness, 11), (Color::Reset, false));
    assert_eq!(buffer[(0, COMPOSER_TEXT_ROW)].bg, Color::Indexed(236));
    assert_eq!(buffer[(79, COMPOSER_TEXT_ROW + 2)].bg, Color::Indexed(236));
}

#[test]
fn no_color_keeps_the_command_bold_without_colour_or_surface() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(true);
    harness.type_text(".info tools");
    let frame = harness.draw();
    assert_welcome_snapshot("composer_command_no_color_80x24", &frame);
    assert!(
        (0..11).all(|x| composer_cell(&harness, x) == (Color::Reset, true)),
        "{frame}"
    );
    let buffer = harness.terminal.backend().buffer();
    assert!((0..80).all(|x| buffer[(x, COMPOSER_TEXT_ROW)].bg == Color::Reset));
}

/// The textarea draws its cursor as a reversed cell; after the draft
/// it has to sit on the field's text rows, not on the rule.
#[test]
fn the_cursor_sits_inside_the_field_under_the_rule() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    harness.render();
    let buffer = harness.terminal.backend().buffer();
    let cursor = buffer
        .area
        .positions()
        .find(|position| buffer[*position].modifier.contains(Modifier::REVERSED))
        .expect("the composer draws its cursor");
    assert_eq!((cursor.x, cursor.y), (5, COMPOSER_TEXT_ROW));
}

fn palette(harness: &Harness) -> Option<&PaletteState> {
    match &harness.app.view {
        View::Palette(palette) => Some(palette),
        _ => None,
    }
}

#[test]
fn a_typed_sigil_opens_the_palette_with_no_highlight() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".");
    let palette = palette(&harness).expect("the palette opens on a sigil");
    assert_eq!(palette.highlight, None);
    assert!(palette.rows.iter().any(|row| row.value == ".help "));
    assert_eq!(palette.token_range, 0..1);
    harness.type_text("in");
    let frame = harness.draw();
    assert_welcome_snapshot("palette_open_80x24", &frame);
    assert!(frame.contains(".info"), "{frame}");
    assert!(frame.contains("Show system info"), "{frame}");
}

/// Seeded from the context, the first `.` lists what the context
/// allows, the rows the REPL completer gives: the tool and skill
/// commands a state without function calling hides, and `.agent`,
/// since function calling and skills are capabilities, not an active
/// role, session, RAG or agent.
#[test]
fn the_palette_seeded_from_the_context_lists_what_the_context_allows() {
    let mut ctx = chat_ctx();
    assert!(ctx.app.config.function_calling_support);
    assert!(ctx.resolved_skills_enabled());
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness.press_with_ctx(KeyCode::Char('.'), Some(&ctx));
    let rows: Vec<&str> = palette(&harness)
        .expect("the palette opens on a sigil")
        .rows
        .iter()
        .map(|row| row.value.trim_end())
        .collect();
    for present in [".info tools", ".skill load", ".tool enable", ".agent"] {
        assert!(rows.contains(&present), "{present} missing from {rows:?}");
    }
}

#[test]
fn enter_with_no_highlight_submits_the_line() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".in");
    assert!(palette(&harness).is_some());
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(".in".to_string()))
    );
    assert!(palette(&harness).is_none());
    assert!(harness.app.composer.is_empty());
}

/// Mid-turn the same Enter holds the line instead; a command waits
/// without reaching the loop.
#[test]
fn enter_with_the_palette_open_and_no_highlight_queues_the_command_mid_turn() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.type_text(".info");
    assert_eq!(
        palette(&harness).map(|palette| palette.highlight),
        Some(None)
    );
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        harness.app.queued,
        vec![QueuedLine {
            text: ".info".to_string(),
            command: true
        }]
    );
    assert!(palette(&harness).is_none());
    assert!(harness.app.composer.is_empty());
}

#[test]
fn tab_highlights_the_first_row_and_enter_takes_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".inf");
    assert!(harness.press(KeyCode::Tab).is_none());
    let open = palette(&harness).expect("still open");
    assert_eq!(open.highlight, Some(0));
    let first = open.rows[0].value.clone();
    assert!(
        first.starts_with(".in") && first.ends_with(' '),
        "{first:?}"
    );
    assert!(harness.press(KeyCode::Enter).is_none());
    assert_eq!(harness.app.composer_text(), first);
    assert!(palette(&harness).is_none_or(|open| open.token_range.start > 0));
    harness.type_text("x");
    assert_eq!(harness.app.composer_text(), format!("{first}x"));
}

#[test]
fn up_from_the_first_row_clears_the_highlight() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".");
    harness.press(KeyCode::Down);
    harness.press(KeyCode::Down);
    assert_eq!(palette(&harness).unwrap().highlight, Some(1));
    harness.press(KeyCode::BackTab);
    assert_eq!(palette(&harness).unwrap().highlight, Some(0));
    harness.press(KeyCode::Up);
    assert_eq!(palette(&harness).unwrap().highlight, None);
}

#[test]
fn esc_closes_the_palette_without_interrupting_or_losing_the_draft() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hello", false);
    harness.type_text(".in");
    assert!(palette(&harness).is_some());
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(palette(&harness).is_none());
    assert_eq!(harness.app.composer_text(), ".in");
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    harness.type_text("f");
    assert!(palette(&harness).is_some(), "an edit reopens it");
}

#[test]
fn tab_reopens_a_closed_palette_on_a_command_line() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".in");
    harness.press(KeyCode::Esc);
    assert!(palette(&harness).is_none());
    harness.press(KeyCode::Tab);
    assert!(palette(&harness).is_some());
    assert_eq!(harness.app.composer_text(), ".in");
}

/// A fence opened in front of the command still gets the palette,
/// with the token to replace behind the fence.
#[test]
fn a_fenced_command_line_opens_the_palette_behind_the_fence() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("::: .in");
    let palette = palette(&harness).expect("the fenced line opens the palette");
    assert!(palette.rows.iter().any(|row| row.value == ".info "));
    assert_eq!(palette.token_range, 4..7);
}

#[test]
fn a_space_after_a_full_command_keeps_the_palette_only_for_arguments() {
    let ctx = chat_ctx();
    let mut harness = Harness::for_ctx(&ctx);
    for c in ".help".chars() {
        harness.press_with_ctx(KeyCode::Char(c), Some(&ctx));
    }
    assert!(palette(&harness).is_some());
    harness.press_with_ctx(KeyCode::Char(' '), Some(&ctx));
    assert!(palette(&harness).is_none());

    harness.app.set_composer_text(".set");
    harness.app.refresh_palette(Some(&ctx));
    harness.press_with_ctx(KeyCode::Char(' '), Some(&ctx));
    let palette = palette(&harness).expect("the settable keys are rows");
    assert!(palette.rows.iter().all(|row| !row.value.starts_with('.')));
    // Argument values keep the completer's trailing space, so the
    // accepted token ends ready for the next one.
    assert!(
        palette.rows.iter().any(|row| row.value == "highlight "),
        "{:?}",
        palette.rows
    );
    assert_eq!(palette.token_range, 5..5);
    harness.press_with_ctx(KeyCode::Tab, Some(&ctx));
    let first = palette_first_value(&harness);
    harness.press_with_ctx(KeyCode::Enter, Some(&ctx));
    assert_eq!(harness.app.composer_text(), format!(".set {first}"));
}

fn palette_first_value(harness: &Harness) -> String {
    palette(harness).unwrap().rows[0].value.clone()
}

#[test]
fn mid_turn_the_palette_lists_commands_from_the_snapshot_only() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hello", false);
    harness.type_text(".set ");
    assert!(palette(&harness).is_none());
    harness.app.set_composer_text(".h");
    harness.app.refresh_palette(None);
    let palette = palette(&harness).expect("commands need no context");
    assert!(palette.rows.iter().any(|row| row.value == ".help "));
}

#[test]
fn a_prompt_arriving_closes_the_palette() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hello", false);
    harness.type_text(".in");
    assert!(palette(&harness).is_some());
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    assert!(palette(&harness).is_none());
    assert!(matches!(
        harness.app.key_context().modal,
        Some(ModalKind::Confirm { .. })
    ));
    harness.press(KeyCode::Esc);
    assert!(matches!(asker.join().unwrap(), Ok(PromptAnswer::Cancelled)));
}

#[test]
fn a_second_line_closes_the_palette() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".in");
    assert!(palette(&harness).is_some());
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    harness.app.refresh_palette(None);
    assert!(palette(&harness).is_none());
}

/// The composer can change under an open band without a key (an
/// editor's result, a rewind's prefill). Enter on the highlight then
/// closes the band and leaves the draft as it is: the band's token
/// range belongs to a line that is gone.
#[test]
fn enter_on_a_palette_left_behind_by_an_edit_closes_it_and_keeps_the_draft() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".in");
    harness.press(KeyCode::Tab);
    assert!(palette(&harness).is_some_and(|open| open.highlighted().is_some()));
    harness.app.set_composer_text(".i");
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(harness.app.handle_key(enter), None);
    assert!(palette(&harness).is_none());
    assert_eq!(harness.app.composer_text(), ".i");

    harness.type_text("n");
    harness.press(KeyCode::Tab);
    assert!(palette(&harness).is_some_and(|open| open.highlighted().is_some()));
    harness.app.set_composer_text("x");
    assert_eq!(harness.app.handle_key(enter), None);
    assert!(palette(&harness).is_none());
    assert_eq!(harness.app.composer_text(), "x");
}

#[test]
fn the_palette_band_sits_above_the_rule_and_keeps_the_transcript_minimum() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".");
    harness.render();
    let rows = harness.app.band_rows(Rect::new(0, 0, 80, 24));
    assert_eq!(rows, bottom_pane::BAND_MAX_ROWS);
    let frame = harness.terminal.backend().to_string();
    assert!(row_text(&harness, COMPOSER_RULE_ROW).contains(ratatui::symbols::line::HORIZONTAL));
    assert!(
        row_text(&harness, COMPOSER_RULE_ROW - 1).contains("(+"),
        "{frame}"
    );
    assert!(
        row_text(&harness, COMPOSER_RULE_ROW - rows).starts_with('.'),
        "{frame}"
    );
    assert_eq!(
        row_text(&harness, COMPOSER_RULE_ROW - rows - 1).trim(),
        IDLE_STATUS,
        "the status row sits between the transcript and the band\n{frame}"
    );

    harness.resize(80, 12);
    harness.render();
    let room = 12 - (harness.app.composer.rows() + 2 + TRANSCRIPT_MIN_ROWS);
    assert_eq!(harness.app.band_rows(Rect::new(0, 0, 80, 12)), room);
    let frame = harness.terminal.backend().to_string();
    let band_top = 12 - 1 - harness.app.composer.rows() - room;
    assert!(row_text(&harness, band_top).starts_with('.'), "{frame}");
    assert!(
        row_text(&harness, band_top + room - 1).contains("(+"),
        "the hidden count is the band's last row\n{frame}"
    );
    assert!(
        row_text(&harness, band_top + room).contains(ratatui::symbols::line::HORIZONTAL),
        "the rule follows the band\n{frame}"
    );
}

fn submit(harness: &mut Harness, line: &str) {
    harness.type_text(line);
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(line.to_string()))
    );
}

#[test]
fn up_and_down_on_a_single_line_walk_the_history_around_the_draft() {
    let mut harness = Harness::standalone(80, 24);
    assert!(harness.app.history_file.is_none(), "tests keep no file");
    submit(&mut harness, "first");
    submit(&mut harness, "second");
    harness.type_text("draft");
    assert!(harness.press(KeyCode::Up).is_none());
    assert_eq!(harness.app.composer_text(), "second");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "first");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "first", "the oldest holds");
    harness.press(KeyCode::Down);
    assert_eq!(harness.app.composer_text(), "second");
    harness.press(KeyCode::Down);
    assert_eq!(harness.app.composer_text(), "draft");
    harness.press(KeyCode::Down);
    assert_eq!(harness.app.composer_text(), "draft");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit("draft".to_string()))
    );
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "draft");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "second");
}

/// A recalled command line is not an edit: the palette stays shut so
/// Up keeps walking instead of moving a highlight, and the first real
/// edit brings the palette back.
#[test]
fn walking_the_history_does_not_reopen_the_palette() {
    let mut harness = Harness::standalone(80, 24);
    for line in [".info", "plain", ".help"] {
        harness.app.history.push(line);
    }
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), ".help");
    assert!(palette(&harness).is_none());
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "plain");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), ".info");
    assert!(palette(&harness).is_none());
    harness.press(KeyCode::Backspace);
    assert_eq!(harness.app.composer_text(), ".inf");
    assert!(palette(&harness).is_some());
}

/// A command line recalled through the Ctrl+R search is a history
/// walk too: no palette over it, and the next Up walks on.
#[test]
fn recalling_from_the_search_does_not_reopen_the_palette() {
    let mut harness = Harness::standalone(80, 24);
    for line in [".info", "plain", ".help"] {
        harness.app.history.push(line);
    }
    harness.app.handle_key(ctrl(KeyCode::Char('r')));
    harness.type_text(".he");
    assert_eq!(search(&harness).unwrap().matches, [".help"]);
    assert!(harness.press(KeyCode::Enter).is_none());
    assert_eq!(harness.app.composer_text(), ".help");
    assert!(palette(&harness).is_none());
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), ".help");
    assert!(palette(&harness).is_none());
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "plain");
}

/// Clearing the composer mid-walk, or recalling an entry through the
/// search mid-walk, ends the walk: the next Up starts from the newest
/// entry rather than carrying on from where the walk was.
#[test]
fn clearing_or_searching_ends_the_history_walk() {
    let mut harness = Harness::standalone(80, 24);
    for line in [".info", "plain", ".help"] {
        harness.app.history.push(line);
    }
    harness.press(KeyCode::Up);
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "plain");
    harness.app.handle_key(ctrl(KeyCode::Char('c')));
    assert!(harness.app.composer.is_empty());
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), ".help");

    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "plain");
    harness.app.handle_key(ctrl(KeyCode::Char('r')));
    harness.type_text(".in");
    assert!(harness.press(KeyCode::Enter).is_none());
    assert_eq!(harness.app.composer_text(), ".info");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), ".help");
}

#[test]
fn the_arrows_of_a_multi_line_draft_stay_with_the_cursor() {
    let mut harness = Harness::standalone(80, 24);
    submit(&mut harness, "old line");
    harness.type_text("one");
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    harness.type_text("two");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "one\ntwo");
    harness.type_text("!");
    assert_eq!(harness.app.composer_text(), "one!\ntwo");
}

#[test]
fn a_submitted_line_is_recorded_in_the_history_file_when_one_is_kept() {
    let dir = std::env::temp_dir().join(format!(
        "coyote-tui-history-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = dir.join("history");
    let mut harness = Harness::standalone(80, 24);
    harness.app.history_file = Some(path.clone());
    submit(&mut harness, "hello");
    submit(&mut harness, ".info");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n.info\n");
    std::fs::remove_dir_all(dir).unwrap();

    let saving = AppConfig {
        save_shell_history: true,
        ..Default::default()
    };
    assert!(history_file(&saving, None).is_some_and(|p| p.ends_with("default")));
    assert!(history_file(&saving, Some("a/b")).is_some_and(|p| p.ends_with("session_a_b")));
    assert_eq!(history_file(&plain_config(), Some("a")), None);
}

/// The plain REPL shares the history file and does not know `.keys`,
/// so the line never reaches the file, while Up in this session
/// still brings it back.
#[test]
fn dot_keys_is_recalled_in_the_session_but_never_written_to_the_history_file() {
    let dir = crate::testing::ScratchDirGuard::new("tui-history-keys");
    let path = dir.path.join("history");
    let mut harness = Harness::standalone(80, 24);
    harness.app.history_file = Some(path.clone());
    submit(&mut harness, "hello");
    harness.type_text(".keys");
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(harness.app.help.is_some(), ".keys opens the help");
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(harness.app.help.is_none());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    assert_eq!(ComposerHistory::load(&path).unwrap().search(""), ["hello"]);
    assert_eq!(harness.press(KeyCode::Up), None);
    assert_eq!(harness.app.composer_text(), ".keys");
    assert_eq!(harness.press(KeyCode::Up), None);
    assert_eq!(harness.app.composer_text(), "hello");
}

fn search(harness: &Harness) -> Option<&SearchState> {
    match &harness.app.view {
        View::HistorySearch(search) => Some(search),
        _ => None,
    }
}

#[test]
fn ctrl_r_opens_the_search_band_whose_filter_narrows_and_enter_recalls() {
    let mut harness = Harness::standalone(80, 24);
    submit(&mut harness, "git status");
    submit(&mut harness, "cargo test");
    submit(&mut harness, "git log");
    assert!(harness.app.handle_key(ctrl(KeyCode::Char('r'))).is_none());
    let open = search(&harness).expect("the band opens");
    assert_eq!(open.matches, ["git log", "cargo test", "git status"]);
    harness.type_text("git");
    let open = search(&harness).unwrap();
    assert_eq!(open.filter, "git");
    assert_eq!(open.matches, ["git log", "git status"]);
    assert_eq!(harness.app.composer_text(), "", "typing goes to the filter");
    let frame = harness.draw();
    assert!(frame.contains("history: git"), "{frame}");
    assert!(!frame.contains("cargo test"), "{frame}");
    harness.press(KeyCode::Down);
    assert!(harness.press(KeyCode::Enter).is_none());
    assert!(search(&harness).is_none());
    assert_eq!(harness.app.composer_text(), "git status");

    harness.app.handle_key(ctrl(KeyCode::Char('r')));
    harness.type_text("x");
    harness.press(KeyCode::Backspace);
    assert_eq!(search(&harness).unwrap().filter, "");
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(search(&harness).is_none());
    assert_eq!(harness.app.composer_text(), "git status");
}

#[test]
fn the_search_band_with_no_matches_closes_on_enter_leaving_the_draft() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    harness.app.handle_key(ctrl(KeyCode::Char('r')));
    assert!(search(&harness).unwrap().matches.is_empty());
    let frame = harness.draw();
    assert!(frame.contains("(no matches)"), "{frame}");
    assert!(harness.press(KeyCode::Enter).is_none());
    assert!(search(&harness).is_none());
    assert_eq!(harness.app.composer_text(), "draft");
}

fn vi_harness() -> Harness {
    let footer = FooterInfo {
        model: "openai:test-model".to_string(),
        ..FooterInfo::default()
    };
    let config = Arc::new(AppConfig {
        keybindings: "vi".to_string(),
        ..(*plain_config()).clone()
    });
    Harness::new(80, 24, config, footer)
}

#[test]
fn emacs_bindings_have_no_vi_mode() {
    let mut harness = Harness::standalone(80, 24);
    assert_eq!(harness.app.vi_mode, None);
    harness.type_text("text");
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(harness.app.vi_mode, None);
    assert!(!harness.draw().contains(composer::NORMAL_TAG));
}

#[test]
fn vi_esc_enters_normal_mode_where_the_letters_move_delete_and_return_to_insert() {
    let mut harness = vi_harness();
    assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
    harness.type_text("one two");
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
    assert!(harness.draw().contains(composer::NORMAL_TAG));
    harness.type_text("0x");
    assert_eq!(harness.app.composer_text(), "ne two");
    harness.type_text("$hx");
    assert_eq!(harness.app.composer_text(), "ne to");
    harness.type_text("0wx");
    assert_eq!(harness.app.composer_text(), "ne o");
    harness.type_text("$bx");
    assert_eq!(harness.app.composer_text(), "e o");
    harness.type_text("qz");
    assert_eq!(
        harness.app.composer_text(),
        "e o",
        "unbound letters are swallowed"
    );
    harness.type_text("dd");
    assert_eq!(harness.app.composer_text(), "");
    harness.type_text("i");
    assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
    harness.type_text("abc");
    assert_eq!(harness.app.composer_text(), "abc");
    assert!(!harness.draw().contains(composer::NORMAL_TAG));
    harness.press(KeyCode::Esc);
    harness.type_text("Ad");
    assert_eq!(harness.app.composer_text(), "abcd");
    harness.press(KeyCode::Esc);
    harness.type_text("0aX");
    assert_eq!(harness.app.composer_text(), "aXbcd");
    harness.press(KeyCode::Esc);
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit("aXbcd".to_string()))
    );
    assert_eq!(
        harness.app.vi_mode,
        Some(ViMode::Insert),
        "a sent line starts the next in insert"
    );
}

#[test]
fn a_lone_d_is_forgotten_by_the_next_key_and_j_k_move_between_lines() {
    let mut harness = vi_harness();
    harness.type_text("top");
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    harness.type_text("bottom");
    harness.press(KeyCode::Esc);
    harness.type_text("hdxd");
    assert_eq!(harness.app.composer_text(), "top\nbotto");
    harness.type_text("k0dd");
    assert_eq!(harness.app.composer_text(), "botto");
    harness.type_text("jdd");
    assert_eq!(harness.app.composer_text(), "");
}

#[test]
fn vi_esc_still_interrupts_a_turn_and_closes_a_popup_first() {
    let mut harness = vi_harness();
    harness.app.begin_turn("hello", false);
    harness.type_text("draft");
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.set_composer_text(".in");
    harness.app.refresh_palette(None);
    assert!(palette(&harness).is_some());
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(palette(&harness).is_none());
    assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
}

#[test]
fn ctrl_x_shows_the_todo_panel_beside_the_transcript_and_hides_it_again() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("plan it", false);
    harness
        .app
        .todo
        .set("ship the panel".to_string(), Vec::new());
    harness.app.handle_msg(UiMsg::TodoUpdated(todo_items()));
    harness
        .app
        .handle_msg(UiMsg::AssistantText("Working through it.".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    assert!(harness.app.handle_key(ctrl(KeyCode::Char('x'))).is_none());
    let frame = harness.draw();
    insta::assert_snapshot!("todo_panel_80x24", &frame);
    for text in [
        "Todo",
        "ship the panel",
        "[x] wire the panel",
        "[ ] snapshot it narrow",
        "Working through it.",
    ] {
        assert!(frame.contains(text), "{text:?} missing:\n{frame}");
    }
    harness.app.handle_key(ctrl(KeyCode::Char('x')));
    let frame = harness.draw();
    assert!(!frame.contains("Todo"), "{frame}");
    assert!(frame.contains("Working through it."), "{frame}");
}

#[test]
fn on_a_narrow_frame_the_todo_panel_takes_the_transcripts_place() {
    let mut harness = Harness::standalone(40, 20);
    harness.app.begin_turn("plan it", false);
    harness.app.handle_msg(UiMsg::TodoUpdated(todo_items()));
    harness
        .app
        .handle_msg(UiMsg::AssistantText("Working through it.".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Char('x')));
    let frame = harness.draw();
    insta::assert_snapshot!("todo_panel_40x20", &frame);
    assert!(frame.contains("[ ] snapshot it narrow"), "{frame}");
    assert!(!frame.contains("Working through it."), "{frame}");
}

#[test]
fn the_welcome_block_greets_an_empty_transcript_until_the_first_turn() {
    let mut harness = Harness::standalone(80, 24);
    let frame = harness.draw();
    assert_welcome_snapshot("welcome_80x24", &frame);
    assert!(frame.contains(concat!("coyote ", env!("CARGO_PKG_VERSION"))));
    assert!(frame.contains("model: openai:test-model"));
    harness.type_text("hello there");
    let frame = harness.draw();
    assert!(
        frame.contains("Tip: Press"),
        "typing removed the welcome:\n{frame}"
    );
    harness.app.begin_turn("hi", false);
    let frame = harness.draw();
    assert!(!frame.contains("Tip: Press"), "{frame}");
    harness
        .app
        .handle_msg(UiMsg::AssistantText("hello".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    let frame = harness.draw();
    assert!(!frame.contains("Tip: Press"), "{frame}");
    assert!(
        !harness
            .app
            .transcript
            .cells()
            .iter()
            .any(|cell| matches!(cell, Cell::Welcome { .. }))
    );

    let mut narrow = Harness::standalone(40, 20);
    assert_welcome_snapshot("welcome_40x20", &narrow.draw());
}

#[test]
fn esc_on_a_focused_row_interrupts_the_running_turn_and_drops_the_focus() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.handle_key(ctrl(KeyCode::Up));
    assert!(harness.app.transcript.focused());
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert!(!harness.app.transcript.focused());
}

/// A submitted line is a new context: the focus goes when the line
/// leaves the composer and again when its turn begins, so the Esc
/// that follows reaches the turn.
#[test]
fn a_submitted_line_drops_the_focus_so_esc_interrupts_its_turn() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Up));
    assert!(harness.app.transcript.focused());
    harness.type_text("next question");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit("next question".to_string()))
    );
    assert!(!harness.app.transcript.focused());

    harness.app.handle_key(ctrl(KeyCode::Up));
    assert!(harness.app.transcript.focused());
    harness.app.begin_turn("next question", false);
    assert!(!harness.app.transcript.focused());
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert!(!harness.app.transcript.focused());
}

#[test]
fn enter_with_a_focused_row_and_a_draft_still_submits() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.type_text("next question");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit("next question".to_string()))
    );
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Tool {
            expanded: false,
            ..
        }
    ));
}

/// With a modal up the strip's interrupt hint would contradict the
/// modal's own `Esc = cancel`. The hint also names what Enter answers,
/// which follows the request's default; a tool's confirm without one
/// answers no.
#[test]
fn the_interrupt_hint_yields_to_an_open_modal() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("ask", false);
    assert!(harness.draw().contains("Esc to interrupt"));
    let _asker = ask_on_a_thread(&mut harness, confirm_request(None));
    let rendered = harness.draw();
    assert!(!rendered.contains("Esc to interrupt"), "{rendered}");
    assert!(
        rendered.contains("y = yes  n = no  Enter = yes  Esc = cancel"),
        "{rendered}"
    );
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Esc);
    let _asker = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
    assert!(
        harness
            .draw()
            .contains("y = yes  n = no  a = yes for this session  Enter = no  Esc = cancel")
    );
    harness.press(KeyCode::Esc);
    let no_by_default = PromptRequest {
        default: Some(serde_json::json!(false)),
        ..confirm_request(None)
    };
    let _asker = ask_on_a_thread(&mut harness, no_by_default);
    let rendered = harness.draw();
    assert!(rendered.contains("Enter = no"), "{rendered}");
    harness.press(KeyCode::Esc);
}

/// The text from `signature` to the first column-zero closing brace,
/// with CRLF line ends (a Windows checkout) read as LF.
fn fn_body(source: &str, signature: &str) -> String {
    let source = source.replace("\r\n", "\n");
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} not found"));
    let body = &source[start..];
    body[..body.find("\n}\n").unwrap()].to_string()
}

/// A `(file, function)` pair under `src/` whose body reads the
/// keyboard directly, with whether that body checks the TUI flag.
struct KeyboardReader {
    file: String,
    function: String,
    gated: bool,
}

/// The name on a `fn` line, if the line declares one.
fn declared_fn(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    if rest.starts_with("//") {
        return None;
    }
    loop {
        if let Some(after) = rest.strip_prefix("pub") {
            rest = match after.strip_prefix('(') {
                Some(scoped) => &scoped[scoped.find(')')? + 1..],
                None => after,
            }
            .trim_start();
        } else if let Some(after) = ["async ", "unsafe ", "const "]
            .iter()
            .find_map(|prefix| rest.strip_prefix(prefix))
        {
            rest = after;
        } else {
            break;
        }
    }
    let name = rest.strip_prefix("fn ")?;
    let end = name.find(['(', '<'])?;
    Some(&name[..end])
}

/// Every `.rs` file under `dir`, sorted.
fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    files.sort();
    files
}

/// The calls that read the keyboard directly: crossterm's
/// `read`/`poll`, `read_single_key`, `edit_file` (which hands the tty
/// to `$EDITOR`), `run_command` and `wait_for_status` (a child on the
/// inherited tty), inquire's prompt builders (nothing else under
/// `src/` constructs a `Text`, `Select` or the like), and the
/// `InquirePrompter` constructors, since a handler seating its own
/// terminal prompter bypasses the one on `ctx.prompter`.
const NEEDLES: [&str; 14] = [
    "event::read(",
    "event::poll(",
    "read_single_key(",
    "edit_file(",
    "run_command(",
    "wait_for_status(",
    "wait_for_status_interruptible(",
    "Confirm::new(",
    "Select::new(",
    "MultiSelect::new(",
    "Text::new(",
    "Password::new(",
    "InquirePrompter::default(",
    "InquirePrompter::new(",
];

/// Every function outside this module whose body makes one of the
/// [`NEEDLES`] calls.
fn keyboard_readers() -> Vec<KeyboardReader> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = root.join("src");
    let mut readers: Vec<KeyboardReader> = Vec::new();
    for path in rust_files(&src) {
        if path.starts_with(src.join("tui")) {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\r\n", "\n");
        let file = path
            .strip_prefix(root)
            .unwrap()
            .display()
            .to_string()
            .replace('\\', "/");
        readers.extend(classify_readers(&file, &text));
    }
    readers
}

/// The functions in `source` (the text of `file`) whose body makes one
/// of the [`NEEDLES`] calls, each once, with whether the body checks
/// the TUI flag. Comments do not count on either side: a needle in
/// one is not a reader, and a gate named only in one is no gate. A
/// `debug_assert!` is no gate either: release builds drop it, so the
/// reader still runs under the UI there.
fn classify_readers(file: &str, source: &str) -> Vec<KeyboardReader> {
    fn at_word_start(line: &str, at: usize) -> bool {
        line[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    }
    /// The line up to its `//` comment, if any. A `//` inside a string
    /// literal on the same line is part of the string.
    fn without_comment(line: &str) -> &str {
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if in_string => at += 1,
                b'\'' if !in_string => {
                    for quote_char in ["'\"'", "'\\\"'"] {
                        if line[at..].starts_with(quote_char) {
                            at += quote_char.len() - 1;
                            break;
                        }
                    }
                }
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(at + 1) == Some(&b'/') => {
                    return &line[..at];
                }
                _ => {}
            }
            at += 1;
        }
        line
    }
    /// The body's lines with every `debug_assert` invocation removed,
    /// through the line its parentheses close on.
    fn without_debug_asserts<'a>(lines: &[&'a str]) -> Vec<&'a str> {
        let mut kept = Vec::new();
        let mut depth: i32 = 0;
        for line in lines {
            let rest = if depth > 0 {
                *line
            } else if let Some(at) = line.find("debug_assert") {
                kept.push(&line[..at]);
                &line[at..]
            } else {
                kept.push(*line);
                continue;
            };
            depth += rest.matches('(').count() as i32 - rest.matches(')').count() as i32;
            depth = depth.max(0);
        }
        kept
    }
    let lines: Vec<&str> = source.lines().collect();
    let mut readers: Vec<KeyboardReader> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let code = without_comment(line);
        let hit = NEEDLES.iter().any(|needle| {
            code.match_indices(needle)
                .any(|(at, _)| at_word_start(code, at))
        });
        if !hit {
            continue;
        }
        let fn_index = (0..=index)
            .rev()
            .find(|i| declared_fn(lines[*i]).is_some())
            .unwrap_or_else(|| panic!("{file}:{} is outside any fn", index + 1));
        let fn_line = lines[fn_index];
        let indent = &fn_line[..fn_line.len() - fn_line.trim_start().len()];
        let close = format!("{indent}}}");
        let body_end = (fn_index..lines.len())
            .find(|i| lines[*i] == close || (*i == fn_index && fn_line.ends_with('}')))
            .unwrap();
        let code_lines: Vec<&str> = lines[fn_index..=body_end]
            .iter()
            .map(|line| without_comment(line))
            .collect();
        let body = without_debug_asserts(&code_lines).join("\n");
        let function = declared_fn(fn_line).unwrap().to_string();
        if readers.iter().any(|r| r.function == function) {
            continue;
        }
        readers.push(KeyboardReader {
            gated: body.contains("crate::tui::active()"),
            file: file.to_string(),
            function,
        });
    }
    readers
}

#[test]
fn a_bare_inquire_builder_is_an_ungated_reader() {
    let readers = classify_readers(
        "src/probe.rs",
        "fn fresh() { Select::new(\"q\", vec![]).prompt(); }\n",
    );
    let [reader] = readers.as_slice() else {
        panic!("{} readers", readers.len());
    };
    assert_eq!(reader.file, "src/probe.rs");
    assert_eq!(reader.function, "fresh");
    assert!(!reader.gated);
}

#[test]
fn a_real_tui_gate_marks_the_reader_gated() {
    let readers = classify_readers(
        "src/probe.rs",
        "fn fresh() {\n    if crate::tui::active() {\n        return;\n    }\n    Select::new(\"q\", vec![]).prompt();\n}\n",
    );
    let [reader] = readers.as_slice() else {
        panic!("{} readers", readers.len());
    };
    assert_eq!(reader.function, "fresh");
    assert!(reader.gated);
}

#[test]
fn a_gate_mentioned_only_in_a_comment_does_not_count() {
    let readers = classify_readers(
        "src/probe.rs",
        "fn fresh() {\n    // crate::tui::active() is checked by the caller\n    Select::new(\"q\", vec![]).prompt(); // not crate::tui::active()\n}\n",
    );
    let [reader] = readers.as_slice() else {
        panic!("{} readers", readers.len());
    };
    assert_eq!(reader.function, "fresh");
    assert!(!reader.gated);
}

#[test]
fn a_debug_assert_on_the_tui_flag_is_no_gate() {
    let readers = classify_readers(
        "src/fixture.rs",
        "fn fresh() {\n    debug_assert!(\n        !crate::tui::active(),\n        \"never under the UI\"\n    );\n    Select::new(\"q\", vec![]).prompt();\n}\n",
    );
    let [reader] = readers.as_slice() else {
        panic!("{} readers", readers.len());
    };
    assert_eq!(reader.function, "fresh");
    assert!(!reader.gated);
}

#[test]
fn a_needle_only_in_a_trailing_comment_is_no_reader() {
    let readers = classify_readers(
        "src/fixture.rs",
        "fn fresh() {\n    let x = 1; // Select::new(\"q\", vec![]) used to live here\n}\n",
    );
    assert!(readers.is_empty(), "{} readers", readers.len());
}

#[test]
fn a_double_slash_inside_a_string_is_not_a_comment() {
    let readers = classify_readers(
        "src/fixture.rs",
        "fn fresh() {\n    if url == \"http://x\" && crate::tui::active() {\n        return;\n    }\n    Select::new(\"q\", vec![]).prompt();\n}\n",
    );
    let [reader] = readers.as_slice() else {
        panic!("{} readers", readers.len());
    };
    assert_eq!(reader.function, "fresh");
    assert!(reader.gated);
}

/// Every direct keyboard reader outside this module either checks the
/// TUI flag or is listed here with the reason it stays REPL-only, so
/// a new reader cannot appear without a decision about the TUI.
#[test]
fn every_direct_keyboard_reader_is_gated_or_accounted_for() {
    const BEFORE_UI: &str = "first-run config wizard; runs before any UI starts";
    const CLI_ONLY: &str = "a CLI flow (-e, --mcp); the process never reaches a UI";
    const CLI_FLAGS: &str = "the CLI flag paths of main (--install, --update, --vault, ...) and install_builtins (Skip mode, never prompts); each returns before any UI starts";
    const REPL_PROMPTER: &str =
        "the REPL's Prompter; the TUI seats TuiPrompter on ctx.prompter instead";
    const VIA_EDIT_GATE: &str = "calls edit_file, which lends the terminal under the TUI";
    const VIA_RUN_GATE: &str = "calls run_command, which lends the terminal under the TUI";
    const REPL_PAINTER: &str =
        "polled by the spinner and the REPL stream painter, both replaced under the TUI";
    const NULL_STDIN: &str = "a unit test; the child it waits on has stdin at /dev/null";
    const LOADER_STDIN_CLOSED: &str =
        "a document loader; its child runs with stdin at /dev/null on every path";
    let allowlist: &[(&str, &str, &str)] = &[
        (
            "src/client/common.rs",
            "create_openai_compatible_client_config",
            BEFORE_UI,
        ),
        (
            "src/client/common.rs",
            "set_client_models_config",
            BEFORE_UI,
        ),
        ("src/client/common.rs", "select_model", BEFORE_UI),
        ("src/client/common.rs", "prompt_input_string", BEFORE_UI),
        ("src/client/macros.rs", "create_client_config", BEFORE_UI),
        ("src/config/mod.rs", "create_config_file", BEFORE_UI),
        ("src/config/mod.rs", "create_config_file_sandbox", BEFORE_UI),
        (
            "src/config/request_context.rs",
            "edit_config",
            VIA_EDIT_GATE,
        ),
        (
            "src/config/request_context.rs",
            "edit_mcp_config",
            VIA_EDIT_GATE,
        ),
        (
            "src/config/request_context.rs",
            "edit_session",
            VIA_EDIT_GATE,
        ),
        (
            "src/config/request_context.rs",
            "edit_agent_config",
            VIA_EDIT_GATE,
        ),
        ("src/config/request_context.rs", "new_macro", VIA_EDIT_GATE),
        (
            "src/config/request_context.rs",
            "upsert_role",
            VIA_EDIT_GATE,
        ),
        (
            "src/config/request_context.rs",
            "upsert_skill",
            VIA_EDIT_GATE,
        ),
        (
            "src/config/request_context.rs",
            "edit_rag_docs",
            VIA_EDIT_GATE,
        ),
        (
            "src/config/request_context.rs",
            "default_prompter",
            REPL_PROMPTER,
        ),
        ("src/main.rs", "run_cli_flag", CLI_FLAGS),
        ("src/main.rs", "shell_execute", CLI_ONLY),
        ("src/mcp/manage.rs", "confirm_on_the_terminal", CLI_ONLY),
        ("src/mcp/manage.rs", "provision_secrets", CLI_ONLY),
        ("src/prompt/mod.rs", "select", REPL_PROMPTER),
        ("src/prompt/mod.rs", "input", REPL_PROMPTER),
        ("src/prompt/mod.rs", "password", REPL_PROMPTER),
        ("src/prompt/mod.rs", "confirm", REPL_PROMPTER),
        ("src/prompt/mod.rs", "prompt", REPL_PROMPTER),
        ("src/repl/mod.rs", "handle_shell_passthrough", VIA_RUN_GATE),
        (
            "src/utils/abort_signal.rs",
            "drain_terminal_events",
            REPL_PAINTER,
        ),
        (
            "src/utils/command.rs",
            "a_signal_killed_loader_is_not_a_success",
            NULL_STDIN,
        ),
        (
            "src/utils/command.rs",
            "run_loader_command",
            LOADER_STDIN_CLOSED,
        ),
        (
            "src/utils/input.rs",
            "read_single_key",
            "its sole caller is shell_execute (-e), a CLI flow; the process never reaches a UI",
        ),
    ];
    let readers = keyboard_readers();
    assert!(!readers.is_empty());
    let unaccounted: Vec<String> = readers
        .iter()
        .filter(|r| !r.gated)
        .filter(|r| {
            !allowlist
                .iter()
                .any(|(file, function, _)| *file == r.file && *function == r.function)
        })
        .map(|r| format!("{}:{}", r.file, r.function))
        .collect();
    assert!(
        unaccounted.is_empty(),
        "keyboard readers neither gated on crate::tui::active() nor allowlisted:\n{}",
        unaccounted.join("\n")
    );
    let stale: Vec<String> = allowlist
        .iter()
        .filter(|(file, function, _)| {
            !readers
                .iter()
                .any(|r| r.file == *file && r.function == *function && !r.gated)
        })
        .map(|(file, function, _)| format!("{file}:{function}"))
        .collect();
    assert!(
        stale.is_empty(),
        "allowlist entries with no matching ungated reader:\n{}",
        stale.join("\n")
    );
    assert!(
        readers
            .iter()
            .any(|r| r.file == "src/utils/command.rs" && r.function == "edit_file" && r.gated)
    );
    // The allowlist keys the inquire prompter's methods by bare name,
    // which the prompt module's free fns `confirm` and `input` share.
    // Those, and the other free askers beside them, must stay free of
    // widgets so a builder in one cannot hide behind a method's entry;
    // so must the headless prompter's `prompt`, which shares its name
    // with the inquire prompter's allowlisted method.
    let prompt = include_str!("../../prompt/mod.rs");
    for signature in [
        "pub fn ask(",
        "pub(crate) fn confirm(",
        "pub(crate) fn confirm_or_cancel(",
        "pub(crate) fn confirm_with_help(",
        "pub(crate) fn select_closed(",
        "pub(crate) fn select_closed_or_cancel(",
        "pub(crate) fn input(",
        "impl Prompter for HeadlessPrompter",
    ] {
        let body = fn_body(prompt, signature);
        for needle in NEEDLES {
            assert!(
                !body.contains(needle),
                "{signature} reads the keyboard itself:\n{body}"
            );
        }
    }
    // The loader never reads the terminal: both of its branches hand
    // the child /dev/null, so the source says `Stdio::null()` for every
    // spawn it makes and never `Stdio::inherit()`.
    let loader = fn_body(
        include_str!("../../utils/command.rs"),
        "pub async fn run_loader_command",
    );
    assert!(!loader.contains("Stdio::inherit()"), "{loader}");
    assert_eq!(loader.matches("Stdio::null()").count(), 2, "{loader}");
}

/// The stdin readers outside this module stand down while the UI owns
/// the terminal: no spinner polls the keyboard, no stale-input drain
/// toggles raw mode under the event stream, and no cursor query waits
/// for a reply the event stream would take. The inquire prompter's
/// drain runs before every widget, tool approvals included, so keys
/// typed ahead of a tool approval never answer it.
#[test]
fn every_other_stdin_reader_checks_the_tui_flag() {
    let spinner = include_str!("../../utils/spinner.rs");
    assert!(fn_body(spinner, "\nasync fn run_with_spinner_rx<").contains("crate::tui::active()"));
    let utils = include_str!("../../utils/mod.rs");
    assert!(fn_body(utils, "pub fn drain_stale_tty_input").contains("crate::tui::active()"));
    let cursor_sync = include_str!("../../utils/cursor_sync.rs");
    assert!(fn_body(cursor_sync, "fn resync_stdout").contains("crate::tui::active()"));
    let prompt = include_str!("../../prompt/mod.rs");
    let prompter = fn_body(prompt, "impl Prompter for InquirePrompter");
    assert!(prompter.contains("drain_stale_tty_input()"));
    assert!(!prompter.contains("req.tool.is_some()"), "{prompter}");
}

/// A document loader that writes to its output file still runs under
/// the TUI, with stdin at EOF instead of the raw tty: the `-` here
/// reads stdin and adds nothing.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn a_file_writing_loader_still_runs_under_the_tui() {
    let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
    let source = crate::utils::temp_file("-loader-source-", ".txt");
    std::fs::write(&source, "loaded under the tui x7x\n").unwrap();
    let loaded = crate::utils::run_loader_command(
        source.to_str().unwrap(),
        "txt",
        "sh -c \"cat $1 - > $2\"",
    )
    .await;
    let _ = std::fs::remove_file(&source);
    assert_eq!(loaded.unwrap(), "loaded under the tui x7x\n");
}

/// A task id in the source names the owner of a debt marker and
/// nothing else: every line carrying one is a lint suppression, a
/// `todo!`, an ignored test, or an entry of the allowlist below.
/// An id quoted as data (inside a string literal or a backtick code
/// span, the way the planning-debris lint and its fixtures spell
/// theirs) is a needle, not a reference. The needles here are spelled
/// in halves so this test's source does not match itself.
#[test]
fn task_ids_mark_only_owned_debt() {
    fn quoted(line: &str, at: usize) -> bool {
        let before = &line[..at];
        let quotes = before
            .match_indices('"')
            .filter(|(i, _)| !before[..*i].ends_with('\\'))
            .count();
        quotes % 2 == 1 || before.matches('`').count() % 2 == 1
    }
    let id_prefix = concat!("TA", "SK-");
    // Split so a grep for suppression attributes does not match this list.
    let debt_markers = [
        concat!("#[", "allow("),
        concat!("#![", "allow("),
        concat!("#[", "expect("),
        concat!("#![", "expect("),
        "cfg_attr(",
        "todo!",
        "unimplemented!",
        concat!("#[", "ignore"),
    ];
    // Each entry is a refusal or a parked test whose line carries the
    // id of the work that removes it.
    let allowlisted: &[(&str, &str)] = &[];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for dir in ["src", "tests"] {
        for path in rust_files(&root.join(dir)) {
            let file = path.strip_prefix(root).unwrap().display().to_string();
            let text = std::fs::read_to_string(&path).unwrap();
            for (index, line) in text.lines().enumerate() {
                let bare: Vec<usize> = line
                    .match_indices(id_prefix)
                    .map(|(at, _)| at)
                    .filter(|at| !quoted(line, *at))
                    .collect();
                if bare.is_empty() {
                    continue;
                }
                let owned = debt_markers.iter().any(|marker| line.contains(marker));
                let listed = allowlisted
                    .iter()
                    .any(|(f, needle)| *f == file && line.contains(needle));
                if !owned && !listed {
                    offenders.push(format!("{file}:{}: {}", index + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "task ids outside a debt marker:\n{}",
        offenders.join("\n")
    );
}

fn press_ctrl(harness: &mut Harness, c: char) {
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}

/// The pager reads the whole output a tool printed, where its row in
/// the transcript keeps the newest lines only; `q` gives the
/// transcript back as it was.
#[test]
fn ctrl_t_shows_a_tools_whole_output_and_q_closes_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("count", false);
    narrate(
        &mut harness,
        "call-1",
        "Call execute_command {\"command\":\"seq 30\"}",
    );
    start_tool(&mut harness, "call-1", "execute_command", None);
    let output: String = (1..=30).map(|n| format!("count {n:02}\n")).collect();
    tool_output(&mut harness, "call-1", &output);
    finish_tool(&mut harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    let Some(Cell::Tool { output: tail, .. }) = harness.app.transcript.cells().get(1) else {
        panic!("the tool row is gone");
    };
    assert_eq!(tail.first().map(String::as_str), Some("count 11"));
    let before = harness.draw();

    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Char('g'));
    let frame = harness.draw();
    insta::assert_snapshot!("pager_over_a_turn_80x24", &frame);
    assert!(frame.contains("count 01"), "{frame}");
    assert!(frame.contains("Thread: root thread"), "{frame}");
    harness.press(KeyCode::Char('q'));
    assert!(harness.app.pager.is_none());
    assert_eq!(harness.draw(), before);
}

/// The style of the first screen cell of `needle`.
fn style_of(harness: &Harness, needle: &str) -> Style {
    let buffer = harness.terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let mut text = String::new();
        let mut starts = Vec::new();
        for x in 0..buffer.area.width {
            starts.push(text.len());
            text.push_str(buffer[(x, y)].symbol());
        }
        if let Some(at) = text.find(needle)
            && let Some(x) = starts.iter().position(|start| *start == at)
        {
            return buffer[(u16::try_from(x).unwrap(), y)].style();
        }
    }
    panic!("{needle:?} is not on screen:\n{buffer:?}");
}

/// Hands `ctx` a config that differs from the one it has only in
/// whether replies are highlighted.
fn swap_highlight(ctx: &mut RequestContext, highlight: bool) {
    let mut state = (*ctx.app).clone();
    state.config = Arc::new(AppConfig {
        highlight,
        ..(*ctx.app.config).clone()
    });
    ctx.app = Arc::new(state);
}

/// A new config brings a new renderer, and every view restyles what
/// it already laid out: the transcript, the pager's root thread and
/// a child's thread open in the pager.
#[test]
#[serial]
fn a_new_renderer_restyles_the_transcript_and_the_pager() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-renderer-swap");
        let root = ChildSessionRoot::Proc { pid: 1, start: 15 };
        write_child(&root, "reviewer-c5s", &child_yaml("'**bold c5s**'"));
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness
            .app
            .transcript
            .push(Cell::assistant("**bold t5s**".to_string(), true));

        harness.draw();
        let plain_transcript = style_of(&harness, "bold t5s");
        press_ctrl(&mut harness, 't');
        harness.draw();
        let plain_root = style_of(&harness, "bold t5s");
        harness.press(KeyCode::Char('q'));
        harness
            .app
            .open_child("reviewer-c5s".to_string(), "reviewer".to_string());
        run_control_requests(&mut harness).await;
        harness.draw();
        let plain_child = style_of(&harness, "bold c5s");

        swap_highlight(&mut ctx, true);
        harness.app.sync_from_ctx(&mut ctx);
        harness.draw();
        assert_ne!(style_of(&harness, "bold c5s"), plain_child);
        harness.press(KeyCode::Char('q'));
        harness.draw();
        assert_ne!(style_of(&harness, "bold t5s"), plain_transcript);
        press_ctrl(&mut harness, 't');
        harness.draw();
        assert_ne!(style_of(&harness, "bold t5s"), plain_root);

        swap_highlight(&mut ctx, false);
        harness.app.sync_from_ctx(&mut ctx);
        harness.draw();
        assert_eq!(style_of(&harness, "bold t5s"), plain_root);
    });
}

fn counted_turn(harness: &mut Harness, lines: usize) {
    harness.app.begin_turn("count", false);
    narrate(
        harness,
        "call-1",
        "Call execute_command {\"command\":\"seq\"}",
    );
    start_tool(harness, "call-1", "execute_command", None);
    let output: String = (1..=lines).map(|n| format!("count {n:02}\n")).collect();
    tool_output(harness, "call-1", &output);
    finish_tool(harness, "call-1", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
}

/// Enter closes the find field on the first match and keeps the text,
/// so `n` and `N` step through the matches from the key legend.
#[test]
fn enter_in_the_pagers_find_field_leaves_n_and_shift_n_stepping() {
    let mut harness = Harness::standalone(80, 24);
    counted_turn(&mut harness, 30);
    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Char('g'));
    harness.press(KeyCode::Char('/'));
    for c in "count 0".chars() {
        harness.press(KeyCode::Char(c));
    }
    harness.press(KeyCode::Enter);
    let top_line = |frame: &str| frame.lines().nth(1).unwrap_or_default().to_string();
    let frame = harness.draw();
    assert!(top_line(&frame).contains("count 01"), "{frame}");
    assert!(
        frame.lines().last().unwrap_or_default().contains("/ find"),
        "the key legend is not back: {frame}"
    );
    harness.press(KeyCode::Char('n'));
    let frame = harness.draw();
    assert!(top_line(&frame).contains("count 02"), "{frame}");
    harness.press(KeyCode::Char('N'));
    let frame = harness.draw();
    assert!(top_line(&frame).contains("count 01"), "{frame}");
}

/// Under the pager the wheel scrolls the pager, a click does nothing
/// and a paste reaches the find field only; the composer and the
/// transcript under it are left as they were.
#[test]
fn the_pager_keeps_the_mouse_and_pastes_from_what_it_covers() {
    let mut harness = Harness::standalone(80, 24);
    counted_turn(&mut harness, 60);
    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Char('g'));
    let top = harness.draw();
    let mouse = |kind: MouseEventKind| {
        TermInput::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 3,
            modifiers: KeyModifiers::NONE,
        })
    };
    harness.app.handle_input(mouse(MouseEventKind::ScrollDown));
    let scrolled = harness.draw();
    assert_ne!(scrolled, top, "the wheel did not move the pager");
    harness
        .app
        .handle_input(mouse(MouseEventKind::Down(MouseButton::Left)));
    assert!(!harness.app.transcript.focused());
    assert_eq!(harness.draw(), scrolled);

    harness
        .app
        .handle_input(TermInput::Paste("dropped-p5x".to_string()));
    harness.press(KeyCode::Char('/'));
    harness
        .app
        .handle_input(TermInput::Paste("count\n59".to_string()));
    let frame = harness.draw();
    assert!(
        frame
            .lines()
            .last()
            .unwrap_or_default()
            .contains("/count 59"),
        "{frame}"
    );
    assert!(!frame.contains("dropped-p5x"), "{frame}");
    assert_eq!(harness.app.composer.text(), "");
}

/// Output of a call no row is waiting for (a background job still
/// writing after its turn) keeps a tail in its cell, and the pager
/// shows all of it.
#[test]
fn the_pager_shows_a_stray_calls_whole_output() {
    let mut harness = Harness::standalone(80, 60);
    let output: String = (1..=30).map(|n| format!("noise {n:02}\n")).collect();
    harness.app.handle_msg(UiMsg::ToolOutput {
        call_id: Some("job-call".to_string()),
        bytes: output.into_bytes(),
    });
    let inline = harness.draw();
    assert!(inline.contains("noise 30"), "{inline}");
    assert!(!inline.contains("noise 01"), "{inline}");
    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Char('g'));
    let frame = harness.draw();
    for n in [1, 10, 30] {
        assert!(frame.contains(&format!("noise {n:02}")), "{frame}");
    }
}

/// A question arriving while the pager is open closes the pager: it
/// must never wait out of sight.
#[test]
fn a_question_closes_the_pager() {
    let mut harness = Harness::standalone(80, 24);
    press_ctrl(&mut harness, 't');
    assert!(harness.app.pager.is_some());
    let (reply, _answer) = mpsc::channel();
    harness.app.handle_msg(UiMsg::Prompt {
        req: Box::new(confirm_request(None)),
        reply,
    });
    assert!(harness.app.pager.is_none());
    assert!(harness.draw().contains("Proceed? tui-x7x"));
}

/// `?` on an empty composer pops the key help up over the transcript
/// and Esc closes it; typed after text, `?` is just typing.
#[test]
fn question_mark_opens_the_key_help_and_esc_closes_it() {
    let mut harness = Harness::standalone(80, 24);
    assert!(harness.draw().contains(IDLE_STATUS_WITH_HELP));
    assert_eq!(harness.press(KeyCode::Char('?')), None);
    let frame = harness.draw();
    assert!(frame.contains("Keys"), "{frame}");
    assert!(frame.contains("Suspend to the shell"), "{frame}");
    harness.press(KeyCode::PageDown);
    assert!(!harness.draw().contains("Suspend to the shell"));
    harness
        .app
        .handle_input(TermInput::Paste("pasted".to_string()));
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(harness.app.help.is_none());
    assert_eq!(harness.app.composer_text(), "");
    assert!(harness.draw().contains(IDLE_STATUS_WITH_HELP));

    harness.type_text("why?");
    assert!(harness.app.help.is_none());
    assert_eq!(harness.app.composer_text(), "why?");
}

/// Each chord either status row names is a key the help lists.
#[test]
fn the_idle_status_names_only_keys_the_help_lists() {
    let chords = |status: &'static str| -> Vec<&'static str> {
        status
            .split("  ")
            .map(|entry| entry.split_once(" = ").expect(entry).0)
            .collect()
    };
    assert_eq!(chords(IDLE_STATUS), ["Alt+T", "Ctrl+X"]);
    assert_eq!(chords(IDLE_STATUS_WITH_HELP), ["?", "Alt+T", "Ctrl+X"]);
    for chord in chords(IDLE_STATUS_WITH_HELP) {
        assert!(
            keys::KEY_HELP
                .iter()
                .any(|row| row.keys.split(" / ").any(|token| token == chord)),
            "{chord} is in no help row"
        );
    }
}

/// The status row offers `?` only where it opens the help: a draft
/// or a band open above the composer takes it as typing.
#[test]
fn the_idle_status_drops_the_help_key_where_it_types() {
    let status = |harness: &Harness| harness.app.status_text().map(|span| span.content);
    let mut harness = Harness::standalone(80, 24);
    assert_eq!(status(&harness).as_deref(), Some(IDLE_STATUS_WITH_HELP));
    harness.type_text("draft");
    assert_eq!(status(&harness).as_deref(), Some(IDLE_STATUS));
    let frame = harness.draw();
    assert!(frame.contains(IDLE_STATUS), "{frame}");
    assert!(!frame.contains("? = keys"), "{frame}");
    press_ctrl(&mut harness, 'c');
    assert_eq!(harness.app.composer_text(), "");
    press_ctrl(&mut harness, 'r');
    assert!(matches!(harness.app.view, View::HistorySearch(_)));
    assert_eq!(status(&harness).as_deref(), Some(IDLE_STATUS));
    harness.press(KeyCode::Esc);
    assert_eq!(status(&harness).as_deref(), Some(IDLE_STATUS_WITH_HELP));
}

/// A turn's first frame rewrites every cell of the working text over
/// either idle hint, with and without colour, so the output stream
/// carries the words whole rather than only the letters that differ.
#[test]
fn the_first_working_frame_rewrites_the_idle_hint_whole() {
    for no_color in [false, true] {
        for draft in ["", "draft"] {
            let mut harness = Harness::standalone(80, 24);
            harness.app.set_no_color(no_color);
            harness.type_text(draft);
            let idle = harness.draw();
            let hint = if draft.is_empty() {
                IDLE_STATUS_WITH_HELP
            } else {
                IDLE_STATUS
            };
            assert!(idle.contains(hint), "{idle}");
            let before = harness.terminal.backend().buffer().clone();
            harness.app.begin_turn("work", false);
            let working = harness.draw();
            let after = harness.terminal.backend().buffer();
            let row = (0..after.area.height)
                .find(|&y| {
                    (0..after.area.width)
                        .map(|x| after[(x, y)].symbol())
                        .collect::<String>()
                        .starts_with("Working . 0s  Esc to interrupt")
                })
                .unwrap_or_else(|| panic!("no working row: {working}"));
            let updated: Vec<u16> = before
                .diff(after)
                .into_iter()
                .filter(|&(_, y, _)| y == row)
                .map(|(x, _, _)| x)
                .collect();
            let text = "Working . 0s  Esc to interrupt";
            for (x, c) in (0u16..).zip(text.chars()) {
                assert!(
                    updated.contains(&x),
                    "no_color {no_color}, draft {draft:?}: column {x} ({c}) kept from {hint:?}"
                );
                let modifier = after[(x, row)].modifier;
                assert!(
                    modifier.contains(Modifier::DIM) && !modifier.contains(Modifier::ITALIC),
                    "no_color {no_color}, draft {draft:?}: column {x} ({c}) is {modifier:?}"
                );
            }
        }
    }
}

/// The wheel scrolls whatever covers the screen: the key help, the
/// pager, or else the transcript.
#[test]
fn the_wheel_scrolls_the_help_the_pager_and_the_transcript() {
    let mut harness = Harness::standalone(80, 24);
    counted_turn(&mut harness, 60);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
    let wheel = |kind: MouseEventKind| {
        TermInput::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 3,
            modifiers: KeyModifiers::NONE,
        })
    };

    harness.press(KeyCode::Char('?'));
    let top = harness.draw();
    harness.app.handle_input(wheel(MouseEventKind::ScrollDown));
    let scrolled = harness.draw();
    assert_ne!(scrolled, top, "the wheel did not move the key help");
    harness.app.handle_input(wheel(MouseEventKind::ScrollUp));
    assert_eq!(harness.draw(), top);
    assert!(!harness.app.transcript.scrolled());
    harness.press(KeyCode::Esc);

    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Char('g'));
    let top = harness.draw();
    harness.app.handle_input(wheel(MouseEventKind::ScrollDown));
    assert_ne!(harness.draw(), top, "the wheel did not move the pager");
    assert!(!harness.app.transcript.scrolled());
    harness.press(KeyCode::Char('q'));

    harness.app.handle_input(wheel(MouseEventKind::ScrollUp));
    assert!(harness.app.transcript.scrolled());
    harness.app.handle_input(wheel(MouseEventKind::ScrollDown));
    assert!(!harness.app.transcript.scrolled());
}

/// One wheel notch moves the key help, the pager and the transcript
/// by one row: a row in the middle of the screen is found one row
/// further along after it.
#[test]
fn a_wheel_notch_moves_one_row() {
    let mut harness = Harness::standalone(80, 24);
    counted_turn(&mut harness, 60);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
    let wheel = |kind: MouseEventKind| {
        TermInput::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 3,
            modifiers: KeyModifiers::NONE,
        })
    };
    let row = |frame: &str, at: usize| frame.lines().nth(at).unwrap_or_default().to_string();

    harness.press(KeyCode::Char('?'));
    let top = harness.draw();
    harness.app.handle_input(wheel(MouseEventKind::ScrollDown));
    let scrolled = harness.draw();
    assert_eq!(
        row(&scrolled, 9),
        row(&top, 10),
        "the key help\n{top}\n{scrolled}"
    );
    harness.press(KeyCode::Esc);

    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Char('g'));
    let top = harness.draw();
    harness.app.handle_input(wheel(MouseEventKind::ScrollDown));
    let scrolled = harness.draw();
    assert_eq!(
        row(&scrolled, 9),
        row(&top, 10),
        "the pager\n{top}\n{scrolled}"
    );
    harness.press(KeyCode::Char('q'));

    let bottom = harness.draw();
    harness.app.handle_input(wheel(MouseEventKind::ScrollUp));
    let scrolled = harness.draw();
    assert_eq!(
        row(&scrolled, 11),
        row(&bottom, 10),
        "the transcript\n{bottom}\n{scrolled}"
    );
}

/// The key help is a pop-up inside the transcript's rows: the composer
/// and the footer stay as they were, the status row names no chord
/// since the help takes them all, and the transcript shows in the
/// margin around it.
#[test]
fn the_key_help_pops_up_over_the_transcript_wide_and_narrow() {
    for (width, height, name) in [
        (80, 24, "help_overlay_80x24"),
        (40, 20, "help_overlay_40x20"),
    ] {
        let mut harness = Harness::standalone(width, height);
        let closed: Vec<String> = harness.draw().lines().map(str::to_string).collect();
        harness.press(KeyCode::Char('?'));
        let frame = harness.draw();
        assert!(frame.contains("Suspend to the shell"), "{frame}");
        let open: Vec<&str> = frame.lines().collect();
        let below = usize::from(harness.app.composer.rows()) + 1;
        let first_below = open.len() - below;
        assert_eq!(open[first_below..], closed[first_below..], "{frame}");
        let status = open[first_below - 1];
        assert!(status.trim_matches(['"', ' ']).is_empty(), "{frame}");
        assert_eq!(open[0], closed[0], "{frame}");
        assert_eq!(open[first_below - 2], closed[first_below - 2], "{frame}");
        insta::assert_snapshot!(name, &frame);
    }
}

/// While the key help is up the status row keeps the turn's progress
/// but not `Esc to interrupt`: Esc closes the help.
#[test]
fn the_status_row_under_the_key_help_offers_no_interrupt() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("work", false);
    assert!(harness.draw().contains("Esc to interrupt"));
    harness.press(KeyCode::Char('?'));
    let frame = harness.draw();
    assert!(frame.contains("Working . 0s"), "{frame}");
    assert!(!frame.contains("Esc to interrupt"), "{frame}");
}

/// `.keys` opens the key help under the active prefix, between turns
/// and during one, and is never handed on as a line to run or hold.
#[test]
fn the_keys_command_opens_the_help_instead_of_reaching_the_repl() {
    for (prefix, line) in [
        (crate::config::CommandPrefix::Dot, ".keys"),
        (crate::config::CommandPrefix::Slash, "/keys"),
    ] {
        let config = Arc::new(AppConfig {
            command_prefix: prefix,
            ..(*plain_config()).clone()
        });
        let mut harness = Harness::new(80, 24, config, FooterInfo::default());
        harness.type_text(line);
        assert_eq!(harness.press(KeyCode::Enter), None, "{line}");
        assert!(harness.app.help.is_some(), "{line}");
        assert_eq!(harness.app.composer_text(), "");
        harness.press(KeyCode::Char('q'));
        assert!(harness.app.help.is_none());

        harness.app.begin_turn("work", false);
        harness.type_text(line);
        assert_eq!(harness.press(KeyCode::Enter), None, "{line}");
        assert!(harness.app.help.is_some(), "{line}");
        assert!(harness.app.queued.is_empty(), "{line}");
    }
}

/// A macro named `keys` shadows the key help command: `.keys` goes to
/// the REPL as a line, sent between turns and held during one, while
/// `?` still opens the help.
#[test]
fn a_keys_macro_takes_the_keys_command_from_the_help() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.macros = vec![(KEYS_COMMAND.to_string(), "my keys".to_string())];
    harness.type_text(".keys");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(".keys".to_string()))
    );
    assert!(harness.app.help.is_none());

    harness.press(KeyCode::Char('?'));
    assert!(harness.app.help.is_some());
    harness.press(KeyCode::Char('q'));

    harness.app.begin_turn("work", false);
    harness.type_text(".keys");
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(harness.app.help.is_none());
    assert_eq!(
        harness
            .app
            .queued
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>(),
        [".keys"]
    );
}

/// Only a draft that is the command alone, under the active prefix,
/// opens the help; arguments, a second line or the other sigil send
/// the line on as typed.
#[test]
fn the_keys_command_is_the_whole_line_or_nothing() {
    for (prefix, line) in [
        (crate::config::CommandPrefix::Dot, ".keys foo"),
        (crate::config::CommandPrefix::Dot, ".keys\nmore"),
        (crate::config::CommandPrefix::Dot, "/keys"),
        (crate::config::CommandPrefix::Slash, ".keys"),
        (crate::config::CommandPrefix::Slash, "/keys foo"),
    ] {
        let config = Arc::new(AppConfig {
            command_prefix: prefix,
            ..(*plain_config()).clone()
        });
        let mut harness = Harness::new(80, 24, config, FooterInfo::default());
        harness.app.set_composer_text(line);
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit(line.to_string())),
            "{line:?}"
        );
        assert!(harness.app.help.is_none(), "{line:?}");
    }
}

/// A `.keys` draft that Alt+Up would put on the queue opens the help
/// instead, and the held line still comes back to edit.
#[test]
fn alt_up_over_a_keys_draft_opens_the_help_instead_of_holding_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    harness.type_text("first");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Queue("first".to_string()))
    );
    harness.type_text(".keys");
    let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
    assert_eq!(
        harness.app.handle_key(alt_up),
        Some(Command::Unqueue {
            held: Some("first".to_string()),
            draft: None,
        })
    );
    assert!(harness.app.help.is_some());
    assert!(harness.app.queued.is_empty());
    assert_eq!(harness.app.composer_text(), "first");
}

/// Opening the pager closes the key help, so closing the pager gives
/// back the transcript rather than a help left under it.
#[test]
fn opening_the_pager_closes_the_key_help() {
    let mut harness = Harness::standalone(80, 24);
    harness.press(KeyCode::Char('?'));
    assert!(harness.app.help.is_some());
    harness.app.open_pager(Pager::root());
    assert!(harness.app.help.is_none());
    let frame = harness.draw();
    assert!(frame.contains("Tab threads"), "{frame}");
    assert!(!frame.contains("Suspend to the shell"), "{frame}");
    harness.press(KeyCode::Char('q'));
    assert!(harness.app.pager.is_none());
    assert!(harness.draw().contains(IDLE_STATUS_WITH_HELP));
}

/// A question arriving under the key help closes it, as it closes
/// the pager.
#[test]
fn a_question_closes_the_key_help() {
    let mut harness = Harness::standalone(80, 24);
    harness.press(KeyCode::Char('?'));
    assert!(harness.app.help.is_some());
    let (reply, _answer) = mpsc::channel();
    harness.app.handle_msg(UiMsg::Prompt {
        req: Box::new(confirm_request(None)),
        reply,
    });
    assert!(harness.app.help.is_none());
    assert!(harness.draw().contains("Proceed? tui-x7x"));
}

fn supervisor() -> Arc<parking_lot::RwLock<crate::supervisor::Supervisor>> {
    Arc::new(parking_lot::RwLock::new(
        crate::supervisor::Supervisor::new(4, 3).with_max_concurrent_jobs(4),
    ))
}

/// `.agent` swaps the supervisor and drops the escalation queue on
/// the context; the next return replaces what the roster and the
/// control context hold, and the queue the roster reads is the one
/// the context will hand its children.
#[test]
fn every_return_of_the_context_replaces_the_handles_the_roster_holds() {
    let mut ctx = chat_ctx();
    let first = supervisor();
    ctx.supervisor = Some(Arc::clone(&first));
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    let queue = ctx
        .escalation_queue
        .clone()
        .expect("the queue is made on the return");
    let held = &harness.app.roster_sources;
    assert!(Arc::ptr_eq(held.supervisor.as_ref().unwrap(), &first));
    assert!(Arc::ptr_eq(held.escalations.as_ref().unwrap(), &queue));
    let control = harness.app.control.clone().unwrap();
    assert!(Arc::ptr_eq(control.supervisor.as_ref().unwrap(), &first));
    assert!(Arc::ptr_eq(
        control.escalation_queue.as_ref().unwrap(),
        &queue
    ));
    assert!(!control.persists_session);

    let second = supervisor();
    ctx.supervisor = Some(Arc::clone(&second));
    ctx.escalation_queue = None;
    harness.app.sync_from_ctx(&mut ctx);
    let fresh = ctx.escalation_queue.clone().unwrap();
    assert!(!Arc::ptr_eq(&fresh, &queue));
    let held = &harness.app.roster_sources;
    assert!(Arc::ptr_eq(held.supervisor.as_ref().unwrap(), &second));
    assert!(Arc::ptr_eq(held.escalations.as_ref().unwrap(), &fresh));
    let control = harness.app.control.clone().unwrap();
    assert!(Arc::ptr_eq(control.supervisor.as_ref().unwrap(), &second));

    harness.app.begin_exit();
    assert!(harness.app.control.is_none());
    assert!(harness.app.roster_sources.supervisor.is_none());
}

fn running_job(id: &str) -> crate::supervisor::JobHandle {
    use crate::supervisor::{JobHandle, JobResult, JobState, JobStatus};
    JobHandle {
        id: id.to_string(),
        tool: "execute_command".to_string(),
        started_at: Instant::now(),
        join_handle: tokio::spawn(std::future::pending::<Result<JobResult>>()),
        abort_signal: create_abort_signal(),
        state: Arc::new(parking_lot::Mutex::new(JobState {
            status: JobStatus::Running,
            pgid: None,
        })),
        output_buf: Arc::new(parking_lot::Mutex::new(
            crate::function::jobs::RingBuf::default(),
        )),
        no_change_checks: 0,
        last_check_state: None,
    }
}

fn finished_agent(id: &str) -> crate::supervisor::AgentHandle {
    use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
    let result_id = id.to_string();
    AgentHandle {
        id: id.to_string(),
        agent_name: "reviewer".to_string(),
        depth: 1,
        inbox: Arc::new(crate::supervisor::mailbox::Inbox::new()),
        abort_signal: create_abort_signal(),
        join_handle: tokio::spawn(async move {
            Ok(AgentResult {
                id: result_id,
                agent_name: "reviewer".to_string(),
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
                session_id: None,
            })
        }),
        child_supervisor: None,
        session_id: None,
        child_session_root: None,
        succeeded: Default::default(),
    }
}

fn running_agent(id: &str) -> crate::supervisor::AgentHandle {
    crate::supervisor::AgentHandle {
        join_handle: tokio::spawn(std::future::pending()),
        ..finished_agent(id)
    }
}

fn footer_counts(harness: &mut Harness) -> String {
    let frame = harness.draw();
    let footer = frame.lines().last().unwrap_or_default();
    let start = footer.find("agents").expect("the footer has its counts");
    footer[start..]
        .split("  ")
        .next()
        .unwrap()
        .trim()
        .to_string()
}

/// The turn holds the context while it runs, so the footer counts
/// what the supervisor it last saw has now, not what it had then.
#[test]
fn the_footer_counts_tasks_registered_after_the_context_left() {
    run_async(async {
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        assert_eq!(footer_counts(&mut harness), "agents 0 . jobs 0");

        tasks.write().register(running_agent("agent-f1a")).unwrap();
        assert_eq!(footer_counts(&mut harness), "agents 1 . jobs 0");
        tasks.write().register(running_job("job-f1a")).unwrap();
        assert_eq!(footer_counts(&mut harness), "agents 1 . jobs 1");

        tasks.write().take("agent-f1a");
        tasks.write().take_job("job-f1a");
        assert_eq!(footer_counts(&mut harness), "agents 0 . jobs 0");
    });
}

/// A child agent's own agents and jobs live in its supervisor, and
/// the footer counts them with the main session's.
#[test]
fn the_footer_counts_the_tasks_of_child_agents() {
    run_async(async {
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        let child_tasks = supervisor();
        let mut child = running_agent("agent-c1b");
        child.child_supervisor = Some(Arc::clone(&child_tasks));
        tasks.write().register(child).unwrap();
        child_tasks
            .write()
            .register(running_agent("agent-g1b"))
            .unwrap();
        child_tasks
            .write()
            .register(running_job("job-g1b"))
            .unwrap();
        assert_eq!(footer_counts(&mut harness), "agents 2 . jobs 1");
    });
}

/// A plain chat has no agent to make a supervisor, yet `job__start`
/// is offered; the return of the context makes the one it will use,
/// so a job it starts mid-turn reaches the footer and the roster.
#[test]
fn a_job_started_without_a_supervisor_lands_where_the_footer_reads() {
    run_async(async {
        let mut ctx = chat_ctx();
        assert!(ctx.supervisor.is_none());
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        let tasks = ctx.ensure_supervisor();
        let held = harness.app.roster_sources.supervisor.as_ref();
        assert!(Arc::ptr_eq(held.expect("made on the return"), &tasks));
        tasks.write().register(running_job("job-l1c")).unwrap();
        assert_eq!(footer_counts(&mut harness), "agents 0 . jobs 1");
    });
}

/// An idle chat keeps ticking while a task runs, so the footer sees
/// it end, and stops once nothing runs.
#[test]
fn an_idle_chat_ticks_only_while_the_footer_counts_a_task() {
    run_async(async {
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        assert!(harness.app.turn.is_none() && matches!(harness.app.view, View::Composer));

        harness.render();
        harness.app.frame_drawn();
        assert_eq!(harness.app.frames.due_at(), None);

        tasks.write().register(running_agent("agent-t1d")).unwrap();
        harness.render();
        harness.app.frame_drawn();
        assert!(harness.app.frames.due_at().is_some());

        harness.app.pager = Some(Pager::root());
        harness.render();
        harness.app.frame_drawn();
        assert_eq!(harness.app.frames.due_at(), None, "the pager has no footer");
    });
}

/// An agent reports finished before its join handle does; the footer
/// counts it until the handle finishes and drops it after.
#[test]
fn the_footer_drops_an_agent_once_its_join_handle_finishes() {
    use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
    run_async(async {
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let done = finished_agent("agent-j1e");
        let result = AgentResult {
            id: done.id.clone(),
            agent_name: done.agent_name.clone(),
            output: String::new(),
            exit_status: AgentExitStatus::Completed,
            session_id: None,
        };
        tasks
            .write()
            .register(AgentHandle {
                join_handle: tokio::spawn(async move {
                    let _ = finished.await;
                    Ok(result)
                }),
                ..done
            })
            .unwrap();
        assert_eq!(footer_counts(&mut harness), "agents 1 . jobs 0");

        finish.send(()).unwrap();
        while harness.app.roster_sources.running_counts() != (0, 0) {
            tokio::task::yield_now().await;
        }
        assert_eq!(footer_counts(&mut harness), "agents 0 . jobs 0");
    });
}

/// Runs what the roster or pager queued the way the loop does and
/// hands the app the one message that comes back.
async fn run_control_requests(harness: &mut Harness) {
    let (tx, mut rx) = unbounded_channel();
    let control = harness.app.control.clone().expect("a control context");
    let requests = harness.app.take_control_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    for request in requests {
        spawn_control_request(
            request,
            Arc::clone(&control),
            tx.clone(),
            &mut harness.app.control_tasks,
        );
    }
    let msg = rx.recv().await.expect("the request reports back");
    harness.app.handle_msg(msg);
}

fn write_child(root: &ChildSessionRoot, session_id: &str, body: &str) -> PathBuf {
    let file = root.child_session_file(session_id);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, body).unwrap();
    file
}

fn child_yaml(reply: &str) -> String {
    format!(
        "model: openai:test-model\nmessages:\n- role: user\n  content: review it\n\
         - role: assistant\n  content: {reply}\n"
    )
}

/// The sink forwards a child's flush and the turn's resolved root to
/// the loop.
#[test]
fn the_sink_forwards_a_childs_flush() {
    let (tx, mut rx) = unbounded_channel();
    let root = Arc::new(ChildSessionRoot::Proc { pid: 1, start: 2 });
    let sink = TuiSink::new(tx);
    sink.on_event(&Event::ChildTranscriptFlushed {
        session_id: "reviewer-x7x".to_string(),
    });
    sink.on_event(&Event::ChildSessionRootResolved {
        root: Arc::clone(&root),
    });
    match rx.try_recv() {
        Ok(UiMsg::ChildTranscriptFlushed { session_id }) => {
            assert_eq!(session_id, "reviewer-x7x");
        }
        other => panic!("{other:?}"),
    }
    match rx.try_recv() {
        Ok(UiMsg::ChildSessionRootResolved(sent)) => assert!(Arc::ptr_eq(&sent, &root)),
        other => panic!("{other:?}"),
    }
}

/// What the exit prints is the turns that ended: the exit's own
/// output is printed apart from it.
#[test]
fn the_dump_covers_the_committed_turns_only() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hello there", false);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("general kenobi".to_string()));
    assert!(
        harness.app.committed_cells().is_empty(),
        "a running turn was counted as committed"
    );
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.begin_exit();
    harness
        .app
        .push_notice(NoticeKind::Routing, "Saved the session".to_string());
    let text = dump::transcript_text(harness.app.committed_cells(), &harness.app.config);
    assert_eq!(render::plain_text(&text), "> hello there\ngeneral kenobi\n");
}

/// A signal mid-turn leaves the half-finished turn out of what is
/// printed, even after the turn is wound up during the exit.
#[test]
fn the_dump_leaves_out_a_turn_the_exit_cut_short() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("first", false);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("one".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.begin_turn("second", false);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("half of tw".to_string()));
    harness.app.begin_exit();
    harness.app.end_turn(false, &TurnOutcome::default());
    let text = dump::transcript_text(harness.app.committed_cells(), &harness.app.config);
    assert_eq!(render::plain_text(&text), "> first\none\n");
}

/// Comments in the UI's sources describe the code, not the process
/// that produced it: none carries the vocabulary of a plan or a
/// review. The needles are spelled in halves (or built from parts) so
/// this test's source does not match itself.
#[test]
fn ui_comments_carry_no_planning_vocabulary() {
    let needles = [
        concat!("acceptance ", "criteri"),
        concat!("usage-", "probe"),
        concat!("usage-pattern ", "probe"),
        concat!("spec-", "first"),
        concat!("quality ", "bar"),
        concat!("scope ", "extension"),
    ]
    .map(str::to_lowercase);
    // Word-bounded needles: the word itself, a letter after `R-`, or
    // the word followed by a digit, with no letter or digit touching
    // either side.
    let bounded = [
        concat!("rul", "ing"),
        concat!("rul", "ings"),
        concat!("the ", "plan"),
        concat!("reci", "pe"),
    ];
    let numbered_words = [
        concat!("rou", "nd "),
        concat!("pha", "se "),
        concat!("st", "ep "),
    ];
    let is_boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    let bounded_hit = |lower: &str, raw: &str| {
        let word = bounded.iter().any(|needle| {
            lower.match_indices(needle).any(|(at, _)| {
                is_boundary(lower[..at].chars().next_back())
                    && is_boundary(lower[at + needle.len()..].chars().next())
            })
        });
        let ruling_id = raw.match_indices(concat!("R", "-")).any(|(at, _)| {
            let after = raw[at + 2..].chars().next();
            is_boundary(raw[..at].chars().next_back())
                && after.is_some_and(|c| c.is_ascii_uppercase())
                && is_boundary(raw[at + 3..].chars().next())
        });
        let numbered = numbered_words.iter().any(|needle| {
            lower.match_indices(needle).any(|(at, _)| {
                is_boundary(lower[..at].chars().next_back())
                    && lower[at + needle.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit())
            })
        });
        word || ruling_id || numbered
    };
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = rust_files(&root.join("src/tui"));
    files.extend(rust_files(&root.join("tests/common")));
    // Every UI suite: `tui_binary.rs` and each `tui_*.rs` beside it.
    let mut suites: Vec<_> = rust_files(&root.join("tests"))
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("tui_"))
        })
        .collect();
    suites.sort();
    assert!(suites.len() >= 4, "{suites:?}");
    files.extend(suites);
    let mut offenders = Vec::new();
    for path in files {
        let file = path.strip_prefix(root).unwrap().display().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        for (index, line) in text.lines().enumerate() {
            if !line.trim_start().starts_with("//") {
                continue;
            }
            let lower = line.to_lowercase();
            if needles.iter().any(|needle| lower.contains(needle)) || bounded_hit(&lower, line) {
                offenders.push(format!("{file}:{}: {}", index + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "planning vocabulary in comments:\n{}",
        offenders.join("\n")
    );
}

fn escalate_as(
    queue: &crate::supervisor::escalation::EscalationQueue,
    id: &str,
    question: &str,
) -> tokio::sync::oneshot::Receiver<String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    queue.submit(crate::supervisor::escalation::EscalationRequest {
        id: id.to_string(),
        from_agent_id: format!("agent-{id}"),
        from_agent_name: "reviewer".to_string(),
        question: question.to_string(),
        options: None,
        reply_tx,
    });
    reply_rx
}

fn last_warning(harness: &Harness) -> Option<String> {
    harness
        .app
        .transcript
        .cells()
        .iter()
        .rev()
        .find_map(|cell| match cell {
            Cell::Notice {
                kind: NoticeKind::ToolWarning,
                text,
                ..
            } => Some(text.clone()),
            _ => None,
        })
}

/// The UI repaints every frame, so an asker that stops waiting while
/// its question is open in the modal is usually seen by a repaint
/// before the answer is given. The answer must still say the reply
/// was not delivered, not that it was already given.
#[test]
#[serial]
fn an_answer_after_a_repaint_saw_the_asker_go_says_not_delivered() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let reply = escalate_as(&queue, "esc_p1d", "Which tag? p1d");
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    harness.press(KeyCode::Enter);
    harness.draw();
    drop(reply);
    harness.draw();
    harness.draw();
    harness.type_text("v1");
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Enter);
    harness.draw();
    let warning = last_warning(&harness);
    assert!(
        warning
            .as_deref()
            .is_some_and(|text| text.contains("not delivered")),
        "{warning:?}"
    );
}

/// Of two questions, the one whose asker is gone has no row; the
/// live one is listed and Enter answers that one.
#[test]
#[serial]
fn only_the_live_question_is_listed_and_answered() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let dead = escalate_as(&queue, "esc_d2d", "Dead asks d2d");
    let mut live = escalate_as(&queue, "esc_l2l", "Live asks l2l");
    drop(dead);
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness.app.handle_key(alt(KeyCode::Char('t')));
    let frame = harness.draw();
    assert!(frame.contains("Live asks l2l"), "{frame}");
    assert!(!frame.contains("d2d"), "{frame}");
    harness.press(KeyCode::Enter);
    harness.type_text("ok");
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Enter);
    assert_eq!(live.try_recv().as_deref(), Ok("ok"));
    assert!(!queue.has_pending());
    assert!(queue.pending_summary().is_empty());
}

/// A focused question whose asker goes takes the focus with it: the
/// row the focus would slide onto by index is not acted on.
#[test]
#[serial]
fn a_focused_question_that_dies_clears_the_focus_not_slides_it() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    let first = escalate_as(&queue, "esc_a3a", "First asks a3a");
    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    let mut second = escalate_as(&queue, "esc_b3b", "Second asks b3b");
    let frame = harness.draw();
    assert!(frame.contains("a3a") && frame.contains("b3b"), "{frame}");
    drop(first);
    let frame = harness.draw();
    assert!(!frame.contains("a3a"), "{frame}");
    harness.press(KeyCode::Enter);
    assert!(
        harness.app.prompts.is_empty(),
        "Enter acted on a row the user never picked"
    );
    assert!(second.try_recv().is_err());
}
