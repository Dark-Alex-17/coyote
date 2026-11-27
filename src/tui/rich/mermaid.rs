//! Mermaid diagrams drawn in box-drawing characters. The drawing is done
//! by a backend behind [`MermaidRenderer`]; [`render_mermaid`] guards
//! every backend alike: it caps the source and the size of the graph,
//! survives a panic, scrubs the output and refuses a drawing wider than
//! the room it is given.

use crate::tui::render::plain_text;
use crate::tui::terminal::catch_quietly;
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

/// The most source a diagram may have before it is shown as source
/// without trying to draw it.
const MAX_SOURCE_BYTES: usize = 16 * 1024;
const MAX_SOURCE_LINES: usize = 400;

/// The largest graph handed to the backend, whose layout runs on the draw
/// path and grows far faster than the graph: in a debug build a random
/// flowchart at these caps lays out in well under a second, where one of
/// 30 nodes and 40 edges with subgraphs took several seconds. Class
/// diagrams lay out slower still, so they have caps of their own.
const MAX_NODES: usize = 24;
const MAX_EDGES: usize = 24;
const MAX_SUBGRAPHS: usize = 4;
const MAX_CLASS_NODES: usize = 16;
const MAX_CLASS_EDGES: usize = 16;

/// Diagram kinds drawn without a graph layout, which stay fast at the
/// source caps whatever they hold, in lower case as the backend matches
/// them whatever their case. Every other kind is counted.
const UNLAYERED_KINDS: [&str; 5] = ["sequencediagram", "gantt", "timeline", "journey", "pie"];
const CLASS_DIAGRAM: &str = "classdiagram";

/// Statements that declare no node of their own, skipped by the count.
const NON_NODE_STATEMENTS: [&str; 22] = [
    "classDef",
    "style",
    "linkStyle",
    "click",
    "direction",
    "title",
    "accTitle",
    "accDescr",
    "note",
    "Note",
    "loop",
    "alt",
    "else",
    "opt",
    "par",
    "and",
    "rect",
    "critical",
    "break",
    "end",
    "activate",
    "deactivate",
];

/// Statements whose next word alone names a node.
const DECLARING_STATEMENTS: [&str; 4] = ["participant", "actor", "class", "state"];

/// Draws a mermaid diagram as text in at most `max_width` columns.
pub(crate) trait MermaidRenderer: Send + Sync {
    fn render(&self, source: &str, max_width: u16) -> Result<String, MermaidError>;
}

/// Why a diagram is shown as source instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MermaidError {
    Empty,
    Unsupported(String),
    Parse(String),
    TooWide { needed: usize, available: u16 },
    Panicked(String),
    TooLarge,
    TooComplex,
}

