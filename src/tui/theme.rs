//! The UI's own colours: the plain REPL's where it shows the same thing
//! (the tool call line, the completion menu, the typed command), the
//! transcript theme's for the rest, and fixed named colours when no theme
//! is loaded. Under `NO_COLOR` every style keeps its attributes and loses
//! its colours.

use crate::events::NoticeKind;
use crate::render::MarkdownRender;
use crate::utils::{tool_dim_color, tool_fn_color, tool_key_color};
use ratatui::backend::FromCrossterm;
use ratatui::style::{Color, Modifier, Style};

pub(super) const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const ITALIC: Style = Style::new().add_modifier(Modifier::ITALIC);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
/// What the REPL's selection prompts paint the highlighted option with.
const HIGHLIGHT: Color = Color::LightCyan;
/// The bright band of the welcome wordmark's startup sweep.
pub(super) const SWEEP_HIGHLIGHT: Color = Color::White;
const KNOWN_COMMAND: Color = Color::Green;
const UNKNOWN_COMMAND: Color = Color::Red;
const HEADING_FALLBACK: Color = Color::Yellow;
/// The separator rules: the brightest the terminal draws on each side of
/// the scale, in bold, so the regions they part read apart at a glance.
const RULE_DARK: Color = Color::White;
const RULE_LIGHT: Color = Color::Black;
/// The todo panel's own colours, kept apart from the hint and status
/// rows' so its items never read as either.
const TODO_OPEN: Color = Color::LightGreen;
const TODO_DONE: Color = Color::Gray;
/// The queued strip's header, apart from the footer's fields under it.
const QUEUED_HEADER: Color = Color::LightBlue;
/// The footer's ` . ` between its fields, apart from the dim queued
/// lines above the composer.
const FOOTER_SEPARATOR: Color = Color::DarkGray;

type Rgb = (u8, u8, u8);

/// The regions' backgrounds and the faded text drawn over them.
struct Palette {
    base: Rgb,
    raised: Rgb,
    chrome: Rgb,
    faded: Rgb,
    selection: Rgb,
}

/// One blue-grey hue; the light palette mirrors the dark one's lightness.
const DARK_PALETTE: Palette = Palette {
    base: (0x1e, 0x22, 0x30),
    raised: (0x2a, 0x30, 0x42),
    chrome: (0x16, 0x19, 0x22),
    faded: (0x9a, 0xa5, 0xbf),
    selection: (0x3b, 0x4a, 0x6b),
};
const LIGHT_PALETTE: Palette = Palette {
    base: (0xec, 0xef, 0xf5),
    raised: (0xdd, 0xe2, 0xec),
    chrome: (0xf6, 0xf7, 0xfa),
    faded: (0x56, 0x60, 0x7a),
    selection: (0xc5, 0xd3, 0xec),
};

/// The colours a sub-session's state is drawn in.
struct ThreadPalette {
    running: Rgb,
    completed: Rgb,
    failed: Rgb,
}

const DARK_THREADS: ThreadPalette = ThreadPalette {
    running: (0xff, 0xa6, 0x57),
    completed: (0x7e, 0xe7, 0x87),
    failed: (0xff, 0x7b, 0x72),
};
const LIGHT_THREADS: ThreadPalette = ThreadPalette {
    running: (0x9a, 0x4f, 0x00),
    completed: (0x11, 0x63, 0x29),
    failed: (0xb3, 0x26, 0x1e),
};

/// How a list's columns and its highlighted row are painted.
pub(super) struct RowStyles {
    pub(super) name: Style,
    pub(super) kind: Style,
    pub(super) description: Style,
    pub(super) highlight: Style,
}

pub(super) const PLAIN_ROWS: RowStyles = RowStyles {
    name: Style::new(),
    kind: DIM,
    description: DIM,
    highlight: Style::new().add_modifier(Modifier::REVERSED),
};

