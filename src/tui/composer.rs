//! The composer: the field the user types into, drawn as a surface under a
//! rule that separates it from the status row. A typed command is painted
//! over the field after the textarea has drawn it, green when it names
//! something that would run and red when it does not.

use super::DIM;
use super::keys::ViMotion;
use crate::config::{AppConfig, CommandPrefix};
use crate::render::RenderOptions;
use crate::repl::{CommandSpan, classify_command};
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};
use ratatui_textarea::{CursorMove, TextArea};
use unicode_width::UnicodeWidthStr;

/// The rule's title while the vi layer is in normal mode.
pub(super) const NORMAL_TAG: &str = " NORMAL ";
/// Text rows the field shows, before the rule above them.
const MIN_ROWS: u16 = 3;
const MAX_ROWS: u16 = 8;
const KNOWN_COLOR: Color = Color::Green;
const UNKNOWN_COLOR: Color = Color::Red;
/// How far each channel of the theme's background moves towards the text
/// so the field reads as its own surface.
const SURFACE_NUDGE: u8 = 12;
const DARK_FALLBACK: Color = Color::Indexed(236);
const LIGHT_FALLBACK: Color = Color::Indexed(254);

fn placeholder(prefix: CommandPrefix) -> String {
    format!(
        "Type a prompt or a {}command; Enter sends",
        prefix.display()
    )
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

pub(super) struct Composer {
    textarea: TextArea<'static>,
    background: Color,
    prefix: CommandPrefix,
}

impl Composer {
    pub(super) fn new(background: Color, prefix: CommandPrefix) -> Self {
        Self {
            textarea: Self::textarea(Vec::new(), prefix),
            background,
            prefix,
        }
    }

    fn textarea(lines: Vec<String>, prefix: CommandPrefix) -> TextArea<'static> {
        let mut textarea = TextArea::new(lines);
        textarea.set_cursor_line_style(Style::new());
        textarea.set_placeholder_text(placeholder(prefix));
        textarea.set_placeholder_style(DIM);
        textarea
    }

    pub(super) fn set_background(&mut self, background: Color) {
        self.background = background;
    }

    pub(super) fn set_command_prefix(&mut self, prefix: CommandPrefix) {
        self.prefix = prefix;
        self.textarea.set_placeholder_text(placeholder(prefix));
    }

    pub(super) fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Replaces the draft and puts the cursor after its last character.
    pub(super) fn set_text(&mut self, text: &str) {
        self.textarea = Self::textarea(text.split('\n').map(str::to_string).collect(), self.prefix);
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::End);
    }

    pub(super) fn clear(&mut self) {
        self.textarea = Self::textarea(Vec::new(), self.prefix);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.textarea.is_empty()
    }

    pub(super) fn lines(&self) -> &[String] {
        self.textarea.lines()
    }

    pub(super) fn input(&mut self, key: KeyEvent) {
        self.textarea.input(key);
    }

    pub(super) fn insert_str(&mut self, text: &str) {
        self.textarea.insert_str(text);
    }

    pub(super) fn insert_newline(&mut self) {
        self.textarea.insert_newline();
    }

    /// The cursor's column and the count of characters on its line.
    fn cursor_in_line(&self) -> (usize, usize) {
        let cursor = self.textarea.cursor();
        (cursor.1, self.textarea.lines()[cursor.0].chars().count())
    }

    /// Normal mode's cursor sits on a character; the textarea's own end
    /// column, one past the last, is pulled back onto it.
    fn clamp_normal_cursor(&mut self) {
        let (col, len) = self.cursor_in_line();
        if len > 0 && col == len {
            self.textarea.move_cursor(CursorMove::Back);
        }
    }

    /// A normal-mode motion: `l` and `$` stop on the last character
    /// instead of the column after it, and `j`/`k` landing on a shorter
    /// line are pulled back onto its last character.
    pub(super) fn move_cursor(&mut self, motion: ViMotion) {
        match motion {
            ViMotion::Forward => {
                let (col, len) = self.cursor_in_line();
                if col + 1 < len {
                    self.textarea.move_cursor(CursorMove::Forward);
                }
            }
            ViMotion::End | ViMotion::Down | ViMotion::Up => {
                self.textarea.move_cursor(Self::cursor_move(motion));
                self.clamp_normal_cursor();
            }
            other => self.textarea.move_cursor(Self::cursor_move(other)),
        }
    }

    /// Where `a` and `A` start inserting: after the current character or
    /// after the last one, never on the next line.
    pub(super) fn move_cursor_to_append(&mut self, motion: ViMotion) {
        match motion {
            ViMotion::Forward => {
                let (col, len) = self.cursor_in_line();
                if col < len {
                    self.textarea.move_cursor(CursorMove::Forward);
                }
            }
            other => self.textarea.move_cursor(Self::cursor_move(other)),
        }
    }

    fn cursor_move(motion: ViMotion) -> CursorMove {
        match motion {
            ViMotion::Back => CursorMove::Back,
            ViMotion::Down => CursorMove::Down,
            ViMotion::Up => CursorMove::Up,
            ViMotion::Forward => CursorMove::Forward,
            ViMotion::WordForward => CursorMove::WordForward,
            ViMotion::WordBack => CursorMove::WordBack,
            ViMotion::Head => CursorMove::Head,
            ViMotion::End => CursorMove::End,
        }
    }

    /// Normal mode's `x`: the character under the cursor, or the last
    /// one when the cursor sits past it; an empty line keeps its break.
    pub(super) fn delete_next_char(&mut self) {
        if self.cursor_in_line().1 == 0 {
            return;
        }
        self.clamp_normal_cursor();
        self.textarea.delete_next_char();
        self.clamp_normal_cursor();
    }

    /// Deletes the cursor's line and the line break after it, so the
    /// lines below move up; the last line takes the break before it
    /// instead, leaving the cursor at the head of the new last line.
    pub(super) fn delete_line(&mut self) {
        let row = self.textarea.cursor().0;
        let last = row + 1 == self.textarea.lines().len();
        self.textarea.move_cursor(CursorMove::Head);
        // On an empty line the end-of-line delete would take the break
        // itself and the char delete below would eat into the next line.
        if !self.textarea.lines()[row].is_empty() {
            self.textarea.delete_line_by_end();
        }
        if last {
            self.textarea.delete_char();
            self.textarea.move_cursor(CursorMove::Head);
        } else {
            self.textarea.delete_next_char();
        }
    }

    /// Rows the field takes in a frame: its text rows, following the
    /// draft between the bounds, plus the rule above them.
    pub(super) fn rows(&self) -> u16 {
        u16::try_from(self.textarea.lines().len())
            .unwrap_or(MAX_ROWS)
            .clamp(MIN_ROWS, MAX_ROWS)
            + 1
    }

    pub(super) fn view<'a>(
        &'a self,
        prefix: CommandPrefix,
        extra_known: &[&str],
        no_color: bool,
        vi_normal: bool,
    ) -> ComposerView<'a> {
        let first_line = self.textarea.lines().first().map(String::as_str);
        ComposerView {
            composer: self,
            command: first_line.and_then(|line| classify_command(line, prefix, extra_known)),
            no_color,
            vi_normal,
        }
    }
}

