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