/// The completion popup in the REPL's colours: command names green as its
/// highlighter paints a known command, descriptions yellow and the
/// selected row green, bold and reversed as its completion menu draws them.
const PALETTE_ROWS: RowStyles = RowStyles {
    name: Style::new().fg(Color::Green),
    kind: Style::new().fg(Color::Cyan),
    description: Style::new().fg(Color::Yellow),
    highlight: Style::new()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::REVERSED),
};

/// The UI's own fixed colours. `no_color` keeps the attributes and drops
/// the colour, which is what `NO_COLOR` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChromeKind {
    Notice(NoticeKind),
    Error,
}

pub(super) fn chrome_style(kind: ChromeKind, no_color: bool) -> Style {
    let (colour, style) = match kind {
        ChromeKind::Notice(NoticeKind::ToolWarning) => (Some(Color::Yellow), Style::new()),
        ChromeKind::Notice(NoticeKind::Routing) => (None, Style::new()),
        ChromeKind::Notice(_) => (None, DIM),
        ChromeKind::Error => (Some(Color::Red), Style::new()),
    };
    match colour {
        Some(colour) if !no_color => style.fg(colour),
        _ => style,
    }
}

/// A notice row's style: a warning or a routing line as the chrome draws
/// it, every other notice in `faded`.
pub(super) fn notice_style(kind: NoticeKind, faded: Style, no_color: bool) -> Style {
    match kind {
        NoticeKind::ToolWarning | NoticeKind::Routing => {
            chrome_style(ChromeKind::Notice(kind), no_color)
        }
        _ => faded,
    }
}

/// A tool row's header in the colours of the REPL's `Call <tool>` line:
/// the name as the REPL paints it, the arrow, arguments and state in its
/// muted colour, a failure in the error colour, a running row's throbber
/// in the status row's.
pub(super) struct ToolHeaderStyles {
    pub(super) muted: Style,
    pub(super) name: Style,
    pub(super) failed: Style,
    pub(super) throbber: Style,
}

impl ToolHeaderStyles {
    pub(super) fn new(markdown: &MarkdownRender, no_color: bool) -> Self {
        let paint = |style: Style, colour: Color| {
            if no_color { style } else { style.fg(colour) }
        };
        let tool_fn = from_repl(tool_fn_color(), markdown);
        Self {
            muted: paint(DIM, from_repl(tool_dim_color(), markdown)),
            name: paint(BOLD, tool_fn),
            failed: chrome_style(ChromeKind::Error, no_color),
            throbber: paint(Style::new(), tool_fn),
        }
    }
}

/// The greeting's wordmark colour: the heading colour; the wordmark adds
/// its own weight.
pub(super) fn logo_style(markdown: &MarkdownRender, no_color: bool) -> Style {
    if no_color {
        Style::new()
    } else {
        Style::new().fg(from_markdown(markdown.heading_color(), HEADING_FALLBACK))
    }
}

/// A REPL colour; RGB goes through `markdown` so a terminal without
/// truecolor gets a palette entry instead.
fn from_repl(colour: nu_ansi_term::Color, markdown: &MarkdownRender) -> Color {
    use nu_ansi_term::Color as Repl;
    match colour {
        Repl::Black => Color::Black,
        Repl::DarkGray => Color::DarkGray,
        Repl::Red => Color::Red,
        Repl::LightRed => Color::LightRed,
        Repl::Green => Color::Green,
        Repl::LightGreen => Color::LightGreen,
        Repl::Yellow => Color::Yellow,
        Repl::LightYellow => Color::LightYellow,
        Repl::Blue => Color::Blue,
        Repl::LightBlue => Color::LightBlue,
        Repl::Purple | Repl::Magenta => Color::Magenta,
        Repl::LightPurple | Repl::LightMagenta => Color::LightMagenta,
        Repl::Cyan => Color::Cyan,
        Repl::LightCyan => Color::LightCyan,
        // nu_ansi_term's White is SGR 37 and its LightGray SGR 97, the
        // other way round from ratatui's names.
        Repl::White => Color::Gray,
        Repl::LightGray => Color::White,
        Repl::Fixed(index) => Color::Indexed(index),
        Repl::Rgb(r, g, b) => Color::from_crossterm(markdown.rgb_color(r, g, b)),
        Repl::Default => Color::Reset,
    }
}

