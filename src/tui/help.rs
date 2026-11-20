//! The key help: every binding of the key map grouped by where it
//! applies, drawn as a bordered pop-up over the transcript, so the
//! composer and the rows around it stay in view. `?` on an empty composer
//! and the `.keys` command open it. The rows come from `keys::KEY_HELP`,
//! the table the key map's tests hold to the bindings.

use super::keys::{HelpRow, KEY_HELP};
use super::render;
use super::theme::{DIM, Theme};
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

const TITLE: &str = "Keys";
/// Fits the bottom border of the pop-up on the 40 columns of the
/// narrowest frame the help is drawn at.
const HINT: &str = "j/k scroll  PgUp/Dn page  Esc close";
/// The transcript left showing around the pop-up.
const MARGIN: Margin = Margin::new(1, 1);
/// Columns before a row's keys.
const INDENT: usize = 2;
/// Columns between the keys and what they do.
const GAP: usize = 2;
/// The fewest columns a description keeps beside its keys; a frame with
/// less room puts the description under them.
const DOES_MIN_WIDTH: usize = 24;

#[derive(Debug, Default)]
pub(super) struct Help {
    top: usize,
    /// Text rows of the last frame drawn, what a page step moves by.
    page: usize,
    /// The last frame's text length, so a scroll never runs past the
    /// end before the next frame clamps it: the step back up would
    /// otherwise be spent on rows that were never shown.
    last_total: usize,
}

impl Help {
    pub(super) fn scroll(&mut self, delta: i32) {
        let top = self
            .top
            .saturating_add_signed(isize::try_from(delta).unwrap_or(0));
        self.top = top.min(self.bottom());
    }

    pub(super) fn page(&mut self, direction: i32) {
        let rows = i32::try_from(self.page.saturating_sub(1).max(1)).unwrap_or(i32::MAX);
        self.scroll(direction.saturating_mul(rows));
    }

    /// The highest `top` that still fills the last frame; before any
    /// frame there is no bound yet.
    fn bottom(&self) -> usize {
        if self.page == 0 {
            return usize::MAX;
        }
        self.last_total.saturating_sub(self.page)
    }

    pub(super) fn view<'a>(&'a mut self, theme: &'a Theme) -> HelpView<'a> {
        HelpView { help: self, theme }
    }
}

/// The help laid out at `width`: a heading per context, then its rows,
/// keys first and what they do beside them or, on a narrow frame, under
/// them.
fn lines(width: u16, heading: Style) -> Vec<Line<'static>> {
    let columns = usize::from(width);
    let keys_width = KEY_HELP
        .iter()
        .map(|row| row.keys.width())
        .max()
        .unwrap_or(0);
    let beside = columns >= INDENT + keys_width + GAP + DOES_MIN_WIDTH;
    let mut lines = Vec::new();
    let mut context = None;
    for HelpRow {
        context: row_context,
        keys,
        does,
    } in KEY_HELP
    {
        if context != Some(*row_context) {
            if context.is_some() {
                lines.push(Line::default());
            }
            lines.extend(render::plain_lines(row_context.title(), width, heading));
            context = Some(*row_context);
        }
        if beside {
            let pad = " ".repeat(keys_width + GAP - keys.width());
            let does_width = u16::try_from(columns - INDENT - keys_width - GAP).unwrap_or(u16::MAX);
            let margin = " ".repeat(INDENT + keys_width + GAP);
            for (i, line) in render::plain_lines(does, does_width, Style::new())
                .into_iter()
                .enumerate()
            {
                let lead = if i == 0 {
                    format!("{}{keys}{pad}", " ".repeat(INDENT))
                } else {
                    margin.clone()
                };
                let mut spans = vec![Span::raw(lead)];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
        } else {
            let indent = " ".repeat(INDENT);
            for keys_line in wrap_keys(keys, columns.saturating_sub(INDENT)) {
                lines.push(Line::from(format!("{indent}{keys_line}")));
            }
            let does_width = u16::try_from(columns.saturating_sub(2 * INDENT)).unwrap_or(u16::MAX);
            for line in render::plain_lines(does, does_width, Style::new()) {
                let mut spans = vec![Span::raw(indent.repeat(2))];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
        }
    }
    lines
}

/// A keys label broken between its ` / `-separated names to fit `width`,
/// each broken line keeping its trailing `/` so it reads on. A name wider
/// than `width` gets a line to itself.
fn wrap_keys(keys: &str, width: usize) -> Vec<String> {
    let mut wrapped: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut names = keys.split(" / ").peekable();
    while let Some(name) = names.next() {
        let more = if names.peek().is_some() {
            " /".width()
        } else {
            0
        };
        if current.is_empty() {
            current.push_str(name);
        } else if current.width() + " / ".width() + name.width() + more <= width {
            current.push_str(" / ");
            current.push_str(name);
        } else {
            current.push_str(" /");
            wrapped.push(std::mem::take(&mut current));
            current.push_str(name);
        }
    }
    wrapped.push(current);
    wrapped
}

pub(super) struct HelpView<'a> {
    help: &'a mut Help,
    theme: &'a Theme,
}

