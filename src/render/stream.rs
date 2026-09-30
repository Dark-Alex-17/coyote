use super::{MarkdownRender, SseEvent};

use crate::utils::{AbortSignal, DrainOutcome, drain_terminal_events, spawn_spinner};

use anyhow::Result;
use crossterm::{
    cursor, queue, style,
    terminal::{self, disable_raw_mode, enable_raw_mode},
};
use std::{
    io::{self, Write, stdout},
    time::Duration,
};
use textwrap::core::display_width;
use tokio::sync::mpsc::UnboundedReceiver;
use unicode_width::UnicodeWidthChar;

pub async fn markdown_stream(
    rx: UnboundedReceiver<SseEvent>,
    render: &mut MarkdownRender,
    abort_signal: &AbortSignal,
) -> Result<()> {
    enable_raw_mode()?;

    let ret = markdown_stream_inner(rx, render, abort_signal).await;

    disable_raw_mode()?;

    if ret.is_err() {
        println!();
    }
    ret
}

pub async fn raw_stream(
    mut rx: UnboundedReceiver<SseEvent>,
    abort_signal: &AbortSignal,
) -> Result<()> {
    let mut spinner = Some(spawn_spinner("Generating"));

    loop {
        if abort_signal.aborted() {
            break;
        }
        match rx.recv().await {
            None => break,
            Some(evt) => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop();
                }

                match evt {
                    SseEvent::Text(text) => {
                        print!("{text}");
                        stdout().flush()?;
                    }
                    SseEvent::Done => {
                        break;
                    }
                }
            }
        }
    }
    if let Some(spinner) = spinner.take() {
        spinner.stop();
    }
    Ok(())
}

async fn markdown_stream_inner(
    mut rx: UnboundedReceiver<SseEvent>,
    render: &mut MarkdownRender,
    abort_signal: &AbortSignal,
) -> Result<()> {
    let mut painter = StreamPainter::new(stdout(), CrosstermProbe { abort_signal });

    let mut spinner = Some(spawn_spinner("Generating"));

    'outer: loop {
        if abort_signal.aborted() {
            break;
        }
        for reply_event in gather_events(&mut rx).await {
            if let Some(spinner) = spinner.take() {
                spinner.stop();
            }

            match reply_event {
                SseEvent::Text(text) => {
                    if painter.paint(render, &text)? {
                        break 'outer;
                    }
                }
                SseEvent::Done => {
                    painter.finish(render)?;
                    break 'outer;
                }
            }
        }

        if painter.poll_abort()? {
            break;
        }
    }

    if let Some(spinner) = spinner.take() {
        spinner.stop();
    }
    Ok(())
}

async fn gather_events(rx: &mut UnboundedReceiver<SseEvent>) -> Vec<SseEvent> {
    let mut texts = vec![];
    let mut done = false;
    tokio::select! {
        _ = async {
            while let Some(reply_event) = rx.recv().await {
                match reply_event {
                    SseEvent::Text(v) => texts.push(v),
                    SseEvent::Done => {
                        done = true;
                        break;
                    }
                }
            }
        } => {}
        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    let mut events = vec![];
    if !texts.is_empty() {
        events.push(SseEvent::Text(texts.join("")))
    }
    if done {
        events.push(SseEvent::Done)
    }
    events
}

trait TerminalProbe {
    fn size(&mut self) -> io::Result<(u16, u16)>;
    fn cursor(&mut self) -> io::Result<(u16, u16)>;
    fn drain(&mut self, timeout: Duration) -> Result<DrainOutcome>;
}

struct CrosstermProbe<'a> {
    abort_signal: &'a AbortSignal,
}