/// A markdown theme colour, or `fallback` where no theme gave one.
fn from_markdown(colour: crossterm::style::Color, fallback: Color) -> Color {
    match colour {
        crossterm::style::Color::Reset => fallback,
        colour => Color::from_crossterm(colour),
    }
}

/// Every style the chrome draws with, built with the renderer so a theme
/// change or a `NO_COLOR` switch reaches both.
pub(super) struct Theme {
    /// Every separator rule: the one between the transcript and the status
    /// row, the composer's two and a question's two.
    pub(super) rule: Style,
    pub(super) hint_key: Style,
    pub(super) hint_label: Style,
    /// What a turn is doing, on the status row.
    pub(super) status_runtime: Style,
    /// The frame that turns in front of it while it runs.
    pub(super) throbber: Style,
    /// The key that interrupts it, beside it.
    pub(super) status_key: Style,
    pub(super) footer_field: Style,
    pub(super) footer_separator: Style,
    pub(super) queued_header: Style,
    pub(super) queued_text: Style,
    pub(super) todo_done: Style,
    pub(super) todo_open: Style,
    /// The key help's title and section headings, and the todo goal.
    pub(super) heading: Style,
    /// A question's text where its markdown sets no colour of its own,
    /// in the REPL's question colour.
    pub(super) question: Style,
    /// The option under a question's cursor.
    pub(super) option_highlight: Style,
    pub(super) known_command: Style,
    pub(super) unknown_command: Style,
    pub(super) palette: RowStyles,
    /// The background of the transcript and the rule under it.
    pub(super) surface_base: Style,
    /// The background of the composer, the bands above it, a question
    /// and the pop-ups.
    pub(super) surface_raised: Style,
    /// The background of the status row, the footer and the pager's title
    /// and hint rows.
    pub(super) surface_chrome: Style,
    /// Text that steps back from what is around it; dim under `NO_COLOR`.
    pub(super) faded: Style,
    /// A sub-session's name and state in the thread pop-up, by state; a
    /// cancelled or crashed one is drawn as failed.
    pub(super) thread_running: Style,
    pub(super) thread_completed: Style,
    pub(super) thread_failed: Style,
    /// Text selected with the mouse; reversed under `NO_COLOR`.
    pub(super) selection: Style,
}

impl Theme {
    pub(super) fn new(markdown: &MarkdownRender, no_color: bool, light: bool) -> Self {
        let paint = |style: Style, colour: Color| {
            if no_color { style } else { style.fg(colour) }
        };
        let heading = from_markdown(markdown.heading_color(), HEADING_FALLBACK);
        let tool_fn = from_repl(tool_fn_color(), markdown);
        let tool_key = from_repl(tool_key_color(), markdown);
        let palette = if light { &LIGHT_PALETTE } else { &DARK_PALETTE };
        let rgb = |(r, g, b): Rgb| Color::from_crossterm(markdown.rgb_color(r, g, b));
        let surface = |colour: Rgb| {
            if no_color {
                Style::new()
            } else {
                Style::new().bg(rgb(colour))
            }
        };
        let faded = if no_color {
            DIM
        } else {
            Style::new().fg(rgb(palette.faded))
        };
        let threads = if light { &LIGHT_THREADS } else { &DARK_THREADS };
        let thread = |colour: Rgb| {
            if no_color {
                Style::new()
            } else {
                Style::new().fg(rgb(colour))
            }
        };
        Self {
            rule: paint(BOLD, if light { RULE_LIGHT } else { RULE_DARK }),
            hint_key: paint(ITALIC.add_modifier(Modifier::BOLD), HIGHLIGHT),
            hint_label: ITALIC.patch(faded),
            status_runtime: paint(DIM, tool_fn),
            throbber: paint(Style::new(), tool_fn),
            status_key: paint(DIM, HIGHLIGHT),
            footer_field: paint(DIM, tool_key),
            footer_separator: paint(DIM, FOOTER_SEPARATOR),
            queued_header: paint(Style::new(), QUEUED_HEADER),
            queued_text: faded,
            todo_done: paint(DIM, TODO_DONE),
            todo_open: paint(Style::new(), TODO_OPEN),
            heading: paint(BOLD, heading),
            question: paint(Style::new(), heading),
            option_highlight: paint(BOLD, HIGHLIGHT),
            known_command: paint(BOLD, KNOWN_COMMAND),
            unknown_command: paint(BOLD, UNKNOWN_COMMAND),
            palette: if no_color { PLAIN_ROWS } else { PALETTE_ROWS },
            surface_base: surface(palette.base),
            surface_raised: surface(palette.raised),
            surface_chrome: surface(palette.chrome),
            faded,
            thread_running: thread(threads.running),
            thread_completed: thread(threads.completed),
            thread_failed: thread(threads.failed),
            selection: if no_color {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new().bg(rgb(palette.selection))
            },
        }
    }
}