impl Widget for HelpView<'_> {
    /// `area` is the screen above the status line, the transcript and any
    /// todo panel; the pop-up sits inside it. Under the title a blank row
    /// sets the first heading apart from the border. With no row left for
    /// the body only the frame is drawn, so no position names lines that
    /// cannot be seen.
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area()).inner(MARGIN);
        if area.is_empty() {
            return;
        }
        let block = Block::bordered()
            .title(Span::styled(TITLE, self.theme.heading))
            .title_bottom(Line::styled(HINT, DIM));
        let inner = block.inner(area);
        let body = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        let text = lines(body.width, self.theme.heading);
        let total = text.len();
        let rows = usize::from(body.height);
        let help = self.help;
        help.page = rows.max(1);
        help.last_total = total;
        help.top = help.top.min(help.bottom());
        Clear.render(area, buf);
        if body.is_empty() {
            block.render(area, buf);
            return;
        }
        let start = help.top;
        let end = (start + rows).min(total);
        let position = format!("{}-{end}/{total}", (start + 1).min(end));
        block
            .title(Line::styled(position, DIM).right_aligned())
            .render(area, buf);
        let visible: Vec<Line<'static>> = text.into_iter().skip(start).take(rows).collect();
        Paragraph::new(visible).render(body, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// What the margin, the border and the blank row under the title take
    /// from the area the pop-up is drawn over.
    const CHROME_COLUMNS: u16 = 4;
    const CHROME_ROWS: usize = 5;

    fn draw(help: &mut Help, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::fallback(false);
        terminal
            .draw(|frame| frame.render_widget(help.view(&theme), frame.area()))
            .unwrap();
        terminal.backend().to_string()
    }

    /// Paging through the help from the top shows every line of it, and
    /// the lines hold every context's title and every row's keys whole,
    /// wrapped between names on a narrow frame but never cut.
    #[test]
    fn paging_through_shows_every_row() {
        for (width, height) in [(80, 24), (40, 12), (24, 12)] {
            let mut help = Help::default();
            let mut seen = String::new();
            for _ in 0..200 {
                seen.push_str(&draw(&mut help, width, height));
                help.page(1);
            }
            let body_width = width - CHROME_COLUMNS;
            let text: Vec<String> = lines(body_width, Style::new())
                .iter()
                .map(Line::to_string)
                .collect();
            for line in &text {
                assert!(line.width() <= usize::from(body_width), "{width}: {line:?}");
                assert!(seen.contains(line.trim_end()), "{width}: {line:?}");
            }
            let trimmed: Vec<&str> = text.iter().map(|line| line.trim()).collect();
            let whole = |name: &str| {
                trimmed
                    .iter()
                    .any(|line| line.starts_with(&format!("{name}  ")))
                    || (0..trimmed.len()).any(|start| {
                        (start + 1..=trimmed.len()).any(|end| trimmed[start..end].join(" ") == name)
                    })
            };
            for row in KEY_HELP {
                assert!(whole(row.context.title()), "{width}: {row:?}");
                assert!(whole(row.keys), "{width}: {row:?}");
            }
        }
    }

    /// The text never scrolls past its end or above its start, and a page
    /// step moves by the rows the last frame showed less one.
    #[test]
    fn scrolling_stays_within_the_text() {
        let rows = 24 - CHROME_ROWS;
        let total = lines(80 - CHROME_COLUMNS, Style::new()).len();
        let mut help = Help::default();
        help.scroll(-5);
        let top = draw(&mut help, 80, 24);
        assert_eq!(help.top, 0);
        assert!(top.contains(&format!("1-{rows}/{total}")), "{top}");
        help.page(1);
        assert_eq!(help.top, rows - 1);
        help.scroll(10_000);
        let bottom = draw(&mut help, 80, 24);
        assert_eq!(help.top, total - rows);
        assert!(bottom.contains(&format!("/{total}")), "{bottom}");
        assert!(bottom.contains("Close the help"), "{bottom}");
    }

    /// Steps past the end between two frames are not banked: the first
    /// step back up moves the text.
    #[test]
    fn a_step_up_after_scrolling_past_the_end_moves_at_once() {
        let mut help = Help::default();
        draw(&mut help, 80, 24);
        let total = lines(80 - CHROME_COLUMNS, Style::new()).len();
        let bottom = total - (24 - CHROME_ROWS);
        for _ in 0..50 {
            help.page(1);
            help.scroll(3);
        }
        assert_eq!(help.top, bottom);
        help.scroll(-1);
        assert_eq!(help.top, bottom - 1);
        let frame = draw(&mut help, 80, 24);
        let position = format!("{bottom}-{}/{total}", total - 1);
        assert!(frame.contains(&position), "{frame}");
    }

    /// The pop-up leaves a row and a column of what it is drawn over on
    /// each side, names itself and its position on the top border and the
    /// keys that move and close it on the bottom one, and keeps a blank
    /// row under the title before the first heading.
    #[test]
    fn the_pop_up_is_bordered_inside_a_margin_with_a_blank_row_under_the_title() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let theme = Theme::fallback(false);
        terminal
            .draw(|frame| {
                let area = frame.area();
                Paragraph::new(vec![Line::from("x".repeat(40)); 12])
                    .render(area, frame.buffer_mut());
                frame.render_widget(Help::default().view(&theme), area);
            })
            .unwrap();
        let rows: Vec<String> = (0..12)
            .map(|y| {
                let buffer = terminal.backend().buffer();
                (0..40).map(|x| buffer[(x, y)].symbol()).collect()
            })
            .collect();
        assert_eq!(rows[0], "x".repeat(40));
        assert_eq!(rows[11], "x".repeat(40));
        for row in &rows[1..11] {
            assert!(row.starts_with('x') && row.ends_with('x'), "{row:?}");
        }
        let total = lines(40 - CHROME_COLUMNS, Style::new()).len();
        assert!(rows[1].starts_with("x\u{250c}Keys"), "{:?}", rows[1]);
        assert!(
            rows[1].ends_with(&format!("1-7/{total}\u{2510}x")),
            "{:?}",
            rows[1]
        );
        assert_eq!(rows[2], format!("x\u{2502}{}\u{2502}x", " ".repeat(36)));
        assert!(rows[3].starts_with("x\u{2502}Anywhere "), "{:?}", rows[3]);
        assert!(
            rows[10].starts_with(&format!("x\u{2514}{HINT}")),
            "{:?}",
            rows[10]
        );
    }

    #[test]
    fn a_wide_frame_puts_what_a_key_does_beside_it() {
        let text: Vec<String> = lines(80, Style::new())
            .iter()
            .map(Line::to_string)
            .collect();
        assert_eq!(text[0], "Anywhere");
        let suspend = text
            .iter()
            .find(|line| line.contains("Ctrl+Z"))
            .expect("a Ctrl+Z row");
        assert!(suspend.ends_with("Suspend to the shell"), "{suspend}");
        let narrow: Vec<String> = lines(40, Style::new())
            .iter()
            .map(Line::to_string)
            .collect();
        let at = narrow.iter().position(|line| line == "  Ctrl+Z").unwrap();
        assert_eq!(narrow[at + 1], "    Suspend to the shell");
    }

    /// With no row left for the body the frame still shows, but no
    /// position for lines nobody can see.
    #[test]
    fn a_frame_with_no_room_for_the_body_shows_no_position() {
        let mut help = Help::default();
        let screen = draw(&mut help, 40, CHROME_ROWS as u16);
        let rows: Vec<&str> = screen.lines().collect();
        assert!(rows[1].contains("Keys"), "{:?}", rows[1]);
        assert!(!rows[1].contains('/'), "{:?}", rows[1]);
    }
}
