use super::mouse_and_copy::{left_drag, row_with};
use super::*;
use crate::tui::rich::math_render::FakeMath;
use crate::tui::rich::mermaid::{FakeMermaid, MermaidError};

const FLOWCHART_TD: &str = "flowchart TD\n    A[Start] --> B[Ship]";
const FLOWCHART_LR: &str = "flowchart LR\n    A[Start] --> B[Build] --> C[Ship]";
const SEQUENCE: &str = "sequenceDiagram\n    Alice->>Bob: Hi\n    Bob-->>Alice: Hello";

fn fenced(body: &str) -> String {
    format!("```mermaid\n{body}\n```")
}

fn reply_with(body: &str) -> String {
    format!("Here it is:\n{}\nThat is all.", fenced(body))
}

fn replied(harness: &mut Harness, reply: &str) -> String {
    harness.app.begin_turn("draw it", false);
    harness
        .app
        .handle_msg(UiMsg::AssistantText(reply.to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw()
}

/// A harness whose mermaid backend draws `drawn` for every diagram.
fn faked(drawn: &'static str) -> (Harness, Arc<FakeMermaid>) {
    let mut harness = Harness::standalone(80, 24);
    let fake = Arc::new(FakeMermaid::new(move |_, _| Ok(drawn.to_string())));
    harness.app.renderer.set_mermaid_backend(fake.clone());
    (harness, fake)
}

const DRAWN: &str = "┌───────┐\n│ Start │\n└───────┘";

/// The crate draws flowcharts both ways and a sequence diagram in the
/// transcript, at a wide and a narrow screen.
#[test]
fn mermaid_diagrams_are_drawn_in_the_transcript() {
    for (name, body) in [
        ("flowchart_td", FLOWCHART_TD),
        ("flowchart_lr", FLOWCHART_LR),
        ("sequence", SEQUENCE),
    ] {
        for (width, height) in [(80, 24), (40, 20)] {
            let mut harness = Harness::standalone(width, height);
            let frame = replied(&mut harness, &reply_with(body));
            assert!(
                !frame.contains("```mermaid"),
                "{name} {width}x{height}:\n{frame}"
            );
            assert!(
                !frame.contains("showing source"),
                "{name} {width}x{height}:\n{frame}"
            );
            insta::assert_snapshot!(format!("mermaid_{name}_{width}x{height}"), frame);
        }
    }
}

/// A diagram the crate does not support shows its source with the note.
#[test]
fn an_unsupported_diagram_shows_its_source_and_a_note() {
    let mut harness = Harness::standalone(80, 24);
    let frame = replied(&mut harness, &reply_with("zenuml\n    Alice.hello()"));
    assert!(frame.contains("```mermaid"), "{frame}");
    assert!(
        frame.contains("mermaid: unsupported diagram \"zenuml\" (showing source)"),
        "{frame}"
    );
    insta::assert_snapshot!("mermaid_fallback_note", frame);
    let mut narrow = Harness::standalone(40, 20);
    let frame = replied(&mut narrow, &reply_with("zenuml\n    Alice.hello()"));
    insta::assert_snapshot!("mermaid_fallback_note_40x20", frame);
}

/// A drag over the source rows of a diagram that could not be drawn
/// copies its fenced source, the note left out.
#[test]
fn a_drag_over_a_fallback_diagram_copies_its_source() {
    let mut harness = Harness::standalone(80, 24);
    let body = "zenuml\n    Alice.hello()";
    replied(&mut harness, &reply_with(body));
    let row = row_with(&harness, "Alice.hello()");
    let note = row_with(&harness, "showing source");
    assert_eq!(
        left_drag(&mut harness, (6, row), (9, note)),
        Some(Command::Copy {
            text: fenced(body),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );
}

/// Alt+M shows every diagram as its source and a second press draws it
/// again.
#[test]
fn alt_m_toggles_between_the_diagram_and_its_source() {
    let (mut harness, _) = faked(DRAWN);
    let frame = replied(&mut harness, &reply_with(FLOWCHART_TD));
    assert!(frame.contains("│ Start │"), "{frame}");
    assert!(!frame.contains("flowchart TD"), "{frame}");

    assert_eq!(harness.app.handle_key(alt(KeyCode::Char('m'))), None);
    let frame = harness.draw();
    assert!(frame.contains("flowchart TD"), "{frame}");
    assert!(!frame.contains("│ Start │"), "{frame}");

    harness.app.handle_key(alt(KeyCode::Char('m')));
    let frame = harness.draw();
    assert!(frame.contains("│ Start │"), "{frame}");
    assert!(!frame.contains("flowchart TD"), "{frame}");
}

/// A drag over part of a diagram copies its whole fenced source, and the
/// prose around it as drawn.
#[test]
fn a_drag_over_a_diagram_copies_its_source() {
    let (mut harness, _) = faked(DRAWN);
    replied(&mut harness, &reply_with(FLOWCHART_TD));
    let start = row_with(&harness, "│ Start │");
    assert_eq!(
        left_drag(&mut harness, (3, start), (6, start)),
        Some(Command::Copy {
            text: fenced(FLOWCHART_TD),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );
    let above = row_with(&harness, "Here it is:");
    let below = row_with(&harness, "That is all.");
    let Some(Command::Copy { text, .. }) = left_drag(&mut harness, (0, above), (11, below)) else {
        panic!("a drag copies");
    };
    assert_eq!(
        text,
        format!("Here it is:\n{}\nThat is all.", fenced(FLOWCHART_TD))
    );
}

/// The transcript view shows a reply's diagrams as source.
#[test]
fn the_transcript_view_shows_the_source() {
    let (mut harness, _) = faked(DRAWN);
    replied(&mut harness, &reply_with(FLOWCHART_TD));
    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    let frame = harness.draw();
    assert!(frame.contains("flowchart TD"), "{frame}");
    assert!(!frame.contains("│ Start │"), "{frame}");
}

/// A closed diagram in a reply still streaming is drawn once, though the
/// tail it sits in is rendered on every frame.
#[test]
fn a_diagram_in_a_streaming_reply_is_drawn_once() {
    let (mut harness, fake) = faked(DRAWN);
    harness.app.begin_turn("draw it", false);
    let start = format!("Intro\n\n{}\nstill", fenced(FLOWCHART_TD));
    harness.app.handle_msg(UiMsg::TextDelta(start));
    let frame = harness.draw();
    assert!(frame.contains("│ Start │"), "{frame}");
    harness
        .app
        .handle_msg(UiMsg::TextDelta(" going".to_string()));
    let frame = harness.draw();
    assert!(frame.contains("still going"), "{frame}");
    assert_eq!(fake.calls(), 1);
}

/// A panic in the backend shows the source with the note and leaves the
/// UI running.
#[test]
fn a_panicking_backend_shows_the_source() {
    let mut harness = Harness::standalone(80, 24);
    let fake = Arc::new(FakeMermaid::new(|_, _| -> Result<String, MermaidError> {
        panic!("layout overflowed")
    }));
    harness.app.renderer.set_mermaid_backend(fake);
    let frame = replied(&mut harness, &reply_with(FLOWCHART_TD));
    assert!(frame.contains("flowchart TD"), "{frame}");
    assert!(
        frame.contains("mermaid: renderer failed: layout overflowed (showing source)"),
        "{frame}"
    );
}

const LINE_CHART: &str = "type: line\ntitle: Requests\nx_label: minute\ny_label: req/s\nseries:\n  \
                          - name: api\n    data: [[0, 2], [1, 5], [2, 3], [3, 8], [4, 6]]\n  \
                          - name: web\n    data: [1, 2, 2, 4, 3]";
const BAR_CHART: &str = "type: bar\ntitle: Languages\nbars:\n  - {label: rust, value: 7}\n  \
                         - {label: go, value: 4}\n  - {label: py, value: 5}";
const SPARKLINE: &str =
    r#"{"type": "sparkline", "title": "Latency", "data": [3, 5, 2, 8, 6, 9, 4, 7, 5, 6]}"#;

fn chart_fenced(body: &str) -> String {
    format!("```chart\n{body}\n```")
}

fn chart_reply(body: &str) -> String {
    format!("Here it is:\n{}\nThat is all.", chart_fenced(body))
}

/// The rows of the last frame between the reply's lead line and its last.
fn chart_rows(harness: &Harness) -> std::ops::Range<u16> {
    row_with(harness, "Here it is:") + 1..row_with(harness, "That is all.")
}

/// Line, bar and sparkline charts are drawn in the transcript at a wide
/// and a narrow screen, the sparkline from a JSON spec. Colour is set
/// rather than taken from the test's stdout, since it picks the markers.
#[test]
fn charts_are_drawn_in_the_transcript() {
    for (name, body, title) in [
        ("line", LINE_CHART, "Requests"),
        ("bar", BAR_CHART, "Languages"),
        ("sparkline", SPARKLINE, "Latency"),
    ] {
        for (width, height) in [(80, 24), (40, 20)] {
            let mut harness = Harness::standalone(width, height);
            harness.app.set_no_color(false);
            let frame = replied(&mut harness, &chart_reply(body));
            let at = format!("{name} {width}x{height}:\n{frame}");
            assert!(width < 80 || frame.contains(title), "{at}");
            assert!(!frame.contains("```chart"), "{at}");
            assert!(!frame.contains("showing source"), "{at}");
            insta::assert_snapshot!(format!("chart_{name}_{width}x{height}"), frame);
        }
    }
}

/// A spec that does not check out shows its source with the note.
#[test]
fn an_invalid_chart_shows_its_source_and_a_note() {
    let mut harness = Harness::standalone(80, 24);
    let frame = replied(&mut harness, &chart_reply("type: line\nseries: []"));
    assert!(frame.contains("```chart"), "{frame}");
    assert!(
        frame.contains("chart: invalid spec: no series (showing source)"),
        "{frame}"
    );
    insta::assert_snapshot!("chart_invalid_fallback", frame);
}

/// The chart is painted in the theme's colours, and under `NO_COLOR` in
/// none, its legend still telling the series apart.
#[test]
fn under_no_color_a_chart_paints_no_colour() {
    for no_color in [false, true] {
        let mut harness = Harness::standalone(80, 24);
        harness.app.set_no_color(no_color);
        let frame = replied(&mut harness, &chart_reply(LINE_CHART));
        let buffer = harness.terminal.backend().buffer();
        let coloured = chart_rows(&harness).any(|y| {
            (0..buffer.area.width).any(|x| {
                let cell = &buffer[(x, y)];
                cell.fg != Color::Reset || cell.bg != Color::Reset
            })
        });
        assert_eq!(coloured, !no_color, "{frame}");
        assert!(frame.contains("api") && frame.contains("web"), "{frame}");
    }
}

/// A theme change lays out again a chart already on screen, so turning
/// colour off takes its colours away at once.
#[test]
fn a_new_theme_repaints_a_chart_on_screen() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(false);
    replied(&mut harness, &chart_reply(LINE_CHART));
    let coloured = |harness: &Harness| {
        let buffer = harness.terminal.backend().buffer();
        chart_rows(harness)
            .any(|y| (0..buffer.area.width).any(|x| buffer[(x, y)].fg != Color::Reset))
    };
    assert!(coloured(&harness));
    harness.app.set_no_color(true);
    harness.draw();
    assert!(!coloured(&harness));
}

/// Alt+M shows a chart as its source, the transcript view shows the
/// source, and a drag over the chart copies its fenced source.
#[test]
fn a_chart_shows_and_copies_as_its_source() {
    let mut harness = Harness::standalone(80, 24);
    let body = format!("{BAR_CHART}\nheight: 8");
    let frame = replied(&mut harness, &chart_reply(&body));
    assert!(!frame.contains("type: bar"), "{frame}");
    let title = row_with(&harness, "Languages");
    assert_eq!(
        left_drag(&mut harness, (3, title), (6, title)),
        Some(Command::Copy {
            text: chart_fenced(&body),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );

    harness.app.handle_key(alt(KeyCode::Char('m')));
    let frame = harness.draw();
    assert!(frame.contains("```chart"), "{frame}");
    assert!(frame.contains("type: bar"), "{frame}");
    harness.app.handle_key(alt(KeyCode::Char('m')));

    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    let frame = harness.draw();
    assert!(frame.contains("type: bar"), "{frame}");
}

/// A config reload that changes only `tui_chart_height` redraws a chart
/// already on screen at the new height.
#[test]
#[serial]
fn a_reloaded_chart_height_redraws_the_chart() {
    let _guard = TestConfigDirGuard::new("tui-chart-height-reload");
    let mut ctx = chat_ctx();
    let mut harness = Harness::for_ctx(&ctx);
    harness.resize(80, 48);
    harness.app.sync_from_ctx(&mut ctx);
    replied(&mut harness, &chart_reply(SPARKLINE));
    assert_eq!(chart_rows(&harness).len(), 12);

    let mut app = (*ctx.app).clone();
    app.config = Arc::new(AppConfig {
        tui_chart_height: 20,
        ..(*app.config).clone()
    });
    ctx.app = Arc::new(app);
    harness.app.sync_from_ctx(&mut ctx);
    harness.draw();
    assert_eq!(chart_rows(&harness).len(), 20);
}

const FRACTION: &str = "$$\\frac{a+b}{c}$$";
const SUM: &str = "$$\\sum_{i=1}^{n} i^2$$";
const MATRIX: &str = "$$\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}$$";
const CASES: &str = "$$\nf(x) = \\begin{cases} x & x > 0 \\\\ -x & x \\le 0 \\end{cases}\n$$";

fn math_reply(formula: &str) -> String {
    format!("Here it is:\n{formula}\nThat is all.")
}

/// A fraction, a sum with limits, a matrix and cases are drawn by the
/// crate in the transcript, at a wide and a narrow screen.
#[test]
fn display_math_is_drawn_in_the_transcript() {
    for (name, formula) in [
        ("fraction", FRACTION),
        ("sum", SUM),
        ("matrix", MATRIX),
        ("cases", CASES),
    ] {
        for (width, height) in [(80, 24), (40, 20)] {
            let mut harness = Harness::standalone(width, height);
            let frame = replied(&mut harness, &math_reply(formula));
            let at = format!("{name} {width}x{height}:\n{frame}");
            assert!(!frame.contains("$$"), "{at}");
            assert!(!frame.contains("showing source"), "{at}");
            insta::assert_snapshot!(format!("math_{name}_{width}x{height}"), frame);
        }
    }
}

const INLINE: &str = "the value $x^2 + y^2$ here";

/// An inline formula that draws on one row is drawn in its sentence, and
/// a drag over the sentence copies the formula's source.
#[test]
fn inline_math_is_drawn_in_its_sentence_and_copies_as_source() {
    let mut harness = Harness::standalone(80, 24);
    let frame = replied(&mut harness, INLINE);
    assert!(frame.contains("the value x² + y² here"), "{frame}");
    insta::assert_snapshot!("math_inline_80x24", frame);
    let row = row_with(&harness, "the value");
    assert_eq!(
        left_drag(&mut harness, (0, row), (79, row)),
        Some(Command::Copy {
            text: INLINE.to_string(),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );
}

/// A construct the crate draws wrongly shows its source with the note.
#[test]
fn display_math_that_cannot_be_drawn_shows_its_source_and_a_note() {
    let mut harness = Harness::standalone(80, 24);
    let formula = "$$\\begin{align*} a &= b \\\\ c &= d \\end{align*}$$";
    let frame = replied(&mut harness, &math_reply(formula));
    assert!(frame.contains("\\begin{align*}"), "{frame}");
    assert!(
        frame.contains("math: \\begin{align*} is not supported (showing source)"),
        "{frame}"
    );
    insta::assert_snapshot!("math_fallback_80x24", frame);
}

/// A `*` in a formula that cannot be drawn is shown as written, not read
/// as emphasis, and a drag over the source copies the formula.
#[test]
fn a_formula_shown_as_source_keeps_its_stars_and_copies_as_written() {
    let mut harness = Harness::standalone(80, 24);
    let formula = "$$\\begin{align*} a *b* &= c_1 * d_2 \\end{align*}$$";
    let frame = replied(&mut harness, &math_reply(formula));
    assert!(frame.contains("a *b* &= c_1 * d_2"), "{frame}");
    insta::assert_snapshot!("math_fallback_with_star_80x24", frame);
    let row = row_with(&harness, "align*} a");
    assert_eq!(
        left_drag(&mut harness, (3, row), (8, row)),
        Some(Command::Copy {
            text: formula.to_string(),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );
}

/// An inline formula that cannot be drawn on one row shows as written in
/// its sentence, its `*` and `_` not read as emphasis, and a drag over
/// the sentence copies it as written.
#[test]
fn an_inline_formula_shown_as_source_keeps_its_stars_and_copies_as_written() {
    let mut harness = Harness::standalone(80, 24);
    let reply = "so $\\frac{1}{2} *b* c_1 _d_$ holds";
    let frame = replied(&mut harness, reply);
    assert!(frame.contains(reply), "{frame}");
    let row = row_with(&harness, "so $");
    assert_eq!(
        left_drag(&mut harness, (0, row), (79, row)),
        Some(Command::Copy {
            text: reply.to_string(),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );

    let mut harness = Harness::standalone(40, 20);
    let long = "so $\\frac{1}{2} *b* c_1 _d_ + \\frac{3}{4} *e* f_1 _g_ + h_2$ holds";
    let frame = replied(&mut harness, long);
    insta::assert_snapshot!("math_inline_fallback_wrapped_40x20", frame);
    let first = row_with(&harness, "$\\frac") - 1;
    assert_eq!(
        left_drag(&mut harness, (0, first), (39, first + 2)),
        Some(Command::Copy {
            text: long.to_string(),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );
}

/// An inline formula is drawn once per width however many frames its
/// streaming tail takes, and so is one whose drawing failed.
#[test]
fn an_inline_formula_in_a_streaming_reply_is_drawn_once() {
    for (drawn, shown) in [("DRAWN", "so DRAWN still"), ("a\nb", "so $x^2$ still")] {
        let mut harness = Harness::standalone(80, 24);
        let fake = Arc::new(FakeMath::new(move |_| drawn.to_string()));
        harness.app.renderer.set_math_backend(fake.clone());
        harness.app.begin_turn("draw it", false);
        harness
            .app
            .handle_msg(UiMsg::TextDelta("so $x^2$ still".to_string()));
        let frame = harness.draw();
        assert!(frame.contains(shown), "{frame}");
        harness
            .app
            .handle_msg(UiMsg::TextDelta(" going".to_string()));
        let frame = harness.draw();
        assert!(frame.contains("still going"), "{frame}");
        assert_eq!(fake.calls(), 1, "{drawn:?}");
    }
}

/// Alt+M shows display and inline math as source, the transcript view
/// shows the source, and a drag over a drawn formula copies its source.
#[test]
fn math_shows_and_copies_as_its_source() {
    let mut harness = Harness::standalone(80, 24);
    let reply = format!("{}\n{INLINE}", math_reply(FRACTION));
    let frame = replied(&mut harness, &reply);
    assert!(!frame.contains("\\frac"), "{frame}");
    let bar = row_with(&harness, "───");
    assert_eq!(
        left_drag(&mut harness, (3, bar), (5, bar)),
        Some(Command::Copy {
            text: FRACTION.to_string(),
            from: "the selection".to_string(),
            dropped: 0,
        })
    );

    harness.app.handle_key(alt(KeyCode::Char('m')));
    let frame = harness.draw();
    assert!(frame.contains("\\frac{a+b}{c}"), "{frame}");
    assert!(frame.contains("$x^2 + y^2$"), "{frame}");
    assert!(!frame.contains("a + b") && !frame.contains('²'), "{frame}");
    harness.app.handle_key(alt(KeyCode::Char('m')));
    let frame = harness.draw();
    assert!(frame.contains("x² + y²"), "{frame}");

    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    let frame = harness.draw();
    assert!(frame.contains("\\frac{a+b}{c}"), "{frame}");
    assert!(frame.contains("$x^2 + y^2$"), "{frame}");
}

/// Display math in a streaming reply is drawn once its closer arrives,
/// and the backend is asked once however many frames the tail takes.
#[test]
fn display_math_in_a_streaming_reply_is_drawn_once_closed() {
    let mut harness = Harness::standalone(80, 24);
    let fake = Arc::new(FakeMath::new(|_| "DRAWN".to_string()));
    harness.app.renderer.set_math_backend(fake.clone());
    harness.app.begin_turn("draw it", false);
    harness
        .app
        .handle_msg(UiMsg::TextDelta("Intro\n\n$$\n\\frac{a}{b}".to_string()));
    let frame = harness.draw();
    assert!(frame.contains("\\frac{a}{b}"), "{frame}");
    assert!(!frame.contains("DRAWN"), "{frame}");
    harness
        .app
        .handle_msg(UiMsg::TextDelta("\n$$\nstill".to_string()));
    let frame = harness.draw();
    assert!(frame.contains("DRAWN"), "{frame}");
    harness
        .app
        .handle_msg(UiMsg::TextDelta(" going".to_string()));
    let frame = harness.draw();
    assert!(frame.contains("still going"), "{frame}");
    assert_eq!(fake.calls(), 1);
}