impl MermaidError {
    /// The one-line note shown under the source.
    pub(crate) fn note(&self) -> String {
        let why = match self {
            MermaidError::Empty => "empty diagram".to_string(),
            MermaidError::Unsupported(kind) => format!("unsupported diagram \"{kind}\""),
            MermaidError::Parse(message) => format!("parse error: {}", first_line(message)),
            MermaidError::TooWide { needed, available } => {
                format!("diagram needs {needed} columns, {available} available")
            }
            MermaidError::Panicked(message) => {
                format!("renderer failed: {}", first_line(message))
            }
            MermaidError::TooLarge => format!(
                "diagram longer than {MAX_SOURCE_LINES} lines or {} KiB",
                MAX_SOURCE_BYTES / 1024
            ),
            MermaidError::TooComplex => "diagram too complex to draw".to_string(),
        };
        format!("mermaid: {why} (showing source)")
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

/// Whether the graph in `source` is over the caps, its nodes, edges and
/// subgraphs counted roughly from the text: a node is any distinct word
/// outside labels, and a run of link characters such as `-->`, `-.-` or
/// `<|--` is an edge from each node on its left to each on its right, as
/// `A & B --> C & D` is four. A composite `state X {` counts as a
/// subgraph. The members in a class's `{` body are skipped, and a body
/// left open is too complex, as nothing after it would be counted. The
/// count errs high, so a diagram near the caps may be refused that the
/// backend would draw in time.
fn too_complex(source: &str) -> bool {
    let mut statements = source
        .lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("%%"));
    let Some(header) = statements.next() else {
        return false;
    };
    let kind = header
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if UNLAYERED_KINDS.contains(&kind.as_str()) {
        return false;
    }
    let class_diagram = kind == CLASS_DIAGRAM;
    let (max_nodes, max_edges) = if class_diagram {
        (MAX_CLASS_NODES, MAX_CLASS_EDGES)
    } else {
        (MAX_NODES, MAX_EDGES)
    };
    let mut nodes: HashSet<String> = HashSet::new();
    let mut edges = 0;
    let mut subgraphs = 0;
    let mut in_class_body = false;
    for line in statements {
        if in_class_body {
            in_class_body = !line.contains('}');
            continue;
        }
        in_class_body = class_diagram && line.ends_with('{');
        let mut words = line.split_whitespace();
        let first = words.next().unwrap_or_default();
        if first == "subgraph" {
            subgraphs += 1;
        } else if DECLARING_STATEMENTS.contains(&first) {
            if first == "state" && line.ends_with('{') {
                subgraphs += 1;
            }
            let declared = words.next().into_iter().flat_map(identifiers);
            nodes.extend(declared.map(str::to_string));
        } else if !NON_NODE_STATEMENTS.contains(&first) {
            let bare = without_labels(line);
            let mut groups = vec![1usize];
            for run in bare.split(|c: char| c.is_whitespace() || is_word_char(c)) {
                if is_link(run) {
                    groups.push(1);
                } else if let Some(group) = groups.last_mut() {
                    *group += run.matches('&').count();
                }
            }
            edges += groups
                .windows(2)
                .map(|pair| pair[0] * pair[1])
                .sum::<usize>();
            nodes.extend(identifiers(&bare).map(str::to_string));
        }
    }
    in_class_body || nodes.len() > max_nodes || edges > max_edges || subgraphs > MAX_SUBGRAPHS
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_word_char(c))
        .filter(|word| !word.is_empty())
}

/// Whether a run of punctuation draws a link: two line characters in a
/// row, or a line character and an arrow head.
fn is_link(run: &str) -> bool {
    let line = |c: char| matches!(c, '-' | '=' | '.' | '~');
    let chars: Vec<char> = run.chars().collect();
    chars
        .windows(2)
        .any(|pair| (line(pair[0]) || pair[0] == '<') && (line(pair[1]) || pair[1] == '>'))
        && chars.iter().copied().any(line)
}

