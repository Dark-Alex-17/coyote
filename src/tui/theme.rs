//! The UI's own colours: the plain REPL's where it shows the same thing
//! (the tool call line, the completion menu, the typed command), the
//! transcript theme's for the rest, and fixed named colours when no theme
//! is loaded. Under `NO_COLOR` every style keeps its attributes and loses
//! its colours.

use crate::config::AppConfig;
use crate::events::NoticeKind;
use crate::render::{MarkdownRender, RenderOptions};
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
/// How far each channel of the theme's background moves towards the text
/// so the composer's field reads as its own surface.
const SURFACE_NUDGE: u8 = 12;
const DARK_FALLBACK: Color = Color::Indexed(236);
const LIGHT_FALLBACK: Color = Color::Indexed(254);

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

/// The field's fill: the transcript theme's background nudged a step
/// towards the text, which only an RGB-capable terminal can show; every
/// other case gets a fixed grey on the theme's side of the scale.
pub(super) fn field_background(config: &AppConfig, options: &RenderOptions) -> Color {
    let light = config.light_theme();
    let fallback = if light { LIGHT_FALLBACK } else { DARK_FALLBACK };
    if !options.truecolor {
        return fallback;
    }
    let Some(settings) = options.theme.as_ref().map(|theme| &theme.settings) else {
        return fallback;
    };
    let Some(base) = settings.background.or(settings.line_highlight) else {
        return fallback;
    };
    let nudge = |channel: u8| {
        if light {
            channel.saturating_sub(SURFACE_NUDGE)
        } else {
            channel.saturating_add(SURFACE_NUDGE)
        }
    };
    Color::Rgb(nudge(base.r), nudge(base.g), nudge(base.b))
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
    no_color: bool,
}

impl Theme {
    pub(super) fn new(markdown: &MarkdownRender, no_color: bool, light: bool) -> Self {
        let paint = |style: Style, colour: Color| {
            if no_color { style } else { style.fg(colour) }
        };
        let heading = from_markdown(markdown.heading_color(), HEADING_FALLBACK);
        let tool_dim = from_repl(tool_dim_color(), markdown);
        let tool_fn = from_repl(tool_fn_color(), markdown);
        let tool_key = from_repl(tool_key_color(), markdown);
        Self {
            rule: paint(BOLD, if light { RULE_LIGHT } else { RULE_DARK }),
            hint_key: paint(ITALIC.add_modifier(Modifier::BOLD), HIGHLIGHT),
            hint_label: paint(ITALIC, tool_dim),
            status_runtime: paint(DIM, tool_fn),
            throbber: paint(Style::new(), tool_fn),
            status_key: paint(DIM, HIGHLIGHT),
            footer_field: paint(DIM, tool_key),
            footer_separator: paint(DIM, FOOTER_SEPARATOR),
            queued_header: paint(Style::new(), QUEUED_HEADER),
            queued_text: DIM,
            todo_done: paint(DIM, TODO_DONE),
            todo_open: paint(Style::new(), TODO_OPEN),
            heading: paint(BOLD, heading),
            question: paint(Style::new(), heading),
            option_highlight: paint(BOLD, HIGHLIGHT),
            known_command: paint(BOLD, KNOWN_COMMAND),
            unknown_command: paint(BOLD, UNKNOWN_COMMAND),
            palette: if no_color { PLAIN_ROWS } else { PALETTE_ROWS },
            no_color,
        }
    }

    /// The composer field's fill; none under `NO_COLOR`.
    pub(super) fn surface(&self, background: Color) -> Style {
        if self.no_color {
            Style::new()
        } else {
            Style::new().bg(background)
        }
    }
}

#[cfg(test)]
impl Theme {
    /// The theme a config without a markdown theme gets.
    pub(super) fn fallback(no_color: bool) -> Self {
        let renderer = super::Renderer::new(RenderOptions::default()).expect("a plain renderer");
        Self::new(renderer.markdown_render(), no_color, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntect::highlighting::{Color as SyntectColor, Theme as SyntectTheme, ThemeSettings};

    #[test]
    fn an_rgb_repl_colour_becomes_a_palette_entry_without_truecolor() {
        let render = |truecolor: bool| {
            let options = RenderOptions {
                truecolor,
                ..RenderOptions::default()
            };
            super::super::Renderer::new(options).expect("a plain renderer")
        };
        let rgb = nu_ansi_term::Color::Rgb(122, 120, 111);
        let plain = render(false);
        assert!(matches!(
            from_repl(rgb, plain.markdown_render()),
            Color::Indexed(_)
        ));
        let truecolor = render(true);
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

    #[test]
    fn the_background_follows_the_theme_only_under_truecolor() {
        let dark = AppConfig::default();
        let light = AppConfig {
            theme: Some("light".to_string()),
            ..Default::default()
        };
        let plain = RenderOptions::new(None, None, false, false, false);
        assert_eq!(field_background(&dark, &plain), DARK_FALLBACK);
        assert_eq!(field_background(&light, &plain), LIGHT_FALLBACK);

        let theme = SyntectTheme {
            settings: ThemeSettings {
                background: Some(SyntectColor {
                    r: 30,
                    g: 31,
                    b: 32,
                    a: 255,
                }),
                ..ThemeSettings::default()
            },
            ..SyntectTheme::default()
        };
        let mut themed = RenderOptions::new(Some(theme), None, false, false, false);
        assert_eq!(field_background(&dark, &themed), DARK_FALLBACK);
        themed.truecolor = true;
        assert_eq!(field_background(&dark, &themed), Color::Rgb(42, 43, 44));
        assert_eq!(field_background(&light, &themed), Color::Rgb(18, 19, 20));
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
            (coloured.hint_label, plain.hint_label),
            (coloured.status_runtime, plain.status_runtime),
            (coloured.throbber, plain.throbber),
            (coloured.status_key, plain.status_key),
            (coloured.footer_field, plain.footer_field),
            (coloured.footer_separator, plain.footer_separator),
            (coloured.queued_header, plain.queued_header),
            (coloured.queued_text, plain.queued_text),
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
        assert_eq!(plain.surface(DARK_FALLBACK), Style::new());
        assert_eq!(coloured.surface(DARK_FALLBACK).bg, Some(DARK_FALLBACK));
        let renderer = super::super::Renderer::new(RenderOptions::default()).unwrap();
        let markdown = renderer.markdown_render();
        assert_eq!(
            logo_style(markdown, false),
            Style::new().fg(HEADING_FALLBACK)
        );
        assert_eq!(logo_style(markdown, true), Style::new());
    }
}
