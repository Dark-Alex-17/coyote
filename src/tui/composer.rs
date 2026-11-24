//! The composer: the field the user types into, drawn as a surface under a
//! rule that separates it from the status row. A typed command is painted
//! over the field after the textarea has drawn it, green when it names
//! something that would run and red when it does not.

use super::keys::ViMotion;
use super::theme::{DIM, Theme};
use crate::config::CommandPrefix;
use crate::repl::{CommandSpan, classify_command, placeholder_ending_at, placeholder_starting_at};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Widget};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The rule's title while the vi layer is in normal mode.
pub(super) const NORMAL_TAG: &str = " NORMAL ";
/// Text rows the field shows, between the rules above and below them.
const MIN_ROWS: u16 = 3;
const MAX_ROWS: u16 = 8;
/// The rules that frame the field.
pub(super) const RULE_ROWS: u16 = 2;

fn placeholder(prefix: CommandPrefix) -> String {
    format!(
        "Type a prompt or a {}command; Enter sends",
        prefix.display()
    )
}

pub(super) struct Composer {
    textarea: TextArea<'static>,
    prefix: CommandPrefix,
    /// The theme's faded style, set each time the App builds a theme.
    placeholder_style: Style,
}

impl Composer {
    pub(super) fn new(prefix: CommandPrefix) -> Self {
        Self {
            textarea: Self::textarea(Vec::new(), prefix, DIM),
            prefix,
            placeholder_style: DIM,
        }
    }

    fn textarea(
        lines: Vec<String>,
        prefix: CommandPrefix,
        placeholder_style: Style,
    ) -> TextArea<'static> {
        let mut textarea = TextArea::new(lines);
        textarea.set_cursor_line_style(Style::new());
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        textarea.set_placeholder_text(placeholder(prefix));
        textarea.set_placeholder_style(placeholder_style);
        textarea
    }

    pub(super) fn set_placeholder_style(&mut self, style: Style) {
        self.placeholder_style = style;
        self.textarea.set_placeholder_style(style);
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
        self.textarea = Self::textarea(
            text.split('\n').map(str::to_string).collect(),
            self.prefix,
            self.placeholder_style,
        );
        self.textarea.move_cursor(CursorMove::Bottom);
        self.textarea.move_cursor(CursorMove::End);
    }

    pub(super) fn clear(&mut self) {
        self.textarea = Self::textarea(Vec::new(), self.prefix, self.placeholder_style);
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

    /// Removes a whole `[Image #N]` naming one of the `pasted` images, as
    /// the REPL does: Backspace the one ending at the cursor, Delete the
    /// one starting there. False, with nothing changed, for any other key,
    /// with no such placeholder there, or while text is selected, since a
    /// selection is what the key then deletes.
    pub(super) fn delete_placeholder(&mut self, key: KeyEvent, pasted: usize) -> bool {
        if !key.modifiers.is_empty() || self.textarea.is_selecting() {
            return false;
        }
        let cursor = self.textarea.cursor();
        let line = &self.textarea.lines()[cursor.0];
        let at = line
            .char_indices()
            .nth(cursor.1)
            .map_or(line.len(), |(at, _)| at);
        let span = match key.code {
            KeyCode::Backspace => placeholder_ending_at(line, at, pasted),
            KeyCode::Delete => placeholder_starting_at(line, at, pasted),
            _ => None,
        };
        let Some(span) = span else {
            return false;
        };
        let chars = line[span].chars().count();
        if key.code == KeyCode::Backspace {
            for _ in 0..chars {
                self.textarea.move_cursor(CursorMove::Back);
            }
        }
        self.textarea.delete_str(chars);
        true
    }

    pub(super) fn insert_str(&mut self, text: &str) {
        self.textarea.insert_str(text);
    }

    pub(super) fn insert_newline(&mut self) {
        self.textarea.insert_newline();
    }

    pub(super) fn delete_line_by_head(&mut self) {
        self.textarea.delete_line_by_head();
    }

    pub(super) fn undo(&mut self) {
        self.textarea.undo();
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

    /// Rows the field takes in a frame `width` wide: its text rows as
    /// they wrap, following the draft between the bounds, plus the rules
    /// above and below them.
    pub(super) fn rows(&self, width: u16) -> u16 {
        u16::try_from(wrapped_rows(self.textarea.lines(), width, None))
            .unwrap_or(MAX_ROWS)
            .clamp(MIN_ROWS, MAX_ROWS)
            + RULE_ROWS
    }

    pub(super) fn view<'a>(
        &'a self,
        prefix: CommandPrefix,
        extra_known: &[&str],
        theme: &'a Theme,
        vi_normal: bool,
    ) -> ComposerView<'a> {
        let first_line = self.textarea.lines().first().map(String::as_str);
        ComposerView {
            composer: self,
            command: first_line.and_then(|line| classify_command(line, prefix, extra_known)),
            theme,
            vi_normal,
        }
    }
}