#[cfg(test)]
impl Theme {
    /// The theme a config without a markdown theme gets.
    pub(super) fn fallback(no_color: bool) -> Self {
        let renderer = super::Renderer::new(crate::render::RenderOptions::default())
            .expect("a plain renderer");
        Self::new(renderer.markdown_render(), no_color, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RenderOptions;

    fn renderer(truecolor: bool) -> super::super::Renderer {
        let options = RenderOptions {
            truecolor,
            ..RenderOptions::default()
        };
        super::super::Renderer::new(options).expect("a plain renderer")
    }

    #[test]
    fn an_rgb_repl_colour_becomes_a_palette_entry_without_truecolor() {
        let rgb = nu_ansi_term::Color::Rgb(122, 120, 111);
        let plain = renderer(false);
        assert!(matches!(
            from_repl(rgb, plain.markdown_render()),
            Color::Indexed(_)
        ));
        let truecolor = renderer(true);
        assert_eq!(
            from_repl(rgb, truecolor.markdown_render()),
            Color::Rgb(122, 120, 111)
        );
        let fixed = nu_ansi_term::Color::Fixed(243);
        assert_eq!(
            from_repl(fixed, plain.markdown_render()),
            Color::Indexed(243)
        );
    }

    /// The surfaces and the faded text take the palette of the theme's
    /// side, and a terminal without truecolor gets palette entries.
    #[test]
    fn the_surfaces_follow_the_palette_only_under_truecolor() {
        let rgb = |(r, g, b): Rgb| Color::Rgb(r, g, b);
        for (light, palette) in [(false, &DARK_PALETTE), (true, &LIGHT_PALETTE)] {
            let truecolor = renderer(true);
            let theme = Theme::new(truecolor.markdown_render(), false, light);
            assert_eq!(theme.surface_base, Style::new().bg(rgb(palette.base)));
            assert_eq!(theme.surface_raised, Style::new().bg(rgb(palette.raised)));
            assert_eq!(theme.surface_chrome, Style::new().bg(rgb(palette.chrome)));
            assert_eq!(theme.faded, Style::new().fg(rgb(palette.faded)));
            assert_eq!(theme.selection, Style::new().bg(rgb(palette.selection)));
            let plain = renderer(false);
            let theme = Theme::new(plain.markdown_render(), false, light);
            for style in [
                theme.surface_base,
                theme.surface_raised,
                theme.surface_chrome,
            ] {
                assert!(matches!(style.bg, Some(Color::Indexed(_))), "{style:?}");
            }
            assert!(matches!(theme.faded.fg, Some(Color::Indexed(_))));
        }
    }

    /// The thread states take their side's colours, and none at all
    /// under `NO_COLOR`.
    #[test]
    fn thread_states_take_their_sides_colours_and_none_under_no_color() {
        let rgb = |(r, g, b): Rgb| Color::Rgb(r, g, b);
        let truecolor = renderer(true);
        for (light, threads) in [(false, &DARK_THREADS), (true, &LIGHT_THREADS)] {
            let theme = Theme::new(truecolor.markdown_render(), false, light);
            assert_eq!(theme.thread_running, Style::new().fg(rgb(threads.running)));
            assert_eq!(
                theme.thread_completed,
                Style::new().fg(rgb(threads.completed))
            );
            assert_eq!(theme.thread_failed, Style::new().fg(rgb(threads.failed)));
            let plain = Theme::new(truecolor.markdown_render(), true, light);
            for style in [
                plain.thread_running,
                plain.thread_completed,
                plain.thread_failed,
            ] {
                assert_eq!(style, Style::new());
            }
        }
    }

    /// WCAG 2 relative luminance.
    fn luminance((r, g, b): Rgb) -> f64 {
        let linear = |channel: u8| {
            let c = f64::from(channel) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Faded text stays readable at the WCAG AA ratio for body text on
    /// the surfaces it is drawn on.
    #[test]
    fn faded_text_meets_the_aa_contrast_on_the_base_and_the_raised_surface() {
        for palette in [&DARK_PALETTE, &LIGHT_PALETTE] {
            for surface in [palette.base, palette.raised] {
                let ratio = contrast(palette.faded, surface);
                assert!(
                    ratio >= 4.5,
                    "{:?} on {surface:?}: {ratio:.2}",
                    palette.faded
                );
            }
        }
    }

    /// With no markdown theme loaded the chrome still has colours, and
    /// `NO_COLOR` takes every one of them while the attributes stay.
    #[test]
    fn no_color_drops_every_colour_and_keeps_the_attributes() {
        let coloured = Theme::fallback(false);
        let plain = Theme::fallback(true);
        assert_eq!(coloured.rule, BOLD.fg(RULE_DARK));
        assert_eq!(coloured.heading.fg, Some(HEADING_FALLBACK));
        assert_eq!(coloured.hint_key.fg, Some(HIGHLIGHT));
        let pairs = [
            (coloured.rule, plain.rule),
            (coloured.hint_key, plain.hint_key),
            (coloured.status_runtime, plain.status_runtime),
            (coloured.throbber, plain.throbber),
            (coloured.status_key, plain.status_key),
            (coloured.footer_field, plain.footer_field),
            (coloured.footer_separator, plain.footer_separator),
            (coloured.queued_header, plain.queued_header),
            (coloured.todo_done, plain.todo_done),
            (coloured.todo_open, plain.todo_open),
            (coloured.heading, plain.heading),
            (coloured.question, plain.question),
            (coloured.option_highlight, plain.option_highlight),
            (coloured.known_command, plain.known_command),
            (coloured.unknown_command, plain.unknown_command),
        ];
        for (coloured, plain) in pairs {
            assert_eq!((plain.fg, plain.bg), (None, None), "{plain:?}");
            assert_eq!(plain.add_modifier, coloured.add_modifier, "{coloured:?}");
        }
        assert!(coloured.faded.fg.is_some());
        assert_eq!(coloured.faded.add_modifier, Modifier::empty());
        assert_eq!(coloured.hint_label, ITALIC.fg(coloured.faded.fg.unwrap()));
        assert_eq!(coloured.queued_text, coloured.faded);
        assert_eq!(plain.faded, DIM);
        assert_eq!(plain.hint_label, ITALIC.add_modifier(Modifier::DIM));
        assert_eq!(plain.queued_text, DIM);
        for surface in [
            coloured.surface_base,
            coloured.surface_raised,
            coloured.surface_chrome,
        ] {
            assert!(surface.bg.is_some(), "{surface:?}");
        }
        for surface in [
            plain.surface_base,
            plain.surface_raised,
            plain.surface_chrome,
        ] {
            assert_eq!(surface, Style::new());
        }
        let renderer = renderer(false);
        let markdown = renderer.markdown_render();
        assert_eq!(
            logo_style(markdown, false),
            Style::new().fg(HEADING_FALLBACK)
        );
        assert_eq!(logo_style(markdown, true), Style::new());
    }
}