pub(super) struct ComposerView<'a> {
    composer: &'a Composer,
    command: Option<CommandSpan>,
    no_color: bool,
    vi_normal: bool,
}

impl ComposerView<'_> {
    /// Repaints the command's cells on the first text row. The textarea
    /// has just drawn, so its scroll offset is the one the row was laid
    /// out with; a draft scrolled past its first line shows no command.
    fn paint_command(&self, span: &CommandSpan, inner: Rect, buf: &mut Buffer) {
        let textarea = &self.composer.textarea;
        let (top_row, top_col) = textarea.scroll_offset();
        if top_row > 0 {
            return;
        }
        let Some(line) = textarea.lines().first() else {
            return;
        };
        let start = line[..span.range.start].width();
        let end = start + line[span.range.clone()].width();
        let visible = |column: usize| {
            u16::try_from(column.saturating_sub(usize::from(top_col)))
                .unwrap_or(u16::MAX)
                .min(inner.width)
        };
        let (start, end) = (visible(start), visible(end));
        if end <= start {
            return;
        }
        let mut style = Style::new().add_modifier(Modifier::BOLD);
        if !self.no_color {
            style = style.fg(if span.known {
                KNOWN_COLOR
            } else {
                UNKNOWN_COLOR
            });
        }
        buf.set_style(Rect::new(inner.x + start, inner.y, end - start, 1), style);
    }
}

