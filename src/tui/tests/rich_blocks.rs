use super::mouse_and_copy::{left_drag, row_with};
use super::*;
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