/// `line` up to any `:` label, with each quoted string, each bracketed
/// or piped label closed on the line and each `:::name` class suffix
/// left out. A suffix is no label: the line goes on past it.
fn without_labels(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut closer = None;
    let mut resume_at = 0;
    for (at, c) in line.char_indices() {
        if at < resume_at {
            continue;
        }
        if let Some(close) = closer {
            if c == close {
                closer = None;
            }
            continue;
        }
        let close = match c {
            '"' => '"',
            '[' => ']',
            '(' => ')',
            '{' => '}',
            '|' => '|',
            ':' if line[at..].starts_with(":::") => {
                let name = &line[at + 3..];
                let name_len = name.find(|c: char| !is_word_char(c)).unwrap_or(name.len());
                resume_at = at + 3 + name_len;
                out.push(' ');
                continue;
            }
            ':' => break,
            _ => {
                out.push(c);
                continue;
            }
        };
        if line[at + c.len_utf8()..].contains(close) {
            closer = Some(close);
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// The rows of `source` drawn by `backend` in at most `width` columns,
/// without control characters, trailing blanks or trailing blank rows. A
/// backend that panics, or returns rows wider than `width` (some diagram
/// kinds ignore the width they are given), fails like one that refused.
pub(crate) fn render_mermaid(
    backend: &dyn MermaidRenderer,
    source: &str,
    width: u16,
) -> Result<Vec<String>, MermaidError> {
    if source.len() > MAX_SOURCE_BYTES || source.lines().count() > MAX_SOURCE_LINES {
        return Err(MermaidError::TooLarge);
    }
    if too_complex(source) {
        return Err(MermaidError::TooComplex);
    }
    let source = source.replace('\t', " ");
    let drawn =
        catch_quietly(|| backend.render(&source, width)).map_err(MermaidError::Panicked)??;
    let mut rows: Vec<String> = plain_text(&drawn)
        .split('\n')
        .map(|row| {
            let row: String = row
                .chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .filter(|c| !c.is_control())
                .collect();
            row.trim_end().to_string()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    if rows.is_empty() {
        return Err(MermaidError::Empty);
    }
    let needed = rows.iter().map(|row| row.width()).max().unwrap_or(0);
    if needed > usize::from(width) {
        return Err(MermaidError::TooWide {
            needed,
            available: width,
        });
    }
    Ok(rows)
}

/// The `mermaid-text` crate, asked for plain box drawing with the width as
/// a hard limit.
pub(crate) struct MermaidTextBackend;

impl MermaidRenderer for MermaidTextBackend {
    fn render(&self, source: &str, max_width: u16) -> Result<String, MermaidError> {
        let options = mermaid_text::RenderOptions {
            max_width: Some(usize::from(max_width)),
            max_width_strict: true,
            color: false,
            ascii: false,
            ..Default::default()
        };
        mermaid_text::render_with_options(source, &options).map_err(|err| match err {
            mermaid_text::Error::EmptyInput => MermaidError::Empty,
            mermaid_text::Error::UnsupportedDiagram(kind) => MermaidError::Unsupported(kind),
            mermaid_text::Error::ParseError(message) => MermaidError::Parse(message),
            mermaid_text::Error::TooWide { actual, .. } => MermaidError::TooWide {
                needed: actual,
                available: max_width,
            },
        })
    }
}

#[cfg(test)]
type Respond = dyn Fn(&str, u16) -> Result<String, MermaidError> + Send + Sync;

/// A backend that answers with whatever `respond` gives and counts its
/// calls.
#[cfg(test)]
pub(crate) struct FakeMermaid {
    respond: Box<Respond>,
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeMermaid {
    pub(crate) fn new(
        respond: impl Fn(&str, u16) -> Result<String, MermaidError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            respond: Box::new(respond),
            calls: Default::default(),
        }
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl MermaidRenderer for FakeMermaid {
    fn render(&self, source: &str, max_width: u16) -> Result<String, MermaidError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (self.respond)(source, max_width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fails_with(error: MermaidError) -> FakeMermaid {
        FakeMermaid::new(move |_, _| Err(error.clone()))
    }

    #[test]
    fn each_refusal_of_the_backend_comes_back_as_is() {
        for error in [
            MermaidError::Empty,
            MermaidError::Unsupported("gantt".to_string()),
            MermaidError::Parse("line 2: expected arrow".to_string()),
            MermaidError::TooWide {
                needed: 90,
                available: 40,
            },
        ] {
            let backend = fails_with(error.clone());
            assert_eq!(render_mermaid(&backend, "x", 40), Err(error));
        }
    }

    #[test]
    fn a_panicking_backend_fails_with_its_message() {
        let backend = FakeMermaid::new(|_, _| panic!("layout overflowed"));
        assert_eq!(
            render_mermaid(&backend, "flowchart TD", 40),
            Err(MermaidError::Panicked("layout overflowed".to_string()))
        );
    }

    /// A row wider than the room fails even when the backend called it a
    /// success, since some diagram kinds ignore the width.
    #[test]
    fn rows_wider_than_the_room_fail_whatever_the_backend_said() {
        let backend = FakeMermaid::new(|_, _| Ok("x".repeat(41)));
        assert_eq!(
            render_mermaid(&backend, "sequenceDiagram", 40),
            Err(MermaidError::TooWide {
                needed: 41,
                available: 40,
            })
        );
    }

    #[test]
    fn too_much_source_is_not_handed_to_the_backend() {
        let backend = FakeMermaid::new(|_, _| Ok("drawn".to_string()));
        let long = "A-->B\n".repeat(MAX_SOURCE_LINES + 1);
        let big = "x".repeat(MAX_SOURCE_BYTES + 1);
        for source in [long, big] {
            assert_eq!(
                render_mermaid(&backend, &source, 80),
                Err(MermaidError::TooLarge)
            );
        }
        assert_eq!(backend.calls(), 0);
    }

    #[test]
    fn control_characters_and_trailing_blank_rows_are_dropped() {
        let backend =
            FakeMermaid::new(|_, _| Ok("\x1b[31m┌─┐\x1b[0m\r\n│\x07a│\n└─┘\n\n  \n".to_string()));
        assert_eq!(
            render_mermaid(&backend, "flowchart TD", 80),
            Ok(vec![
                "┌─┐".to_string(),
                "│a│".to_string(),
                "└─┘".to_string()
            ])
        );
        let blank = FakeMermaid::new(|_, _| Ok("\n \n".to_string()));
        assert_eq!(
            render_mermaid(&blank, "flowchart TD", 80),
            Err(MermaidError::Empty)
        );
    }

    #[test]
    fn every_note_is_one_line_naming_the_reason() {
        let notes = [
            (MermaidError::Empty, "empty diagram"),
            (
                MermaidError::Unsupported("gantt".to_string()),
                "unsupported diagram \"gantt\"",
            ),
            (
                MermaidError::Parse("bad arrow\nat line 3".to_string()),
                "parse error: bad arrow",
            ),
            (
                MermaidError::TooWide {
                    needed: 90,
                    available: 40,
                },
                "diagram needs 90 columns, 40 available",
            ),
            (
                MermaidError::Panicked("boom".to_string()),
                "renderer failed: boom",
            ),
            (
                MermaidError::TooLarge,
                "diagram longer than 400 lines or 16 KiB",
            ),
            (MermaidError::TooComplex, "diagram too complex to draw"),
        ];
        for (error, why) in notes {
            assert_eq!(error.note(), format!("mermaid: {why} (showing source)"));
        }
    }

    /// The real crate draws a flowchart within the width, and reports one
    /// it cannot fit as too wide rather than overflowing.
    #[test]
    fn the_crate_draws_within_the_width_or_says_it_cannot() {
        let rows = render_mermaid(&MermaidTextBackend, "flowchart LR\nA-->B", 40).unwrap();
        assert!(rows.iter().all(|row| row.width() <= 40), "{rows:?}");
        assert!(rows.concat().contains('A'), "{rows:?}");
        let wide = "flowchart LR\nAlpha-->Bravo-->Charlie-->Delta-->Echo-->Foxtrot";
        assert!(matches!(
            render_mermaid(&MermaidTextBackend, wide, 12),
            Err(MermaidError::TooWide { available: 12, .. })
        ));
        assert!(matches!(
            render_mermaid(&MermaidTextBackend, "zenuml\nA.b()", 80),
            Err(MermaidError::Unsupported(_))
        ));
    }

    /// A graph over the caps is refused before the backend sees it, by
    /// its nodes, its edges (a link joins each node on its left to each
    /// on its right), its subgraphs, also when written on one line or with
    /// `:::name` class suffixes, or for a class diagram by the tighter caps
    /// of its own.
    #[test]
    fn a_graph_too_complex_to_lay_out_is_not_handed_to_the_backend() {
        let backend = FakeMermaid::new(|_, _| Ok("drawn".to_string()));
        let chain = |kind: &str, links: usize| {
            let mut source = format!("{kind}\n");
            for n in 0..links {
                source.push_str(&format!("N{n}[Step {n}] --> N{}\n", n + 1));
            }
            source
        };
        let edges: String = std::iter::once("flowchart TD\n".to_string())
            .chain((0..25).map(|n| format!("A -->|go {n}| B\n")))
            .collect();
        let subgraphs: String = std::iter::once("flowchart TD\n".to_string())
            .chain((0..5).map(|n| format!("subgraph S{n}\nA{n}\nend\n")))
            .collect();
        let classes: String = std::iter::once("classDiagram\n".to_string())
            .chain((0..17).map(|n| format!("class C{n} {{\n  +String name\n}}\n")))
            .collect();
        let fan_out = "flowchart TD\nA & B & C & D & E --> F & G & H & I & J\n".to_string();
        let styled: String = std::iter::once("flowchart TD\nclassDef c fill:#f00\n".to_string())
            .chain((0..25).map(|n| format!("N{}:::c --> N{}:::c\n", n % 20, (n * 7) % 20)))
            .collect();
        let one_line = format!(
            "flowchart TD; {}",
            (0..5)
                .map(|n| format!("subgraph S{n}; A{n}; end"))
                .collect::<Vec<_>>()
                .join("; ")
        );
        let start = std::time::Instant::now();
        let refused = [chain("flowchart TD", 24), edges, subgraphs, classes];
        for source in refused.into_iter().chain([fan_out, styled, one_line]) {
            assert_eq!(
                render_mermaid(&backend, &source, 80),
                Err(MermaidError::TooComplex),
                "{source}"
            );
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(backend.calls(), 0);

        for source in [
            chain("flowchart TD", 23),
            chain("graph LR", 23),
            chain("classDiagram", 15).replace("-->", "<|--"),
        ] {
            assert!(render_mermaid(&backend, &source, 80).is_ok(), "{source}");
        }
        assert_eq!(backend.calls(), 3);
    }

    /// Labels, quoted text and member bodies are not nodes, and a diagram
    /// drawn without a graph layout is not counted at all.
    #[test]
    fn only_the_graph_is_counted() {
        let labelled: String =
            std::iter::once("flowchart TD\n".to_string())
                .chain((0..20).map(|n| {
                    format!("A[\"start {n}: go\"] -->|word{n} other{n}| B{{is it {n}?}}\n")
                }))
                .collect();
        assert!(!too_complex(&labelled), "{labelled}");
        let members: String = std::iter::once("classDiagram\n".to_string())
            .chain(
                (0..4).map(|n| format!("class C{n} {{\n  +a{n}\n  +b{n}\n  +c{n}\n  +d{n}\n}}\n")),
            )
            .collect();
        assert!(!too_complex(&members), "{members}");
        let messages: String = std::iter::once("sequenceDiagram\n".to_string())
            .chain((0..60).map(|n| format!("P{n}->>Q{n}: hello\n")))
            .collect();
        assert!(!too_complex(&messages));
    }

    /// The transitions inside a composite state are counted, the state
    /// itself counts as a subgraph, and a class body left open is too
    /// complex rather than hiding the rest of the diagram.
    #[test]
    fn a_state_body_is_counted_and_an_open_class_body_is_refused() {
        let transitions: String = (0..30)
            .map(|n| format!("  S{n} --> S{}\n", n + 1))
            .collect();
        let big_state = format!("stateDiagram-v2\nstate Big {{\n{transitions}}}\n");
        assert!(too_complex(&big_state), "{big_state}");
        let small_state = "stateDiagram-v2\nstate Big {\n  A --> B\n}\n[*] --> Big\n";
        assert!(!too_complex(small_state));
        let composites: String = std::iter::once("stateDiagram-v2\n".to_string())
            .chain((0..5).map(|n| format!("state C{n} {{\n  A{n} --> B{n}\n}}\n")))
            .collect();
        assert!(too_complex(&composites), "{composites}");

        let open_body = format!("classDiagram\nclass A {{\n  +a\n{transitions}");
        assert!(too_complex(&open_body), "{open_body}");
        let closed_inline = "classDiagram\nclass A {\n  +a\n  +b }\nA <|-- B\n";
        assert!(!too_complex(closed_inline));
    }

    /// The kind is read whatever its case, as the backend reads it.
    #[test]
    fn the_kind_is_read_whatever_its_case() {
        let classes = |kind: &str, count: usize| -> String {
            std::iter::once(format!("{kind}\n"))
                .chain((0..count).map(|n| format!("class C{n} {{\n  +a{n}\n  +b{n}\n}}\n")))
                .collect()
        };
        let messages = |kind: &str| -> String {
            std::iter::once(format!("{kind}\n"))
                .chain((0..60).map(|n| format!("P{n}->>Q{n}: hello\n")))
                .collect()
        };
        for (kind, class_kind) in [
            ("classDiagram", true),
            ("ClassDiagram", true),
            ("CLASSDIAGRAM", true),
            ("classdiagram", true),
            ("flowchart", false),
        ] {
            assert_eq!(
                too_complex(&classes(kind, MAX_CLASS_NODES)),
                !class_kind,
                "{kind}: members are skipped only in a class diagram"
            );
            assert!(too_complex(&classes(kind, MAX_CLASS_NODES + 1)), "{kind}");
        }
        for kind in ["sequenceDiagram", "SequenceDiagram", "SEQUENCEDIAGRAM"] {
            assert!(!too_complex(&messages(kind)), "{kind}");
        }
        assert!(too_complex(&messages("flowchart")));
    }

    /// An ordinary flowchart of eighteen nodes and a sequence diagram of
    /// ten messages are under the caps and drawn by the real crate.
    #[test]
    fn ordinary_diagrams_are_drawn_by_the_crate() {
        let flowchart = "flowchart TD\n\
            A[Start] --> B{Logged in?}\n\
            B -->|Yes| C[Dashboard]\n\
            B -->|No| D[Login]\n\
            D --> E[Check password]\n\
            E -->|ok| C\n\
            E -->|bad| F[Show error]\n\
            F --> D\n\
            C --> G[Load profile]\n\
            G --> H[Load feed]\n\
            H --> I[Render]\n\
            I --> J{More?}\n\
            J -->|Yes| K[Fetch page]\n\
            K --> H\n\
            J -->|No| L[Idle]\n\
            L --> M[Timeout]\n\
            M --> N[Logout]\n\
            N --> O[Clear cache]\n\
            O --> P[Goodbye]\n\
            P --> Q[End]\n\
            Q --> R[Exit]\n";
        let sequence = "sequenceDiagram\n\
            participant U as User\n\
            participant S as Server\n\
            participant D as Database\n\
            U->>S: login\n\
            S->>D: find user\n\
            D-->>S: user\n\
            S-->>U: token\n\
            U->>S: list\n\
            S->>D: query\n\
            D-->>S: rows\n\
            S-->>U: page\n\
            U->>S: logout\n\
            S-->>U: bye\n";
        for source in [flowchart, sequence] {
            assert!(!too_complex(source), "{source}");
            let rows = render_mermaid(&MermaidTextBackend, source, 120);
            assert!(rows.is_ok(), "{source}: {rows:?}");
        }
    }

    /// A graph whose edges join nodes picked at random, the shape that
    /// lays out slowest: `nodes` nodes, `edges` edges, each of the first
    /// `subgraphs` subgraphs holding a few nodes.
    fn random_graph(kind: &str, nodes: usize, edges: usize, subgraphs: usize) -> String {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move |below: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % below as u64).unwrap_or(0)
        };
        let mut source = format!("{kind}\n");
        for group in 0..subgraphs {
            source.push_str(&format!("subgraph S{group}\n"));
            for n in (group..nodes).step_by(subgraphs.max(1)).take(3) {
                source.push_str(&format!("N{n}[Node {n}]\n"));
            }
            source.push_str("end\n");
        }
        for n in 0..nodes.min(edges) {
            source.push_str(&format!("N{n} --> N{}\n", (n + 1) % nodes));
        }
        for _ in nodes.min(edges)..edges {
            source.push_str(&format!("N{} --> N{}\n", next(nodes), next(nodes)));
        }
        source
    }

    /// The worst graphs under the caps the real crate is handed still lay
    /// out in about a second in a debug build, the slowest kind there is.
    #[test]
    fn the_crate_lays_out_the_largest_graphs_allowed_in_time() {
        let flowchart = random_graph("flowchart TD", MAX_NODES, MAX_EDGES, MAX_SUBGRAPHS);
        let classes = random_graph("classDiagram", MAX_CLASS_NODES, MAX_CLASS_EDGES, 0)
            .replace("-->", "<|--");
        let fan = format!(
            "flowchart TD\nN0 & N1 & N2 & N3 --> N4 & N5 & N6 & N7 & N8 & N9\n{}\n",
            (10..MAX_NODES)
                .map(|n| format!("N{n}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let dense = random_graph("flowchart LR", MAX_NODES, MAX_EDGES, MAX_SUBGRAPHS)
            .replace("[Node", "[A rather longer label for node");
        let composites: String = (0..MAX_SUBGRAPHS)
            .map(|group| format!("state C{group} {{\n  N{group} --> M{group}\n}}\n"))
            .collect();
        let states = format!(
            "stateDiagram-v2\n{composites}{}",
            random_graph(
                "",
                MAX_NODES - 3 * MAX_SUBGRAPHS,
                MAX_EDGES - MAX_SUBGRAPHS,
                0
            )
            .trim_start()
            .replace("N", "S")
        );
        for source in [flowchart, classes, fan, dense, states] {
            assert!(!too_complex(&source), "{source}");
            for width in [80, 200] {
                let start = std::time::Instant::now();
                let _ = render_mermaid(&MermaidTextBackend, &source, width);
                let took = start.elapsed();
                eprintln!("{} at width {width}: {took:?}", first_line(&source));
                assert!(
                    took < std::time::Duration::from_secs(2),
                    "{took:?}\n{source}"
                );
            }
        }
    }

    /// A tab in the source reaches the backend as a space, a tab in the
    /// drawing is a space, and trailing blanks do not count to the width.
    #[test]
    fn tabs_become_spaces_and_trailing_blanks_do_not_count() {
        let echo = FakeMermaid::new(|source, _| Ok(source.to_string()));
        assert_eq!(
            render_mermaid(&echo, "flowchart\tTD", 80),
            Ok(vec!["flowchart TD".to_string()])
        );
        let padded = FakeMermaid::new(|_, _| Ok("a\tb   \n".to_string()));
        assert_eq!(render_mermaid(&padded, "x", 3), Ok(vec!["a b".to_string()]));
    }
}