impl Widget for ComposerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let mut rule = Block::new().borders(Borders::TOP).border_style(DIM);
        if self.vi_normal {
            rule = rule.title(NORMAL_TAG);
        }
        let inner = rule.inner(area);
        rule.render(area, buf);
        if inner.is_empty() {
            return;
        }
        if !self.no_color {
            buf.set_style(inner, Style::new().bg(self.composer.background));
        }
        (&self.composer.textarea).render(inner, buf);
        if let Some(span) = &self.command {
            self.paint_command(span, inner, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::symbols::line::HORIZONTAL;
    use syntect::highlighting::{Color as SyntectColor, Theme, ThemeSettings};

    const AREA: Rect = Rect::new(0, 0, 20, 4);
    const BACKGROUND: Color = Color::Indexed(236);

    fn render(text: &str, prefix: CommandPrefix, extra_known: &[&str], no_color: bool) -> Buffer {
        let mut composer = Composer::new(BACKGROUND, prefix);
        composer.set_text(text);
        let mut buf = Buffer::empty(AREA);
        composer
            .view(prefix, extra_known, no_color, false)
            .render(AREA, &mut buf);
        buf
    }

    /// The foreground and bold flag of each cell on the first text row.
    fn first_row(buf: &Buffer) -> Vec<(Color, bool)> {
        (0..AREA.width)
            .map(|x| {
                let cell = &buf[(x, 1)];
                (cell.fg, cell.modifier.contains(Modifier::BOLD))
            })
            .collect()
    }

    fn painted(buf: &Buffer, columns: usize, colour: Color) -> bool {
        let row = first_row(buf);
        row[..columns].iter().all(|cell| *cell == (colour, true))
            && row[columns..].iter().all(|cell| !cell.1)
    }

    #[test]
    fn a_known_command_is_green_and_its_arguments_plain() {
        let buf = render(".info x", CommandPrefix::Dot, &[], false);
        assert!(painted(&buf, 5, KNOWN_COLOR), "{:?}", first_row(&buf));
        let buf = render("/info x", CommandPrefix::Slash, &[], false);
        assert!(painted(&buf, 5, KNOWN_COLOR), "{:?}", first_row(&buf));
        let buf = render(".help", CommandPrefix::Both, &[], false);
        assert!(painted(&buf, 5, KNOWN_COLOR), "{:?}", first_row(&buf));
    }

    #[test]
    fn an_unknown_command_is_red() {
        let buf = render(".nosuch", CommandPrefix::Dot, &[], false);
        assert!(painted(&buf, 7, UNKNOWN_COLOR), "{:?}", first_row(&buf));
        let buf = render(".infox", CommandPrefix::Dot, &[], false);
        assert!(painted(&buf, 6, UNKNOWN_COLOR), "{:?}", first_row(&buf));
    }

    #[test]
    fn a_macro_name_counts_as_known() {
        let buf = render(".mymacro", CommandPrefix::Dot, &["mymacro"], false);
        assert!(painted(&buf, 8, KNOWN_COLOR), "{:?}", first_row(&buf));
    }

    #[test]
    fn the_placeholder_names_the_configured_sigil() {
        let composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        assert_eq!(
            composer.textarea.placeholder_text(),
            "Type a prompt or a .command; Enter sends"
        );
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Slash);
        assert_eq!(
            composer.textarea.placeholder_text(),
            "Type a prompt or a /command; Enter sends"
        );
        composer.set_text("draft");
        composer.clear();
        assert!(composer.textarea.placeholder_text().contains("/command"));
        composer.set_command_prefix(CommandPrefix::Both);
        assert!(composer.textarea.placeholder_text().contains(".command"));
    }

    #[test]
    fn the_rule_and_the_surface_frame_the_field() {
        let buf = render("hi", CommandPrefix::Dot, &[], false);
        assert_eq!(buf[(0, 0)].symbol(), HORIZONTAL);
        assert_eq!(buf[(19, 0)].symbol(), HORIZONTAL);
        assert!(buf[(0, 0)].modifier.contains(Modifier::DIM));
        assert_eq!(buf[(0, 1)].bg, BACKGROUND);
        assert_eq!(buf[(19, 3)].bg, BACKGROUND);
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn no_color_keeps_the_rule_and_bold_but_paints_nothing() {
        let buf = render(".info x", CommandPrefix::Dot, &[], true);
        assert_eq!(buf[(0, 0)].symbol(), HORIZONTAL);
        assert!(painted(&buf, 5, Color::Reset), "{:?}", first_row(&buf));
        assert!((0..AREA.height).all(|y| (0..AREA.width).all(|x| buf[(x, y)].bg == Color::Reset)));
    }

    #[test]
    fn a_draft_scrolled_past_its_first_line_shows_no_command() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text(&[".info x", "a", "b", "c", "d", "e"].join("\n"));
        let mut buf = Buffer::empty(AREA);
        composer
            .view(CommandPrefix::Dot, &[], false, false)
            .render(AREA, &mut buf);
        assert!(composer.textarea.scroll_offset().0 > 0);
        assert!(first_row(&buf).iter().all(|cell| !cell.1));
    }

    #[test]
    fn normal_mode_titles_the_rule() {
        let composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        let mut buf = Buffer::empty(AREA);
        composer
            .view(CommandPrefix::Dot, &[], false, true)
            .render(AREA, &mut buf);
        let top: String = (0..AREA.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top.contains(NORMAL_TAG), "{top:?}");
        assert_eq!(buf[(AREA.width - 1, 0)].symbol(), HORIZONTAL);
    }

    #[test]
    fn the_vi_edits_move_and_delete_in_the_draft() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("one two\nthree");
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::Head);
        composer.move_cursor(ViMotion::WordForward);
        composer.delete_next_char();
        assert_eq!(composer.text(), "one wo\nthree");
        composer.move_cursor(ViMotion::End);
        composer.move_cursor(ViMotion::Back);
        composer.delete_next_char();
        assert_eq!(composer.text(), "one o\nthree");
        composer.delete_line();
        assert_eq!(composer.text(), "three");
        composer.delete_line();
        assert_eq!(composer.text(), "");
        composer.set_text("a b");
        composer.move_cursor(ViMotion::Head);
        composer.move_cursor(ViMotion::Forward);
        composer.delete_next_char();
        assert_eq!(composer.text(), "ab");
        composer.move_cursor(ViMotion::WordBack);
        composer.delete_next_char();
        assert_eq!(composer.text(), "b");
    }

    #[test]
    fn dollar_lands_on_the_last_character_so_x_deletes_it() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("one two");
        composer.move_cursor(ViMotion::Head);
        composer.move_cursor(ViMotion::End);
        assert_eq!(composer.textarea.cursor(), (0, 6));
        composer.delete_next_char();
        assert_eq!(composer.text(), "one tw");
        assert_eq!(composer.textarea.cursor(), (0, 5));
    }

    #[test]
    fn l_stops_on_the_last_character() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("ab\ncd");
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::Head);
        for _ in 0..3 {
            composer.move_cursor(ViMotion::Forward);
        }
        assert_eq!(composer.textarea.cursor(), (0, 1));
    }

    #[test]
    fn j_and_k_onto_a_shorter_line_land_on_its_last_character() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("one two\nab\nthree four");
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::End);
        assert_eq!(composer.textarea.cursor(), (0, 6));
        composer.move_cursor(ViMotion::Down);
        assert_eq!(composer.textarea.cursor(), (1, 1));
        composer.move_cursor(ViMotion::Down);
        composer.move_cursor(ViMotion::End);
        composer.move_cursor(ViMotion::Up);
        assert_eq!(composer.textarea.cursor(), (1, 1));
    }

    #[test]
    fn a_at_the_last_character_appends_on_the_same_line() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("ab\ncd");
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::End);
        composer.move_cursor_to_append(ViMotion::Forward);
        composer.insert_str("X");
        assert_eq!(composer.text(), "abX\ncd");
        // Past the end already (the column insert mode left behind).
        composer.move_cursor_to_append(ViMotion::Forward);
        composer.insert_str("Y");
        assert_eq!(composer.text(), "abXY\ncd");
    }

    #[test]
    fn x_at_the_end_of_a_line_deletes_the_last_character_and_keeps_the_lines() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("ab\ncd");
        // Past the end, as insert mode leaves the cursor; `k` would clamp.
        composer.textarea.move_cursor(CursorMove::Up);
        assert_eq!(composer.textarea.cursor(), (0, 2));
        composer.delete_next_char();
        assert_eq!(composer.text(), "a\ncd");
        assert_eq!(composer.textarea.cursor(), (0, 0));
    }

    #[test]
    fn x_on_an_empty_line_is_a_no_op() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("\ncd");
        composer.move_cursor(ViMotion::Up);
        composer.delete_next_char();
        assert_eq!(composer.text(), "\ncd");
        assert_eq!(composer.textarea.cursor(), (0, 0));
    }

    #[test]
    fn deleting_a_line_takes_only_that_line() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        composer.set_text("one\n\nthree");
        composer.move_cursor(ViMotion::Up);
        composer.delete_line();
        assert_eq!(composer.text(), "one\nthree", "an empty middle line");

        composer.set_text("one\n");
        composer.delete_line();
        assert_eq!(composer.text(), "one", "an empty last line");
        assert_eq!(composer.textarea.cursor(), (0, 0));

        composer.set_text("one\ntwo\nthree");
        composer.move_cursor(ViMotion::Up);
        composer.delete_line();
        assert_eq!(composer.text(), "one\nthree", "a middle line with text");

        composer.set_text("one\ntwo");
        composer.delete_line();
        assert_eq!(composer.text(), "one", "the last line with text");
    }

    #[test]
    fn the_rows_follow_the_draft_between_the_bounds_plus_the_rule() {
        let mut composer = Composer::new(BACKGROUND, CommandPrefix::Dot);
        assert_eq!(composer.rows(), MIN_ROWS + 1);
        composer.set_text(&["x"; 5].join("\n"));
        assert_eq!(composer.rows(), 6);
        composer.set_text(&["x"; 20].join("\n"));
        assert_eq!(composer.rows(), MAX_ROWS + 1);
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

        let theme = Theme {
            settings: ThemeSettings {
                background: Some(SyntectColor {
                    r: 30,
                    g: 31,
                    b: 32,
                    a: 255,
                }),
                ..ThemeSettings::default()
            },
            ..Theme::default()
        };
        let mut themed = RenderOptions::new(Some(theme), None, false, false, false);
        assert_eq!(field_background(&dark, &themed), DARK_FALLBACK);
        themed.truecolor = true;
        assert_eq!(field_background(&dark, &themed), Color::Rgb(42, 43, 44));
        assert_eq!(field_background(&light, &themed), Color::Rgb(18, 19, 20));
    }
}