/// Columns a tab advances to the next stop, as the field draws it.
const TAB_LEN: usize = 4;

/// Screen rows `lines` take in a field `width` wide, packed as the field
/// packs them: the Unicode word-boundary chunks of a line fill a row while
/// they fit, a chunk wider than a whole row is broken between graphemes,
/// and whatever follows a broken chunk starts a row of its own. A masked
/// field draws every character as `mask`.
pub(super) fn wrapped_rows(lines: &[String], width: u16, mask: Option<char>) -> usize {
    let width = usize::from(width.max(1));
    lines.iter().map(|line| line_rows(line, width, mask)).sum()
}

/// The column `text` ends at when it starts at column `at`.
fn width_to(text: &str, at: usize, mask: Option<char>) -> usize {
    let cell = |c: char| {
        if c.is_control() {
            0
        } else {
            c.width().unwrap_or(0)
        }
    };
    text.chars().fold(at, |at, c| match mask {
        Some(mask) => at + cell(mask),
        None if c == '\t' => at + TAB_LEN - at % TAB_LEN,
        None => at + cell(c),
    })
}

fn line_rows(line: &str, width: usize, mask: Option<char>) -> usize {
    let chunks: Vec<&str> = line.split_word_bounds().collect();
    let mut rows = 0;
    let mut used = 0;
    let mut open = false;
    let mut i = 0;
    while i < chunks.len() {
        let chunk = chunks[i];
        let span = width_to(chunk, used, mask) - used;
        if used + span <= width {
            used += span;
            open = true;
            i += 1;
            continue;
        }
        if open {
            rows += 1;
            used = 0;
            open = false;
            continue;
        }
        rows += grapheme_rows(chunk, width, mask);
        i += 1;
    }
    (rows + usize::from(open)).max(1)
}

/// Rows a chunk wider than a row takes once broken between graphemes,
/// each row taking graphemes while they fit and at least one.
fn grapheme_rows(chunk: &str, width: usize, mask: Option<char>) -> usize {
    let graphemes: Vec<&str> = chunk.graphemes(true).collect();
    let mut rows = 0;
    let mut i = 0;
    while i < graphemes.len() {
        let mut used = 0;
        let mut taken = 0;
        for grapheme in &graphemes[i..] {
            let next = width_to(grapheme, used, mask);
            if taken > 0 && next > width {
                break;
            }
            taken += 1;
            used = next;
            if used > width {
                break;
            }
        }
        i += taken;
        rows += 1;
    }
    rows
}

pub(super) struct ComposerView<'a> {
    composer: &'a Composer,
    command: Option<CommandSpan>,
    theme: &'a Theme,
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
        let style = if span.known {
            self.theme.known_command
        } else {
            self.theme.unknown_command
        };
        buf.set_style(Rect::new(inner.x + start, inner.y, end - start, 1), style);
    }
}