impl TerminalProbe for CrosstermProbe<'_> {
    fn size(&mut self) -> io::Result<(u16, u16)> {
        terminal::size()
    }

    fn cursor(&mut self) -> io::Result<(u16, u16)> {
        let mut attempts = 0;
        loop {
            match cursor::position() {
                Ok(pos) => return Ok(pos),
                Err(_) if attempts < 5 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn drain(&mut self, timeout: Duration) -> Result<DrainOutcome> {
        drain_terminal_events(self.abort_signal, timeout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaintPlan {
    start_row: u16,
    hidden: u16,
}

struct LastPaint {
    expected_row: u16,
    size: (u16, u16),
}

struct StreamPainter<W: Write, P: TerminalProbe> {
    writer: W,
    probe: P,
    buffer: String,
    buffer_rows: u16,
    painted_width: usize,
    suspended: bool,
    last_cursor: (u16, u16),
    last_paint: Option<LastPaint>,
    resized_since_paint: bool,
}

impl<W: Write, P: TerminalProbe> StreamPainter<W, P> {
    fn new(writer: W, probe: P) -> Self {
        Self {
            writer,
            probe,
            buffer: String::new(),
            buffer_rows: 1,
            painted_width: 0,
            suspended: false,
            last_cursor: (0, 0),
            last_paint: None,
            resized_since_paint: false,
        }
    }

    fn poll_abort(&mut self) -> Result<bool> {
        self.drain_events(Duration::from_millis(25))
    }

    fn drain_events(&mut self, timeout: Duration) -> Result<bool> {
        let outcome = self.probe.drain(timeout)?;
        self.resized_since_paint |= outcome.resized;
        Ok(outcome.aborted)
    }

    fn paint(&mut self, render: &mut MarkdownRender, text: &str) -> Result<bool> {
        if self.drain_events(Duration::ZERO)? {
            return Ok(true);
        }

        // tab width hacking
        let text = text.replace('\t', "    ");

        let (columns, rows) = self.observe_size()?;
        if rows <= 1 {
            self.buffer.push_str(&text);
            self.suspended = true;
        } else {
            self.paint_rows(render, &text, columns, rows)?;
        }
        Ok(false)
    }

    fn finish(&mut self, render: &mut MarkdownRender) -> Result<()> {
        if self.suspended {
            if self.drain_events(Duration::ZERO)? {
                return Ok(());
            }
            let (columns, rows) = self.observe_size()?;
            if rows > 1 {
                self.paint_rows(render, "", columns, rows)?;
            } else {
                // A one-row viewport can at best bound duplication to the current row.
                queue!(
                    self.writer,
                    cursor::MoveToColumn(0),
                    terminal::Clear(terminal::ClearType::CurrentLine),
                )?;
                let output = self.render_block(render, columns);
                print_rows(&mut self.writer, output.split('\n'))?;
                self.suspended = false;
            }
        }
        let tail = render.finalize();
        if !tail.is_empty() {
            queue!(self.writer, style::Print("\r\n"))?;
            print_rows(&mut self.writer, tail.split('\n'))?;
        }
        self.writer.flush()?;
        Ok(())
    }

    // A size change means the row we painted to no longer describes the screen.
    fn observe_size(&mut self) -> io::Result<(u16, u16)> {
        let size = self.probe.size()?;
        if self
            .last_paint
            .as_ref()
            .is_some_and(|last| last.size != size)
        {
            self.last_paint = None;
            self.resized_since_paint = false;
        }
        Ok(size)
    }

    fn paint_rows(
        &mut self,
        render: &mut MarkdownRender,
        text: &str,
        columns: u16,
        rows: u16,
    ) -> Result<()> {
        let row = self.locate(columns, rows);
        let plan = plan_paint(self.buffer_rows, row, rows);

        self.buffer.push_str(text);
        self.suspended = false;
        let output = self.render_block(render, columns);
        self.painted_width = display_width(&self.buffer);
        self.emit(&output, columns, rows, plan)
    }

    fn locate(&mut self, columns: u16, rows: u16) -> u16 {
        let (col, mut row) = match self.probe.cursor() {
            Ok(pos) => {
                self.last_cursor = pos;
                pos
            }
            Err(_) => self.last_cursor,
        };

        // Fix unexpected duplicate lines on kitty. Only the width painted last
        // batch describes the screen; text accumulated while suspended does not.
        if col == 0 && row > 0 && self.painted_width == columns as usize {
            row -= 1;
        }

        // A resize that left the size unchanged means the multiplexer moved the
        // cursor, not the content, so the row we painted to is more trustworthy.
        if self.resized_since_paint {
            self.resized_since_paint = false;
            if let Some(last) = &self.last_paint
                && last.size == (columns, rows)
                && last.expected_row != row
            {
                row = last.expected_row;
            }
        }
        row
    }

    fn render_block(&mut self, render: &mut MarkdownRender, columns: u16) -> String {
        let mut output = String::new();
        if self.buffer.contains('\n') {
            let (head, tail) = split_line_tail(&self.buffer);
            output = render.render(head);
            self.buffer = tail.to_string();
        }

        let line = render.render_line(&self.buffer);
        // No guarantee the buffer width of the buffer will not exceed the number of columns.
        // So we calculate the number of rows needed, rather than setting it directly to 1.
        self.buffer_rows = match line.rsplit_once('\n') {
            Some((head, tail)) => head.split('\n').count() as u16 + need_rows(tail, columns),
            None => need_rows(&line, columns),
        };

        if output.is_empty() {
            line
        } else {
            format!("{output}\n{line}")
        }
    }

    fn emit(&mut self, output: &str, columns: u16, rows: u16, plan: PaintPlan) -> Result<()> {
        // No guarantee that text returned by render will not be re-layouted, so it is better to clear it.
        queue!(
            self.writer,
            cursor::MoveTo(0, plan.start_row),
            terminal::Clear(terminal::ClearType::FromCursorDown),
        )?;

        // Rows above the viewport are already in history; skipping them keeps
        // every row of the turn entering history at most once.
        let screen = screen_rows(output, columns);
        let hidden = plan.hidden as usize;
        if hidden == 0 {
            print_rows(&mut self.writer, output.split('\n'))?;
        } else {
            print_rows(
                &mut self.writer,
                screen.iter().skip(hidden).map(String::as_str),
            )?;
        }
        self.writer.flush()?;

        let emitted = u16::try_from(screen.len().saturating_sub(hidden)).unwrap_or(u16::MAX);
        let expected_row = plan
            .start_row
            .saturating_add(emitted.saturating_sub(1))
            .min(rows.saturating_sub(1));
        self.last_paint = Some(LastPaint {
            expected_row,
            size: (columns, rows),
        });
        Ok(())
    }
}

fn plan_paint(prev_rows: u16, cursor_row: u16, term_rows: u16) -> PaintPlan {
    let cursor_row = cursor_row.min(term_rows.saturating_sub(1));
    PaintPlan {
        start_row: (cursor_row + 1).saturating_sub(prev_rows),
        hidden: prev_rows.saturating_sub(cursor_row + 1),
    }
}

fn screen_rows(output: &str, columns: u16) -> Vec<String> {
    let columns = columns.max(1) as usize;
    let mut rows = vec![];
    let mut row = String::new();
    let mut width = 0;
    let mut active_sgr: Vec<String> = vec![];
    let mut chars = output.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let seq = take_escape(&mut chars);
            row.push_str(&seq);
            if seq.starts_with("\x1b[") && seq.ends_with('m') {
                if seq == "\x1b[m" || seq == "\x1b[0m" {
                    active_sgr.clear();
                } else {
                    active_sgr.push(seq);
                }
            }
            continue;
        }
        if c == '\n' {
            rows.push(std::mem::replace(&mut row, active_sgr.concat()));
            width = 0;
            continue;
        }
        let w = c.width().unwrap_or(0);
        if w > 0 && width + w > columns {
            rows.push(std::mem::replace(&mut row, active_sgr.concat()));
            width = 0;
        }
        row.push(c);
        width += w;
    }
    rows.push(row);
    rows
}

// Mirrors textwrap's `skip_ansi_escape_sequence` so row cuts agree with `need_rows`.
fn take_escape(chars: &mut impl Iterator<Item = char>) -> String {
    let mut seq = String::from("\x1b");
    match chars.next() {
        Some('[') => {
            seq.push('[');
            for c in chars.by_ref() {
                seq.push(c);
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            seq.push(']');
            let mut last = ']';
            for c in chars.by_ref() {
                seq.push(c);
                if c == '\x07' || (c == '\\' && last == '\x1b') {
                    break;
                }
                last = c;
            }
        }
        Some(c) => seq.push(c),
        None => {}
    }
    seq
}

fn print_rows<'a, W: Write>(writer: &mut W, rows: impl Iterator<Item = &'a str>) -> Result<()> {
    for (i, row) in rows.enumerate() {
        if i > 0 {
            queue!(writer, style::Print("\n"), cursor::MoveToColumn(0))?;
        }
        queue!(writer, style::Print(row))?;
    }
    Ok(())
}

fn split_line_tail(text: &str) -> (&str, &str) {
    if let Some((head, tail)) = text.rsplit_once('\n') {
        (head, tail)
    } else {
        ("", text)
    }
}

fn need_rows(text: &str, columns: u16) -> u16 {
    let buffer_width = display_width(text).max(1) as u16;
    buffer_width.div_ceil(columns.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RenderOptions;

    struct ScriptedProbe {
        size: (u16, u16),
        cursor: (u16, u16),
        cursor_calls: usize,
        next_drain: DrainOutcome,
    }

    impl ScriptedProbe {
        fn new(size: (u16, u16), cursor: (u16, u16)) -> Self {
            Self {
                size,
                cursor,
                cursor_calls: 0,
                next_drain: DrainOutcome::default(),
            }
        }
    }

    impl TerminalProbe for ScriptedProbe {
        fn size(&mut self) -> io::Result<(u16, u16)> {
            Ok(self.size)
        }

        fn cursor(&mut self) -> io::Result<(u16, u16)> {
            self.cursor_calls += 1;
            Ok(self.cursor)
        }

        fn drain(&mut self, _timeout: Duration) -> Result<DrainOutcome> {
            Ok(std::mem::take(&mut self.next_drain))
        }
    }

    type TestPainter = StreamPainter<Vec<u8>, ScriptedProbe>;

    fn painter(size: (u16, u16), cursor: (u16, u16)) -> TestPainter {
        StreamPainter::new(Vec::new(), ScriptedProbe::new(size, cursor))
    }

    fn render() -> MarkdownRender {
        MarkdownRender::init(RenderOptions {
            raw_markdown: true,
            ..Default::default()
        })
        .unwrap()
    }

    fn sink(painter: &mut TestPainter) -> String {
        String::from_utf8(std::mem::take(&mut painter.writer)).unwrap()
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
        }
        out
    }

    fn csi_finals(text: &str) -> Vec<char> {
        let mut finals = vec![];
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        finals.push(c);
                        break;
                    }
                }
            }
        }
        finals
    }

    fn move_to(col: u16, row: u16) -> String {
        format!("\x1b[{};{}H", row + 1, col + 1)
    }

    /// Minimal ASCII terminal: autowrap with pending-wrap, scroll into history.
    struct Terminal {
        columns: usize,
        screen: Vec<Vec<char>>,
        history: Vec<String>,
        row: usize,
        col: usize,
    }

    impl Terminal {
        fn new(columns: usize, initial_rows: &[&str]) -> Self {
            Self {
                columns,
                screen: initial_rows.iter().map(|r| r.chars().collect()).collect(),
                history: vec![],
                row: initial_rows.len() - 1,
                col: 0,
            }
        }

        fn feed(&mut self, bytes: &str) {
            let mut chars = bytes.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\x1b' => {
                        assert_eq!(chars.next(), Some('['));
                        let mut params = String::new();
                        let final_byte = loop {
                            let c = chars.next().expect("unterminated CSI");
                            if ('\x40'..='\x7e').contains(&c) {
                                break c;
                            }
                            params.push(c);
                        };
                        match final_byte {
                            'H' => {
                                let (r, c) = params.split_once(';').unwrap();
                                self.row = r.parse::<usize>().unwrap() - 1;
                                self.col = c.parse::<usize>().unwrap() - 1;
                            }
                            'G' => self.col = params.parse::<usize>().unwrap() - 1,
                            'J' => {
                                self.screen[self.row].truncate(self.col);
                                for r in &mut self.screen[self.row + 1..] {
                                    r.clear();
                                }
                            }
                            'K' => {
                                assert_eq!(params, "2");
                                self.screen[self.row].clear();
                            }
                            'm' => {}
                            other => panic!("unexpected CSI {params}{other}"),
                        }
                    }
                    '\n' => self.line_feed(),
                    '\r' => self.col = 0,
                    c => {
                        if self.col == self.columns {
                            self.line_feed();
                            self.col = 0;
                        }
                        let row = &mut self.screen[self.row];
                        while row.len() <= self.col {
                            row.push(' ');
                        }
                        row[self.col] = c;
                        self.col += 1;
                    }
                }
            }
        }

        fn line_feed(&mut self) {
            if self.row + 1 == self.screen.len() {
                let top = self.screen.remove(0);
                self.history.push(top.into_iter().collect());
                self.screen.push(vec![]);
            } else {
                self.row += 1;
            }
        }

        fn screen(&self) -> Vec<String> {
            self.screen.iter().map(|r| r.iter().collect()).collect()
        }
    }

    #[test]
    fn normal_path_bytes_match_legacy_sequence() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 12));

        painter.paint(&mut render, "hel\tlo").unwrap();
        let expected = format!("{}\x1b[J{}", move_to(0, 12), "hel    lo");
        assert_eq!(sink(&mut painter), expected);
        assert_eq!(painter.buffer_rows, 1);

        painter.paint(&mut render, " world\nnext").unwrap();
        let expected = format!(
            "{}\x1b[J{}\n\x1b[1G{}",
            move_to(0, 12),
            "hel    lo world",
            "next"
        );
        assert_eq!(sink(&mut painter), expected);
    }

    #[test]
    fn suspends_all_output_on_single_row_viewport() {
        let mut render = render();
        let mut painter = painter((40, 1), (0, 0));
        let chunk = "lorem ipsum dolor sit amet, consectetur";
        for _ in 0..10 {
            assert!(!painter.paint(&mut render, chunk).unwrap());
        }
        assert!(painter.writer.is_empty());
        assert_eq!(painter.probe.cursor_calls, 0);
        assert_eq!(painter.buffer_rows, 1);
        assert_eq!(painter.buffer, chunk.repeat(10));
    }

    #[test]
    fn overflowing_block_never_scrolls_and_enters_history_once() {
        let mut render = render();
        let mut painter = painter((40, 3), (39, 2));
        let rows: Vec<String> = (0..6)
            .map(|i| char::from(b'a' + i).to_string().repeat(40))
            .collect();
        for row in &rows {
            painter.paint(&mut render, row).unwrap();
        }
        let out = sink(&mut painter);
        assert!(
            !csi_finals(&out).contains(&'S'),
            "ScrollUp emitted: {out:?}"
        );

        let mut term = Terminal::new(40, &["x", "y", ""]);
        term.feed(&out);
        assert_eq!(term.history, ["x", "y", &rows[0], &rows[1], &rows[2]]);
        assert_eq!(term.screen(), rows[3..]);
        assert_eq!(painter.buffer_rows, 6);
    }

    #[test]
    fn resume_paints_accumulated_buffer_once() {
        let mut render = render();
        let mut painter = painter((40, 1), (0, 0));
        let lines: Vec<String> = (0..10).map(|i| format!("para line {i}\n")).collect();
        for line in &lines {
            painter.paint(&mut render, line).unwrap();
        }
        assert!(painter.writer.is_empty());

        painter.probe.size = (40, 40);
        painter.probe.cursor = (0, 39);
        painter.paint(&mut render, "tail").unwrap();

        let out = sink(&mut painter);
        let finals = csi_finals(&out);
        assert_eq!(finals.iter().filter(|c| **c == 'H').count(), 1);
        assert!(out.starts_with(&move_to(0, 39)), "{out:?}");
        assert_eq!(strip_ansi(&out), format!("{}tail", lines.concat()));
        assert_eq!(painter.buffer, "tail");
        assert_eq!(painter.probe.cursor_calls, 1);
    }

    #[test]
    fn plan_paint_slices_hidden_prefix() {
        let plan = |prev, row, rows| {
            let p = plan_paint(prev, row, rows);
            (p.start_row, p.hidden)
        };
        assert_eq!(plan(3, 0, 1), (0, 2));
        assert_eq!(plan(3, 2, 40), (0, 0));
        assert_eq!(plan(41, 39, 40), (0, 1));
        assert_eq!(plan(2, 10, 40), (9, 0));
    }

    #[test]
    fn screen_rows_splits_at_columns_and_carries_style() {
        let styled = format!("\x1b[1m{}\x1b[0m", "x".repeat(100));
        let rows = screen_rows(&styled, 40);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], format!("\x1b[1m{}", "x".repeat(40)));
        assert_eq!(rows[1], format!("\x1b[1m{}", "x".repeat(40)));
        assert_eq!(rows[2], format!("\x1b[1m{}\x1b[0m", "x".repeat(20)));

        let combining = format!("{}e\u{301}", "a".repeat(39));
        let rows = screen_rows(&combining, 40);
        assert_eq!(rows, [combining]);

        assert_eq!(screen_rows("ab\ncd", 40), ["ab", "cd"]);
        assert_eq!(
            screen_rows("\x1b[31mab\ncd", 40),
            ["\x1b[31mab", "\x1b[31mcd"]
        );
    }

    #[test]
    fn screen_rows_keeps_osc_hyperlinks_unsplit() {
        let open = "\x1b]8;;https://example.com/a/rather/long/path\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        let link = format!("{open}{}{close}", "x".repeat(60));
        let rows = screen_rows(&link, 40);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.len(), need_rows(&link, 40) as usize);
        for row in &rows {
            assert_eq!(
                row.matches("\x1b]").count(),
                row.matches("\x1b\\").count(),
                "{row:?}"
            );
        }
        assert_eq!(rows[0], format!("{open}{}", "x".repeat(40)));
        assert_eq!(rows[1], format!("{}{close}", "x".repeat(20)));

        let bel_title = format!("\x1b]0;title\x07{}", "x".repeat(40));
        assert_eq!(screen_rows(&bel_title, 40), [bel_title]);

        let save_cursor = format!("\x1b7{}", "x".repeat(40));
        assert_eq!(screen_rows(&save_cursor, 40), [save_cursor]);
    }

    fn teleport_scenario(resized: bool, size_after: (u16, u16)) -> (TestPainter, MarkdownRender) {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 7));
        painter.paint(&mut render, "abc").unwrap();
        sink(&mut painter);

        painter.probe.size = size_after;
        painter.probe.cursor = (0, 39);
        painter.probe.next_drain = DrainOutcome {
            aborted: false,
            resized,
        };
        painter.paint(&mut render, "def").unwrap();
        (painter, render)
    }

    #[test]
    fn teleport_only_when_resized_without_size_change() {
        let (mut painter, mut render) = teleport_scenario(true, (40, 40));
        assert!(sink(&mut painter).starts_with(&move_to(0, 7)));
        painter.probe.cursor = (0, 20);
        painter.paint(&mut render, "ghi").unwrap();
        let out = sink(&mut painter);
        assert!(out.starts_with(&move_to(0, 20)), "{out:?}");

        let (mut painter, _) = teleport_scenario(false, (40, 40));
        assert!(sink(&mut painter).starts_with(&move_to(0, 39)));
        let (mut painter, _) = teleport_scenario(true, (40, 41));
        assert!(sink(&mut painter).starts_with(&move_to(0, 39)));
    }

    #[test]
    fn collapse_and_restore_drops_teleport_anchor() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 10));
        painter.paint(&mut render, "abc").unwrap();
        sink(&mut painter);

        painter.probe.size = (40, 1);
        painter.probe.next_drain = DrainOutcome {
            aborted: false,
            resized: true,
        };
        painter.paint(&mut render, "def").unwrap();
        assert!(painter.writer.is_empty());

        painter.probe.size = (40, 40);
        painter.probe.cursor = (0, 39);
        painter.probe.next_drain = DrainOutcome {
            aborted: false,
            resized: true,
        };
        painter.paint(&mut render, "ghi").unwrap();
        let out = sink(&mut painter);
        assert!(out.starts_with(&move_to(0, 39)), "{out:?}");
    }

    #[test]
    fn done_flushes_suspended_buffer_once_on_collapsed_viewport() {
        let mut render = render();
        let mut painter = painter((40, 1), (0, 0));
        painter.paint(&mut render, "hello world").unwrap();
        painter.finish(&mut render).unwrap();
        let out = sink(&mut painter);
        assert_eq!(out.matches("hello world").count(), 1);
        assert_eq!(painter.probe.cursor_calls, 0);
    }

    #[test]
    fn collapsed_finish_clears_row_before_reprinting_painted_prefix() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 2));
        painter.paint(&mut render, "hello").unwrap();
        let first = sink(&mut painter);
        painter.probe.size = (40, 1);
        painter.paint(&mut render, " world").unwrap();
        painter.finish(&mut render).unwrap();
        let out = sink(&mut painter);

        let clear = out
            .find("\x1b[2K")
            .expect("Clear(CurrentLine) before the flush");
        assert_eq!(strip_ansi(&out[clear..]), "hello world");
        assert_eq!(strip_ansi(&out).matches("hello").count(), 1);

        let mut term = Terminal::new(40, &["", "", ""]);
        term.feed(&first);
        term.feed(&out);
        assert_eq!(term.screen(), ["", "", "hello world"]);
    }

    #[test]
    fn done_flushes_suspended_buffer_via_normal_path_when_restored() {
        let mut render = render();
        let mut painter = painter((40, 1), (0, 0));
        painter.paint(&mut render, "hello world").unwrap();
        painter.probe.size = (40, 40);
        painter.probe.cursor = (0, 10);
        painter.finish(&mut render).unwrap();
        let out = sink(&mut painter);
        assert_eq!(out.matches("hello world").count(), 1);
        assert!(out.starts_with(&move_to(0, 10)), "{out:?}");
        assert_eq!(painter.probe.cursor_calls, 1);
    }

    #[test]
    fn finish_returns_carriage_before_finalize_tail() {
        let table = "| a | b |\n|---|---|\n| 1 | 2 |";
        let mut render = MarkdownRender::init(RenderOptions::default()).unwrap();
        let mut painter = painter((40, 40), (0, 0));
        painter.paint(&mut render, table).unwrap();
        painter.finish(&mut render).unwrap();
        let out = sink(&mut painter);

        let mut reference = MarkdownRender::init(RenderOptions::default()).unwrap();
        reference.render(split_line_tail(table).0);
        let tail = reference.finalize();
        assert!(!tail.is_empty());
        let mut expected = Vec::new();
        print_rows(&mut expected, tail.split('\n')).unwrap();
        let expected = String::from_utf8(expected).unwrap();
        assert!(out.ends_with(&format!("\r\n{expected}")), "{out:?}");
    }

    #[test]
    fn full_width_buffer_at_column_zero_backs_up_one_row() {
        let start_row_after = |first: &str| {
            let mut render = render();
            let mut painter = painter((40, 40), (0, 5));
            painter.paint(&mut render, first).unwrap();
            sink(&mut painter);
            painter.probe.cursor = (0, 10);
            painter.paint(&mut render, "y").unwrap();
            sink(&mut painter)
        };

        assert!(start_row_after(&"x".repeat(40)).starts_with(&move_to(0, 9)));
        assert!(start_row_after(&"x".repeat(39)).starts_with(&move_to(0, 10)));
    }

    #[test]
    fn buffer_rows_follow_column_changes() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 2));
        painter.paint(&mut render, &"x".repeat(100)).unwrap();
        assert_eq!(painter.buffer_rows, 3);
        sink(&mut painter);
        painter.probe.size = (50, 40);
        painter.paint(&mut render, "").unwrap();
        assert_eq!(painter.buffer_rows, 2);

        let mut term = Terminal::new(50, &["", "", ""]);
        term.feed(&sink(&mut painter));
        assert_eq!(
            term.screen(),
            ["x".repeat(50), "x".repeat(50), String::new()]
        );
    }

    #[test]
    fn kitty_hack_ignores_text_accumulated_while_suspended() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 2));
        painter.paint(&mut render, "hello").unwrap();
        sink(&mut painter);

        painter.probe.size = (40, 1);
        painter
            .paint(&mut render, &format!(" {}", "x".repeat(34)))
            .unwrap();
        assert_eq!(display_width(&painter.buffer), 40);
        assert!(painter.writer.is_empty());

        painter.probe.size = (40, 40);
        painter.probe.cursor = (0, 10);
        painter.paint(&mut render, "").unwrap();
        let out = sink(&mut painter);
        assert!(out.starts_with(&move_to(0, 10)), "{out:?}");
    }

    #[test]
    fn restore_overwrites_in_progress_rows_without_duplication() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 39));
        let block = "x".repeat(100);
        painter.paint(&mut render, &block).unwrap();
        assert_eq!(painter.buffer_rows, 3);
        let first = sink(&mut painter);

        painter.probe.size = (40, 1);
        painter.paint(&mut render, "\nmore").unwrap();
        painter.probe.size = (40, 40);
        painter.paint(&mut render, "").unwrap();
        let out = sink(&mut painter);
        assert!(out.starts_with(&move_to(0, 37)), "{out:?}");
        assert_eq!(strip_ansi(&out).matches(block.as_str()).count(), 1);

        let mut term = Terminal::new(40, &[""; 40]);
        term.feed(&first);
        term.feed(&out);
        let screen = term.screen();
        assert_eq!(
            screen[36..],
            [
                "x".repeat(40),
                "x".repeat(40),
                "x".repeat(20),
                "more".to_string()
            ]
        );
        assert!(term.history.iter().all(|row| !row.contains('x')));
    }

    #[test]
    fn abort_from_drain_stops_paint() {
        let mut render = render();
        let mut painter = painter((40, 40), (0, 0));
        painter.probe.next_drain = DrainOutcome {
            aborted: true,
            resized: false,
        };
        assert!(painter.paint(&mut render, "abc").unwrap());
        assert!(painter.writer.is_empty());
    }

    #[test]
    fn finish_honours_abort_from_drain() {
        let mut render = render();
        let mut painter = painter((40, 1), (0, 0));
        painter.paint(&mut render, "hello").unwrap();
        painter.probe.next_drain = DrainOutcome {
            aborted: true,
            resized: false,
        };
        painter.finish(&mut render).unwrap();
        assert!(painter.writer.is_empty());
        assert_eq!(painter.probe.cursor_calls, 0);
    }
}