impl Widget for ComposerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let mut rule = Block::new()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(self.theme.rule);
        if self.vi_normal {
            rule = rule.title(NORMAL_TAG);
        }
        let inner = rule.inner(area);
        rule.render(area, buf);
        if inner.is_empty() {
            return;
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
    use ratatui::style::{Color, Modifier};
    use ratatui::symbols::line::HORIZONTAL;

    const AREA: Rect = Rect::new(0, 0, 20, 5);
    const KNOWN_COLOR: Color = Color::Green;
    const UNKNOWN_COLOR: Color = Color::Red;

    fn render(text: &str, prefix: CommandPrefix, extra_known: &[&str], no_color: bool) -> Buffer {
        let theme = Theme::fallback(no_color);
        let mut composer = Composer::new(prefix);
        composer.set_placeholder_style(theme.faded);
        composer.set_text(text);
        let mut buf = Buffer::empty(AREA);
        composer
            .view(prefix, extra_known, &theme, false)
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
        let composer = Composer::new(CommandPrefix::Dot);
        assert_eq!(
            composer.textarea.placeholder_text(),
            "Type a prompt or a .command; Enter sends"
        );
        let mut composer = Composer::new(CommandPrefix::Slash);
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
    fn the_placeholder_is_faded_with_and_without_colour() {
        for no_color in [false, true] {
            let faded = Theme::fallback(no_color).faded;
            let buf = render("", CommandPrefix::Dot, &[], no_color);
            let row: String = (1..AREA.width).map(|x| buf[(x, 1)].symbol()).collect();
            assert!(row.starts_with("Type a prompt"), "{no_color}: {row:?}");
            assert!(
                (1..5).all(|x| buf[(x, 1)].modifier.contains(Modifier::DIM) == no_color),
                "{no_color}: {:?}",
                first_row(&buf)
            );
            assert_eq!(buf[(1, 1)].fg, faded.fg.unwrap_or(Color::Reset));
        }
    }

    #[test]
    fn the_rules_frame_the_field() {
        let buf = render("hi", CommandPrefix::Dot, &[], false);
        for y in [0, AREA.height - 1] {
            assert_eq!(buf[(0, y)].symbol(), HORIZONTAL);
            assert_eq!(buf[(19, y)].symbol(), HORIZONTAL);
            assert!(buf[(0, y)].modifier.contains(Modifier::BOLD));
            assert!(!buf[(0, y)].modifier.contains(Modifier::DIM));
        }
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
        let mut composer = Composer::new(CommandPrefix::Dot);
        composer.set_text(&[".info x", "a", "b", "c", "d", "e"].join("\n"));
        let mut buf = Buffer::empty(AREA);
        composer
            .view(CommandPrefix::Dot, &[], &Theme::fallback(false), false)
            .render(AREA, &mut buf);
        assert!(composer.textarea.scroll_offset().0 > 0);
        assert!(first_row(&buf).iter().all(|cell| !cell.1));
    }

    #[test]
    fn normal_mode_titles_the_rule() {
        let composer = Composer::new(CommandPrefix::Dot);
        let mut buf = Buffer::empty(AREA);
        composer
            .view(CommandPrefix::Dot, &[], &Theme::fallback(false), true)
            .render(AREA, &mut buf);
        let top: String = (0..AREA.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top.contains(NORMAL_TAG), "{top:?}");
        assert_eq!(buf[(AREA.width - 1, 0)].symbol(), HORIZONTAL);
    }

    #[test]
    fn the_vi_edits_move_and_delete_in_the_draft() {
        let mut composer = Composer::new(CommandPrefix::Dot);
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
        let mut composer = Composer::new(CommandPrefix::Dot);
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
        let mut composer = Composer::new(CommandPrefix::Dot);
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
        let mut composer = Composer::new(CommandPrefix::Dot);
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
        let mut composer = Composer::new(CommandPrefix::Dot);
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
        let mut composer = Composer::new(CommandPrefix::Dot);
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
        let mut composer = Composer::new(CommandPrefix::Dot);
        composer.set_text("\ncd");
        composer.move_cursor(ViMotion::Up);
        composer.delete_next_char();
        assert_eq!(composer.text(), "\ncd");
        assert_eq!(composer.textarea.cursor(), (0, 0));
    }

    #[test]
    fn deleting_a_line_takes_only_that_line() {
        let mut composer = Composer::new(CommandPrefix::Dot);
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
    fn the_rows_follow_the_draft_between_the_bounds_plus_the_rules() {
        let mut composer = Composer::new(CommandPrefix::Dot);
        assert_eq!(composer.rows(80), MIN_ROWS + RULE_ROWS);
        composer.set_text(&["x"; 5].join("\n"));
        assert_eq!(composer.rows(80), 5 + RULE_ROWS);
        composer.set_text(&["x"; 20].join("\n"));
        assert_eq!(composer.rows(80), MAX_ROWS + RULE_ROWS);
    }

    /// A line wider than the field wraps onto rows of its own, which the
    /// field grows to show, a word too long for a row broken across it.
    #[test]
    fn a_long_line_wraps_and_the_field_grows_with_it() {
        let mut composer = Composer::new(CommandPrefix::Dot);
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        composer.set_text(text);
        assert_eq!(composer.rows(20), 4 + RULE_ROWS);
        let area = Rect::new(0, 0, 20, composer.rows(20));
        let mut buf = Buffer::empty(area);
        composer
            .view(CommandPrefix::Dot, &[], &Theme::fallback(false), false)
            .render(area, &mut buf);
        let row = |y: u16| -> String { (0..20).map(|x| buf[(x, y)].symbol()).collect() };
        assert_eq!(row(0), HORIZONTAL.repeat(20));
        assert_eq!(row(1).trim_end(), "alpha bravo charlie");
        assert_eq!(row(2).trim_end(), "delta echo foxtrot");
        assert_eq!(row(4).trim_end(), "juliet");
        assert_eq!(row(area.height - 1), HORIZONTAL.repeat(20));
        assert_eq!(composer.text(), text, "wrapping changed the draft");

        composer.set_text(&"x".repeat(45));
        assert_eq!(
            wrapped_rows(composer.lines(), 20, None),
            3,
            "a word wider than the row is broken across rows"
        );
    }

    /// The count follows the field's packing where it differs from plain
    /// word wrapping: a space is a chunk that takes a row of its own when
    /// nothing else fits, what follows a broken word starts a row, and a
    /// path breaks at its punctuation rather than filling every row.
    #[test]
    fn the_row_count_packs_like_the_field() {
        let rows = |line: &str| wrapped_rows(&[line.to_string()], 20, None);
        assert_eq!(rows(&"a".repeat(20)), 1, "a word that fills the row");
        assert_eq!(rows(&format!("{} {}", "a".repeat(20), "b".repeat(20))), 3);
        assert_eq!(rows(&format!("{} y", "x".repeat(25))), 3);
        assert_eq!(rows("\tx"), 1);
        assert_eq!(rows(""), 1);
        let path = "aaaaaaaaaaaa/bbbbbbbbbbbb/cccccccccccc/dddddddddddd/eeeeeeeeeeee";
        assert_eq!(rows(path), 5, "a path packs at full width");
        assert_eq!(
            wrapped_rows(&["\u{4e2d}".repeat(10)], 20, Some('*')),
            1,
            "a masked field draws one mask per character"
        );
    }

    /// The count is the rows the field actually draws, for each kind of
    /// line the packing treats differently.
    #[test]
    fn the_row_count_matches_the_rows_the_field_draws() {
        let lines = [
            "alpha bravo charlie delta echo foxtrot golf hotel india juliet",
            "aaaaaaaaaaaa/bbbbbbbbbbbb/cccccccccccc/dddddddddddd/eeeeeeeeeeee",
            "https://example.com/some/long-path/with.dots?and=query&more=stuff",
            "xxxxxxxxxxxxxxxxxxxxxxxxx y",
            "\u{4e2d}\u{6587}\u{5b57}\u{4e2d}\u{6587}\u{5b57}\u{4e2d}\u{6587}\u{5b57}\u{4e2d}\u{6587}",
        ];
        for line in lines {
            let mut composer = Composer::new(CommandPrefix::Dot);
            composer.set_text(line);
            let area = Rect::new(0, 0, 20, 14);
            let mut buf = Buffer::empty(area);
            composer
                .view(CommandPrefix::Dot, &[], &Theme::fallback(true), false)
                .render(area, &mut buf);
            let drawn = (1..area.height - 1)
                .filter(|y| (0..20).any(|x| !buf[(x, *y)].symbol().trim().is_empty()))
                .count();
            assert_eq!(wrapped_rows(composer.lines(), 20, None), drawn, "{line:?}");
        }
    }
}
