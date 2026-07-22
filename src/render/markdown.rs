use crate::utils::decode_bin;

use ansi_colours::AsRGB;
use anyhow::{Context, Result, anyhow};
use comfy_table::{CellAlignment, ContentArrangement, Table, presets::UTF8_FULL};
use crossterm::style::{Color, Stylize};
use crossterm::terminal;
use fancy_regex::{Captures, Regex};
use std::collections::HashMap;
use std::iter;
use std::sync::LazyLock;
use syntect::highlighting::{Color as SyntectColor, FontStyle, Style, Theme, ThemeItem};
use syntect::parsing::SyntaxSet;
use syntect::{easy::HighlightLines, parsing::SyntaxReference};

/// Comes from <https://github.com/sharkdp/bat/raw/5e77ca37e89c873e4490b42ff556370dc5c6ba4f/assets/syntaxes.bin>
const SYNTAXES: &[u8] = include_bytes!("../../assets/syntaxes.bin");

static LANG_MAPS: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("csharp".into(), "C#".into());
    m.insert("php".into(), "PHP Source".into());
    m
});

static HEADING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(#{1,6}) +.+").unwrap());
static BLOCKQUOTE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*>").unwrap());
static TASK_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*+] \[([ xX])\] +.+").unwrap());
static BULLET_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[-*+] +.+").unwrap());
static NUMBERED_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d+\. +.+").unwrap());
static HRULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(-{3,}|_{3,}|\*{3,})\s*$").unwrap());
static TABLE_SEPARATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\|(\s*:?-+:?\s*\|)+\s*$").unwrap());
static TABLE_ROW_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\|.*\|\s*$").unwrap());

static INLINE_CODE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)").unwrap());
static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
static BOLD_AST_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*\n]+)\*\*").unwrap());
static BOLD_US_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__([^_\n]+)__").unwrap());
static ITALIC_AST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?<![*\w])\*(?!\s)([^*\n]+?)(?<!\s)\*(?!\*)").unwrap());
static ITALIC_US_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?<![_\w])_(?!\s)([^_\n]+?)(?<!\s)_(?!_)").unwrap());
static STRIKETHROUGH_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~([^~\n]+)~~").unwrap());
static CODE_PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x00C(\d+)\x00").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Heading(u8),
    Blockquote,
    TaskItem(bool),
    BulletItem,
    NumberedItem,
    HorizontalRule,
    TableRow,
    TableSeparator,
    Paragraph,
}

fn detect_line_kind(line: &str) -> LineKind {
    if HRULE_RE.is_match(line).unwrap_or(false) {
        return LineKind::HorizontalRule;
    }
    if let Ok(Some(caps)) = HEADING_RE.captures(line) {
        let level = caps
            .get(1)
            .map(|m| m.as_str().len() as u8)
            .unwrap_or(1)
            .min(6);
        return LineKind::Heading(level);
    }
    if BLOCKQUOTE_RE.is_match(line).unwrap_or(false) {
        return LineKind::Blockquote;
    }
    if let Ok(Some(caps)) = TASK_ITEM_RE.captures(line) {
        let checked = caps.get(1).map(|m| m.as_str() != " ").unwrap_or(false);
        return LineKind::TaskItem(checked);
    }
    if BULLET_ITEM_RE.is_match(line).unwrap_or(false) {
        return LineKind::BulletItem;
    }
    if NUMBERED_ITEM_RE.is_match(line).unwrap_or(false) {
        return LineKind::NumberedItem;
    }
    if TABLE_SEPARATOR_RE.is_match(line).unwrap_or(false) {
        return LineKind::TableSeparator;
    }
    if TABLE_ROW_RE.is_match(line).unwrap_or(false) {
        return LineKind::TableRow;
    }

    LineKind::Paragraph
}

fn parse_table_row(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

fn parse_alignments(separator_row: &str) -> Vec<CellAlignment> {
    parse_table_row(separator_row)
        .iter()
        .map(|c| {
            let trimmed = c.trim();
            let starts = trimmed.starts_with(':');
            let ends = trimmed.ends_with(':');
            match (starts, ends) {
                (true, true) => CellAlignment::Center,
                (false, true) => CellAlignment::Right,
                _ => CellAlignment::Left,
            }
        })
        .collect()
}

fn regex_replace<F>(text: &str, re: &Regex, mut f: F) -> String
where
    F: FnMut(&Captures) -> String,
{
    let mut out = String::new();
    let mut last_end = 0;
    for caps_result in re.captures_iter(text) {
        let Ok(caps) = caps_result else { continue };
        let Some(whole) = caps.get(0) else { continue };
        out.push_str(&text[last_end..whole.start()]);
        out.push_str(&f(&caps));
        last_end = whole.end();
    }

    out.push_str(&text[last_end..]);
    out
}

fn wrap_osc8(url: &str, visible: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{visible}\x1b]8;;\x1b\\")
}

fn style_inline_code(content: &str, styles: &MarkdownStyles) -> String {
    let styled = content.with(styles.inline_code_fg);
    match styles.inline_code_bg {
        Some(bg) => styled.on(bg).to_string(),
        None => styled.to_string(),
    }
}

fn render_markdown_line(
    line: &str,
    kind: LineKind,
    styles: &MarkdownStyles,
    wrap_width: Option<u16>,
) -> String {
    match kind {
        LineKind::Heading(level) => render_heading(line, level, styles),
        LineKind::Blockquote => render_blockquote(line, styles, wrap_width),
        LineKind::BulletItem => render_bullet(line, styles, wrap_width),
        LineKind::NumberedItem => render_numbered(line, styles, wrap_width),
        LineKind::TaskItem(checked) => render_task(line, checked, styles, wrap_width),
        LineKind::HorizontalRule => render_hrule(styles),
        LineKind::TableRow | LineKind::TableSeparator => apply_inline(line, styles),
        LineKind::Paragraph => apply_inline(line, styles),
    }
}

fn kind_pre_wraps(kind: LineKind) -> bool {
    matches!(
        kind,
        LineKind::BulletItem
            | LineKind::NumberedItem
            | LineKind::TaskItem(_)
            | LineKind::Blockquote
    )
}

fn wrap_plain_content(content: &str, effective_width: usize) -> Vec<String> {
    let effective_width = effective_width.max(1);
    textwrap::wrap(content, effective_width)
        .into_iter()
        .map(|c| c.into_owned())
        .collect()
}

fn split_indent(line: &str) -> (&str, &str) {
    let indent_len = line
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    line.split_at(indent_len)
}

fn render_heading(line: &str, level: u8, styles: &MarkdownStyles) -> String {
    let (indent, rest) = split_indent(line);
    let content = rest.trim_start_matches('#').trim_start();
    let inline = apply_inline(content, styles);
    let (color, _force_bold) = styles.heading;

    let body = if level == 1 {
        format!(" {inline} ")
    } else {
        format!("{} {inline}", "#".repeat(level as usize))
    };

    format!("{indent}{}", body.with(color).bold())
}

fn render_blockquote(line: &str, styles: &MarkdownStyles, wrap_width: Option<u16>) -> String {
    let (indent, rest) = split_indent(line);
    let content = rest.trim_start_matches('>').trim_start();
    let prefix = "│ ".with(styles.blockquote).to_string();

    let render_one = |c: &str| apply_inline(c, styles).with(styles.blockquote).to_string();

    let Some(wrap_width) = wrap_width else {
        return format!("{indent}{prefix}{}", render_one(content));
    };

    let prefix_width = 2;
    let leading_width = indent.chars().count();
    let effective_width = (wrap_width as usize).saturating_sub(leading_width + prefix_width);
    let wrapped = wrap_plain_content(content, effective_width);
    if wrapped.is_empty() {
        return format!("{indent}{prefix}");
    }

    let mut out = String::new();
    for (i, chunk) in wrapped.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{indent}{prefix}{}", render_one(chunk)));
    }

    out
}

fn render_bullet(line: &str, styles: &MarkdownStyles, wrap_width: Option<u16>) -> String {
    let (indent, rest) = split_indent(line);
    let content = rest.get(2..).unwrap_or("");
    let bullet = "•".with(styles.list_bullet).to_string();

    let Some(wrap_width) = wrap_width else {
        return format!("{indent}{bullet} {}", apply_inline(content, styles));
    };

    let prefix_width = 2;
    let leading_width = indent.chars().count();
    let effective_width = (wrap_width as usize).saturating_sub(leading_width + prefix_width);
    let wrapped = wrap_plain_content(content, effective_width);
    if wrapped.is_empty() {
        return format!("{indent}{bullet} ");
    }

    let subseq = " ".repeat(prefix_width);
    let mut out = String::new();
    for (i, chunk) in wrapped.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let styled = apply_inline(chunk, styles);
        if i == 0 {
            out.push_str(&format!("{indent}{bullet} {styled}"));
        } else {
            out.push_str(&format!("{indent}{subseq}{styled}"));
        }
    }

    out
}

fn render_numbered(line: &str, styles: &MarkdownStyles, wrap_width: Option<u16>) -> String {
    let (indent, rest) = split_indent(line);
    let Some(dot_pos) = rest.find('.') else {
        return format!("{indent}{}", apply_inline(rest, styles));
    };
    let number = &rest[..dot_pos];
    let after = rest[dot_pos + 1..].trim_start();
    let styled_dot = ".".with(styles.list_bullet).to_string();

    let Some(wrap_width) = wrap_width else {
        return format!(
            "{indent}{number}{styled_dot} {}",
            apply_inline(after, styles)
        );
    };

    let prefix_width = number.chars().count() + 2;
    let leading_width = indent.chars().count();
    let effective_width = (wrap_width as usize).saturating_sub(leading_width + prefix_width);
    let wrapped = wrap_plain_content(after, effective_width);
    if wrapped.is_empty() {
        return format!("{indent}{number}{styled_dot} ");
    }

    let subseq = " ".repeat(prefix_width);
    let mut out = String::new();
    for (i, chunk) in wrapped.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let styled = apply_inline(chunk, styles);
        if i == 0 {
            out.push_str(&format!("{indent}{number}{styled_dot} {styled}"));
        } else {
            out.push_str(&format!("{indent}{subseq}{styled}"));
        }
    }

    out
}

fn render_task(
    line: &str,
    checked: bool,
    styles: &MarkdownStyles,
    wrap_width: Option<u16>,
) -> String {
    let (indent, rest) = split_indent(line);
    let after_bullet = rest.get(2..).unwrap_or("");
    let after_brackets = after_bullet.get(3..).map(str::trim_start).unwrap_or("");
    let glyph = if checked { "[✓]" } else { "[ ]" };
    let styled_brackets = glyph.with(styles.list_bullet).to_string();

    let Some(wrap_width) = wrap_width else {
        return format!(
            "{indent}{styled_brackets} {}",
            apply_inline(after_brackets, styles)
        );
    };

    let prefix_width = 4;
    let leading_width = indent.chars().count();
    let effective_width = (wrap_width as usize).saturating_sub(leading_width + prefix_width);
    let wrapped = wrap_plain_content(after_brackets, effective_width);
    if wrapped.is_empty() {
        return format!("{indent}{styled_brackets} ");
    }

    let subseq = " ".repeat(prefix_width);
    let mut out = String::new();
    for (i, chunk) in wrapped.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let styled = apply_inline(chunk, styles);
        if i == 0 {
            out.push_str(&format!("{indent}{styled_brackets} {styled}"));
        } else {
            out.push_str(&format!("{indent}{subseq}{styled}"));
        }
    }

    out
}

fn render_hrule(styles: &MarkdownStyles) -> String {
    "────────".with(styles.hrule).to_string()
}

fn colorize_box_chars(text: &str, color: Color) -> String {
    let sample = "X".with(color).to_string();
    let paint_idx = match sample.find('X') {
        Some(i) => i,
        None => return text.to_string(),
    };
    let prefix = &sample[..paint_idx];
    let suffix = &sample[paint_idx + 1..];
    if prefix.is_empty() && suffix.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut in_border = false;
    for c in text.chars() {
        let is_border = matches!(c, '\u{2500}'..='\u{257F}');
        if is_border && !in_border {
            out.push_str(prefix);
            in_border = true;
        } else if !is_border && in_border {
            out.push_str(suffix);
            in_border = false;
        }
        out.push(c);
    }

    if in_border {
        out.push_str(suffix);
    }

    out
}

fn apply_inline(text: &str, styles: &MarkdownStyles) -> String {
    let mut code_bank: Vec<String> = Vec::new();
    let masked = regex_replace(text, &INLINE_CODE_RE, |caps| {
        let content = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let idx = code_bank.len();
        code_bank.push(style_inline_code(content, styles));
        format!("\x00C{idx}\x00")
    });

    let with_images = regex_replace(&masked, &IMAGE_RE, |caps| {
        let alt = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let url = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let visible = format!("Image: {alt} → {url}")
            .with(styles.link_url)
            .to_string();
        wrap_osc8(url, &visible)
    });

    let with_links = regex_replace(&with_images, &LINK_RE, |caps| {
        let label = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let url = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let styled_label = label.with(styles.link_text).to_string();
        let styled_url = url.with(styles.link_url).to_string();
        wrap_osc8(url, &format!("{styled_label} {styled_url}"))
    });

    let with_bold = regex_replace(&with_links, &BOLD_AST_RE, |caps| {
        let content = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        content.with(styles.bold).bold().to_string()
    });
    let with_bold = regex_replace(&with_bold, &BOLD_US_RE, |caps| {
        let content = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        content.with(styles.bold).bold().to_string()
    });

    let with_italic = regex_replace(&with_bold, &ITALIC_AST_RE, |caps| {
        let content = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        content.with(styles.italic).italic().to_string()
    });
    let with_italic = regex_replace(&with_italic, &ITALIC_US_RE, |caps| {
        let content = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        content.with(styles.italic).italic().to_string()
    });

    let with_strike = regex_replace(&with_italic, &STRIKETHROUGH_RE, |caps| {
        let content = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        content.with(styles.strikethrough).crossed_out().to_string()
    });

    regex_replace(&with_strike, &CODE_PLACEHOLDER_RE, |caps| {
        let idx: usize = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        code_bank
            .get(idx)
            .cloned()
            .unwrap_or_else(|| caps.get(0).unwrap().as_str().to_string())
    })
}

enum TableState {
    PendingHeader(String),
    Active {
        header: Vec<String>,
        alignments: Vec<CellAlignment>,
        rows: Vec<Vec<String>>,
    },
}

enum TableAction {
    Consumed(String),
    ConsumedSilent,
    FlushAndContinue(String),
    Passthrough,
}

pub struct MarkdownRender {
    options: RenderOptions,
    syntax_set: SyntaxSet,
    code_color: Option<Color>,
    md_syntax: SyntaxReference,
    code_syntax: Option<SyntaxReference>,
    prev_line_type: LineType,
    wrap_width: Option<u16>,
    styles: MarkdownStyles,
    table_state: Option<TableState>,
}

impl MarkdownRender {
    pub fn init(options: RenderOptions) -> Result<Self> {
        let syntax_set: SyntaxSet =
            decode_bin(SYNTAXES).with_context(|| "MarkdownRender: invalid syntaxes binary")?;

        let code_color = options
            .theme
            .as_ref()
            .map(|theme| get_code_color(theme, options.truecolor));
        let md_syntax = syntax_set.find_syntax_by_extension("md").unwrap().clone();
        let line_type = LineType::Normal;
        let wrap_width = match options.wrap.as_deref() {
            None => None,
            Some(value) => match terminal::size() {
                Ok((columns, _)) => {
                    if value == "auto" {
                        Some(columns)
                    } else {
                        let value = value
                            .parse::<u16>()
                            .map_err(|_| anyhow!("Invalid wrap value"))?;
                        Some(columns.min(value))
                    }
                }
                Err(_) => None,
            },
        };
        let styles = MarkdownStyles::from_theme(options.theme.as_ref(), options.truecolor);
        Ok(Self {
            syntax_set,
            code_color,
            md_syntax,
            code_syntax: None,
            prev_line_type: line_type,
            wrap_width,
            styles,
            table_state: None,
            options,
        })
    }

    pub fn render(&mut self, text: &str) -> String {
        text.split('\n')
            .filter_map(|line| self.render_line_mut(line))
            .collect::<Vec<String>>()
            .join("\n")
    }

    pub fn render_line(&self, line: &str) -> String {
        let (_, line_kind, code_syntax, is_code) = self.check_line(line);
        if is_code {
            self.highlight_code_line(line, &code_syntax)
        } else if self.options.raw_markdown {
            self.highlight_line(line, &self.md_syntax, false)
        } else {
            self.render_rich_markdown_line(line, line_kind)
        }
    }

    fn render_line_mut(&mut self, line: &str) -> Option<String> {
        let (line_type, line_kind, code_syntax, is_code) = self.check_line(line);

        let table_prefix = if self.options.raw_markdown {
            None
        } else {
            let effective_kind = if is_code {
                LineKind::Paragraph
            } else {
                line_kind
            };
            match self.handle_table_state(line, effective_kind) {
                TableAction::Consumed(s) => {
                    self.prev_line_type = line_type;
                    self.code_syntax = code_syntax;
                    return Some(s);
                }
                TableAction::ConsumedSilent => {
                    self.prev_line_type = line_type;
                    self.code_syntax = code_syntax;
                    return None;
                }
                TableAction::FlushAndContinue(s) => Some(s),
                TableAction::Passthrough => None,
            }
        };

        let output = if is_code {
            self.highlight_code_line(line, &code_syntax)
        } else if self.options.raw_markdown {
            self.highlight_line(line, &self.md_syntax, false)
        } else {
            self.render_rich_markdown_line(line, line_kind)
        };
        self.prev_line_type = line_type;
        self.code_syntax = code_syntax;

        Some(match table_prefix {
            Some(prefix) => format!("{prefix}\n{output}"),
            None => output,
        })
    }

    fn render_as_paragraph(&self, line: &str) -> String {
        self.render_rich_markdown_line(line, LineKind::Paragraph)
    }

    fn handle_table_state(&mut self, line: &str, kind: LineKind) -> TableAction {
        match (self.table_state.take(), kind) {
            (None, LineKind::TableRow) => {
                self.table_state = Some(TableState::PendingHeader(line.to_string()));
                TableAction::ConsumedSilent
            }
            (None, LineKind::TableSeparator) => TableAction::Passthrough,
            (None, _) => TableAction::Passthrough,
            (Some(TableState::PendingHeader(header_line)), LineKind::TableSeparator) => {
                let header = parse_table_row(&header_line);
                let alignments = parse_alignments(line);
                self.table_state = Some(TableState::Active {
                    header,
                    alignments,
                    rows: Vec::new(),
                });
                TableAction::ConsumedSilent
            }
            (Some(TableState::PendingHeader(header_line)), LineKind::TableRow) => {
                let a = self.render_as_paragraph(&header_line);
                let b = self.render_as_paragraph(line);
                TableAction::Consumed(format!("{a}\n{b}"))
            }
            (Some(TableState::PendingHeader(header_line)), _) => {
                let flushed = self.render_as_paragraph(&header_line);
                TableAction::FlushAndContinue(flushed)
            }
            (
                Some(TableState::Active {
                    header,
                    alignments,
                    mut rows,
                }),
                LineKind::TableRow,
            ) => {
                rows.push(parse_table_row(line));
                self.table_state = Some(TableState::Active {
                    header,
                    alignments,
                    rows,
                });
                TableAction::ConsumedSilent
            }
            (
                Some(TableState::Active {
                    header,
                    alignments,
                    rows,
                }),
                _,
            ) => {
                let rendered = self.render_table(header, alignments, rows);
                TableAction::FlushAndContinue(rendered)
            }
        }
    }

    pub fn finalize(&mut self) -> String {
        match self.table_state.take() {
            None => String::new(),
            Some(TableState::PendingHeader(line)) => self.render_as_paragraph(&line),
            Some(TableState::Active {
                header,
                alignments,
                rows,
            }) => self.render_table(header, alignments, rows),
        }
    }

    fn render_rich_markdown_line(&self, line: &str, kind: LineKind) -> String {
        let styled = render_markdown_line(line, kind, &self.styles, self.wrap_width);
        if kind_pre_wraps(kind) {
            styled
        } else {
            self.wrap_line(styled, false)
        }
    }

    fn render_table(
        &self,
        header: Vec<String>,
        alignments: Vec<CellAlignment>,
        rows: Vec<Vec<String>>,
    ) -> String {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL);
        table.set_content_arrangement(ContentArrangement::Dynamic);
        if let Some(width) = self.wrap_width {
            table.set_width(width);
        }

        let (heading_color, _) = self.styles.heading;
        let styled_header: Vec<String> = header
            .iter()
            .map(|c| {
                apply_inline(c, &self.styles)
                    .with(heading_color)
                    .bold()
                    .to_string()
            })
            .collect();
        table.set_header(styled_header);

        for (i, align) in alignments.iter().enumerate() {
            if let Some(col) = table.column_mut(i) {
                col.set_cell_alignment(*align);
            }
        }

        for row in rows {
            let styled_row: Vec<String> =
                row.iter().map(|c| apply_inline(c, &self.styles)).collect();
            table.add_row(styled_row);
        }

        colorize_box_chars(&table.to_string(), self.styles.table_border)
    }

    fn check_line(&self, line: &str) -> (LineType, LineKind, Option<SyntaxReference>, bool) {
        let mut line_type = self.prev_line_type;
        let mut code_syntax = self.code_syntax.clone();
        let mut is_code = false;
        if let Some(lang) = detect_code_block(line) {
            match line_type {
                LineType::Normal | LineType::CodeEnd => {
                    line_type = LineType::CodeBegin;
                    code_syntax = if lang.is_empty() {
                        None
                    } else {
                        self.find_syntax(&lang).cloned()
                    };
                }
                LineType::CodeBegin | LineType::CodeInner => {
                    line_type = LineType::CodeEnd;
                    code_syntax = None;
                }
            }
        } else {
            match line_type {
                LineType::Normal => {}
                LineType::CodeEnd => {
                    line_type = LineType::Normal;
                }
                LineType::CodeBegin => {
                    if code_syntax.is_none()
                        && let Some(syntax) = self.syntax_set.find_syntax_by_first_line(line)
                    {
                        code_syntax = Some(syntax.clone());
                    }
                    line_type = LineType::CodeInner;
                    is_code = true;
                }
                LineType::CodeInner => {
                    is_code = true;
                }
            }
        }
        let line_kind = if is_code {
            LineKind::Paragraph
        } else {
            detect_line_kind(line)
        };

        (line_type, line_kind, code_syntax, is_code)
    }

    fn highlight_line(&self, line: &str, syntax: &SyntaxReference, is_code: bool) -> String {
        let ws: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        let trimmed_line: &str = &line[ws.len()..];
        let mut line_highlighted = None;
        if let Some(theme) = &self.options.theme {
            let mut highlighter = HighlightLines::new(syntax, theme);
            if let Ok(ranges) = highlighter.highlight_line(trimmed_line, &self.syntax_set) {
                line_highlighted = Some(format!(
                    "{ws}{}",
                    as_terminal_escaped(&ranges, self.options.truecolor)
                ))
            }
        }
        let line = line_highlighted.unwrap_or_else(|| line.into());
        self.wrap_line(line, is_code)
    }

    fn highlight_code_line(&self, line: &str, code_syntax: &Option<SyntaxReference>) -> String {
        if let Some(syntax) = code_syntax {
            self.highlight_line(line, syntax, true)
        } else {
            let line = match self.code_color {
                Some(color) => line.with(color).to_string(),
                None => line.to_string(),
            };
            self.wrap_line(line, true)
        }
    }

    fn wrap_line(&self, line: String, is_code: bool) -> String {
        if let Some(width) = self.wrap_width {
            if is_code && !self.options.wrap_code {
                return line;
            }
            wrap(&line, width as usize)
        } else {
            line
        }
    }

    fn find_syntax(&self, lang: &str) -> Option<&SyntaxReference> {
        if let Some(new_lang) = LANG_MAPS.get(&lang.to_ascii_lowercase()) {
            self.syntax_set.find_syntax_by_name(new_lang)
        } else {
            self.syntax_set
                .find_syntax_by_token(lang)
                .or_else(|| self.syntax_set.find_syntax_by_extension(lang))
        }
    }
}

fn wrap(text: &str, width: usize) -> String {
    let indent: usize = text.chars().take_while(|c| *c == ' ').count();
    let wrap_options = textwrap::Options::new(width)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
        .initial_indent(&text[0..indent]);
    textwrap::wrap(&text[indent..], wrap_options).join("\n")
}

#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    pub theme: Option<Theme>,
    pub wrap: Option<String>,
    pub wrap_code: bool,
    pub raw_markdown: bool,
    pub truecolor: bool,
}

impl RenderOptions {
    pub(crate) fn new(
        theme: Option<Theme>,
        wrap: Option<String>,
        wrap_code: bool,
        raw_markdown: bool,
        truecolor: bool,
    ) -> Self {
        Self {
            theme,
            wrap,
            wrap_code,
            raw_markdown,
            truecolor,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineType {
    Normal,
    CodeBegin,
    CodeInner,
    CodeEnd,
}

fn as_terminal_escaped(ranges: &[(Style, &str)], truecolor: bool) -> String {
    let mut output = String::new();
    for (style, text) in ranges {
        let fg = blend_fg_color(style.foreground, style.background);
        let mut text = text.with(convert_color(fg, truecolor));
        if style.font_style.contains(FontStyle::BOLD) {
            text = text.bold();
        }
        if style.font_style.contains(FontStyle::UNDERLINE) {
            text = text.underlined();
        }
        output.push_str(&text.to_string());
    }
    output
}

fn convert_color(c: SyntectColor, truecolor: bool) -> Color {
    if truecolor {
        Color::Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    } else {
        let value = (c.r, c.g, c.b).to_ansi256();
        // lower contrast
        let value = match value {
            7 | 15 | 231 | 252..=255 => 252,
            _ => value,
        };
        Color::AnsiValue(value)
    }
}

fn blend_fg_color(fg: SyntectColor, bg: SyntectColor) -> SyntectColor {
    if fg.a == 0xff {
        return fg;
    }
    let ratio = u32::from(fg.a);
    let r = (u32::from(fg.r) * ratio + u32::from(bg.r) * (255 - ratio)) / 255;
    let g = (u32::from(fg.g) * ratio + u32::from(bg.g) * (255 - ratio)) / 255;
    let b = (u32::from(fg.b) * ratio + u32::from(bg.b) * (255 - ratio)) / 255;
    SyntectColor {
        r: u8::try_from(r).unwrap_or(u8::MAX),
        g: u8::try_from(g).unwrap_or(u8::MAX),
        b: u8::try_from(b).unwrap_or(u8::MAX),
        a: 255,
    }
}

fn detect_code_block(line: &str) -> Option<String> {
    let line = line.trim_start();
    if !line.starts_with("```") {
        return None;
    }
    let lang = line
        .chars()
        .skip(3)
        .take_while(|v| !v.is_whitespace())
        .collect();
    Some(lang)
}

fn get_code_color(theme: &Theme, truecolor: bool) -> Color {
    let scope = theme.scopes.iter().find(|v| {
        v.scope
            .selectors
            .iter()
            .any(|v| v.path.scopes.iter().any(|v| v.to_string() == "string"))
    });
    scope
        .and_then(|v| v.style.foreground)
        .map_or_else(|| Color::Yellow, |c| convert_color(c, truecolor))
}

#[derive(Debug, Clone, Copy, Default)]
struct ResolvedStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    font_style: FontStyle,
}

fn find_theme_scope<'a>(theme: &'a Theme, name: &str) -> Option<&'a ThemeItem> {
    theme.scopes.iter().find(|item| {
        item.scope
            .selectors
            .iter()
            .any(|sel| sel.path.scopes.iter().any(|s| s.to_string() == name))
    })
}

fn resolve_scope_style(
    theme: &Theme,
    primary: &str,
    fallbacks: &[&str],
    truecolor: bool,
) -> ResolvedStyle {
    for scope_name in iter::once(primary).chain(fallbacks.iter().copied()) {
        let Some(item) = find_theme_scope(theme, scope_name) else {
            continue;
        };
        let resolved = ResolvedStyle {
            fg: item.style.foreground.map(|c| convert_color(c, truecolor)),
            bg: item.style.background.map(|c| convert_color(c, truecolor)),
            font_style: item.style.font_style.unwrap_or_default(),
        };

        if resolved.fg.is_some() || resolved.bg.is_some() || !resolved.font_style.is_empty() {
            return resolved;
        }
    }

    ResolvedStyle::default()
}

#[derive(Debug, Clone)]
pub struct MarkdownStyles {
    heading: (Color, bool),
    bold: Color,
    italic: Color,
    inline_code_fg: Color,
    inline_code_bg: Option<Color>,
    blockquote: Color,
    list_bullet: Color,
    link_text: Color,
    link_url: Color,
    strikethrough: Color,
    hrule: Color,
    table_border: Color,
}

impl MarkdownStyles {
    fn from_theme(theme: Option<&Theme>, truecolor: bool) -> Self {
        let Some(theme) = theme else {
            return Self::none();
        };

        let heading = resolve_scope_style(
            theme,
            "markup.heading",
            &["markup.bold", "entity.name.section"],
            truecolor,
        );
        let bold = resolve_scope_style(
            theme,
            "markup.bold",
            &["entity.other.attribute-name"],
            truecolor,
        );
        let italic = resolve_scope_style(theme, "markup.italic", &["comment"], truecolor);
        let inline_code = resolve_scope_style(
            theme,
            "markup.raw.inline",
            &["markup.raw", "string"],
            truecolor,
        );
        let blockquote =
            resolve_scope_style(theme, "markup.quote", &["comment", "string"], truecolor);
        let list_bullet = resolve_scope_style(
            theme,
            "markup.list",
            &["punctuation.definition.list", "keyword"],
            truecolor,
        );
        let link_text = resolve_scope_style(
            theme,
            "markup.link",
            &["string.other.link", "entity.name.tag"],
            truecolor,
        );
        let link_url = resolve_scope_style(
            theme,
            "markup.underline.link",
            &["string.other.link", "constant"],
            truecolor,
        );
        let strikethrough = resolve_scope_style(theme, "markup.deleted", &["invalid"], truecolor);
        let hrule = resolve_scope_style(theme, "comment", &["punctuation"], truecolor);
        let table_border = resolve_scope_style(
            theme,
            "punctuation.definition.table.markdown",
            &["punctuation", "meta.separator"],
            truecolor,
        );

        Self {
            heading: (
                heading.fg.unwrap_or(Color::Yellow),
                heading.font_style.contains(FontStyle::BOLD),
            ),
            bold: bold.fg.unwrap_or(Color::Reset),
            italic: italic.fg.unwrap_or(Color::Reset),
            inline_code_fg: inline_code.fg.unwrap_or(Color::Yellow),
            inline_code_bg: inline_code.bg,
            blockquote: blockquote.fg.unwrap_or(Color::DarkGrey),
            list_bullet: list_bullet.fg.unwrap_or(Color::Reset),
            link_text: link_text.fg.unwrap_or(Color::Cyan),
            link_url: link_url.fg.unwrap_or(Color::Blue),
            strikethrough: strikethrough.fg.unwrap_or(Color::DarkGrey),
            hrule: hrule.fg.unwrap_or(Color::DarkGrey),
            table_border: table_border.fg.unwrap_or(Color::DarkGrey),
        }
    }

    fn none() -> Self {
        Self {
            heading: (Color::Reset, false),
            bold: Color::Reset,
            italic: Color::Reset,
            inline_code_fg: Color::Reset,
            inline_code_bg: None,
            blockquote: Color::Reset,
            list_bullet: Color::Reset,
            link_text: Color::Reset,
            link_url: Color::Reset,
            strikethrough: Color::Reset,
            hrule: Color::Reset,
            table_border: Color::Reset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"
To unzip a file in Rust, you can use the `zip` crate. Here's an example code that shows how to unzip a file:

```rust
use std::fs::File;

fn unzip_file(path: &str, output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
```
"#;
    const TEXT_NO_WRAP_CODE: &str = r#"
To unzip a file in Rust, you can use the `zip` crate. Here's an example code
that shows how to unzip a file:

```rust
use std::fs::File;

fn unzip_file(path: &str, output_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
```
"#;

    const TEXT_WRAP_ALL: &str = r#"
To unzip a file in Rust, you can use the `zip` crate. Here's an example code
that shows how to unzip a file:

```rust
use std::fs::File;

fn unzip_file(path: &str, output_dir: &str) -> Result<(), Box<dyn
std::error::Error>> {
    todo!()
}
```
"#;

    #[test]
    fn test_render() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        assert!(render.find_syntax("csharp").is_some());
    }

    #[test]
    fn no_theme() {
        let options = RenderOptions {
            raw_markdown: true,
            ..Default::default()
        };
        let mut render = MarkdownRender::init(options).unwrap();
        let output = render.render(TEXT);
        assert_eq!(TEXT, output);
    }

    #[test]
    fn no_wrap_code() {
        let options = RenderOptions {
            raw_markdown: true,
            ..Default::default()
        };
        let mut render = MarkdownRender::init(options).unwrap();
        render.wrap_width = Some(80);
        let output = render.render(TEXT);
        assert_eq!(TEXT_NO_WRAP_CODE, output);
    }

    #[test]
    fn wrap_all() {
        let options = RenderOptions {
            wrap_code: true,
            raw_markdown: true,
            ..Default::default()
        };
        let mut render = MarkdownRender::init(options).unwrap();
        render.wrap_width = Some(80);
        let output = render.render(TEXT);
        assert_eq!(TEXT_WRAP_ALL, output);
    }

    #[test]
    fn test_detect_code_block() {
        assert_eq!(detect_code_block("```rust"), Some("rust".into()));
        assert_eq!(detect_code_block("```c++"), Some("c++".into()));
        assert_eq!(detect_code_block("  ```rust"), Some("rust".into()));
        assert_eq!(detect_code_block("```"), Some("".into()));
        assert_eq!(detect_code_block("``rust"), None);
    }

    use std::str::FromStr;
    use syntect::highlighting::{ScopeSelectors, StyleModifier, ThemeItem};

    const DARK_THEME_BYTES: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");

    fn load_dark_theme() -> Theme {
        decode_bin(DARK_THEME_BYTES).expect("built-in dark theme should decode")
    }

    fn syntect_rgb(r: u8, g: u8, b: u8) -> SyntectColor {
        SyntectColor { r, g, b, a: 255 }
    }

    fn theme_item(scope: &str, fg: SyntectColor, font_style: Option<FontStyle>) -> ThemeItem {
        ThemeItem {
            scope: ScopeSelectors::from_str(scope).expect("valid scope selector"),
            style: StyleModifier {
                foreground: Some(fg),
                background: None,
                font_style,
            },
        }
    }

    fn minimal_root_scope_theme() -> Theme {
        let mut theme = Theme::default();
        theme
            .scopes
            .push(theme_item("string", syntect_rgb(0xff, 0xdd, 0x00), None));
        theme
            .scopes
            .push(theme_item("comment", syntect_rgb(0x88, 0x88, 0x88), None));
        theme
            .scopes
            .push(theme_item("keyword", syntect_rgb(0xaa, 0x00, 0xff), None));
        theme
            .scopes
            .push(theme_item("constant", syntect_rgb(0x00, 0xcc, 0xff), None));
        theme.scopes.push(theme_item(
            "entity.name.tag",
            syntect_rgb(0x11, 0x22, 0x33),
            None,
        ));
        theme.scopes.push(theme_item(
            "entity.other.attribute-name",
            syntect_rgb(0x44, 0x55, 0x66),
            Some(FontStyle::BOLD),
        ));
        theme.scopes.push(theme_item(
            "entity.name.section",
            syntect_rgb(0xde, 0xad, 0xbe),
            Some(FontStyle::BOLD),
        ));
        theme
            .scopes
            .push(theme_item("invalid", syntect_rgb(0xff, 0x00, 0x00), None));
        theme.scopes.push(theme_item(
            "punctuation",
            syntect_rgb(0x77, 0x77, 0x77),
            None,
        ));
        theme
    }

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb { r, g, b }
    }

    #[test]
    fn resolve_scope_style_uses_primary() {
        let mut theme = Theme::default();
        theme.scopes.push(theme_item(
            "markup.bold",
            syntect_rgb(0xaa, 0xbb, 0xcc),
            Some(FontStyle::BOLD),
        ));

        let resolved = resolve_scope_style(&theme, "markup.bold", &["fallback"], true);

        assert_eq!(resolved.fg, Some(rgb(0xaa, 0xbb, 0xcc)));
        assert!(resolved.font_style.contains(FontStyle::BOLD));
    }

    #[test]
    fn resolve_scope_style_falls_back_when_primary_missing() {
        let mut theme = Theme::default();
        theme
            .scopes
            .push(theme_item("comment", syntect_rgb(0x33, 0x44, 0x55), None));

        let resolved = resolve_scope_style(&theme, "markup.italic", &["nope", "comment"], true);

        assert_eq!(resolved.fg, Some(rgb(0x33, 0x44, 0x55)));
    }

    #[test]
    fn resolve_scope_style_returns_default_when_nothing_matches() {
        let theme = Theme::default();

        let resolved = resolve_scope_style(&theme, "markup.italic", &["comment"], true);

        assert!(resolved.fg.is_none());
        assert!(resolved.bg.is_none());
        assert!(resolved.font_style.is_empty());
    }

    #[test]
    fn markdown_styles_none_when_theme_absent() {
        let styles = MarkdownStyles::from_theme(None, true);

        assert_eq!(styles.heading, (Color::Reset, false));
        assert_eq!(styles.bold, Color::Reset);
        assert_eq!(styles.italic, Color::Reset);
        assert_eq!(styles.inline_code_fg, Color::Reset);
        assert_eq!(styles.inline_code_bg, None);
        assert_eq!(styles.blockquote, Color::Reset);
        assert_eq!(styles.list_bullet, Color::Reset);
        assert_eq!(styles.link_text, Color::Reset);
        assert_eq!(styles.link_url, Color::Reset);
        assert_eq!(styles.strikethrough, Color::Reset);
        assert_eq!(styles.hrule, Color::Reset);
        assert_eq!(styles.table_border, Color::Reset);
    }

    #[test]
    fn markdown_styles_resolve_with_builtin_dark_theme() {
        let theme = load_dark_theme();
        let styles = MarkdownStyles::from_theme(Some(&theme), true);

        assert_ne!(styles.heading.0, Color::Reset);
        assert_ne!(styles.bold, Color::Reset);
        assert_ne!(styles.italic, Color::Reset);
        assert_ne!(styles.inline_code_fg, Color::Reset);
        assert_ne!(styles.blockquote, Color::Reset);
        assert_ne!(styles.list_bullet, Color::Reset);
        assert_ne!(styles.link_text, Color::Reset);
        assert_ne!(styles.link_url, Color::Reset);
        assert_ne!(styles.strikethrough, Color::Reset);
        assert_ne!(styles.hrule, Color::Reset);
        assert_ne!(styles.table_border, Color::Reset);
    }

    #[test]
    fn detect_line_kind_heading_levels() {
        assert_eq!(detect_line_kind("# H1"), LineKind::Heading(1));
        assert_eq!(detect_line_kind("## H2"), LineKind::Heading(2));
        assert_eq!(detect_line_kind("### H3"), LineKind::Heading(3));
        assert_eq!(detect_line_kind("#### H4"), LineKind::Heading(4));
        assert_eq!(detect_line_kind("##### H5"), LineKind::Heading(5));
        assert_eq!(detect_line_kind("###### H6"), LineKind::Heading(6));
        assert_eq!(detect_line_kind("  ## Indented"), LineKind::Heading(2));
    }

    #[test]
    fn detect_line_kind_heading_requires_space_after_hashes() {
        assert_eq!(detect_line_kind("##notheading"), LineKind::Paragraph);
        assert_eq!(detect_line_kind("#"), LineKind::Paragraph);
    }

    #[test]
    fn detect_line_kind_blockquote() {
        assert_eq!(detect_line_kind("> quoted"), LineKind::Blockquote);
        assert_eq!(detect_line_kind(">no space"), LineKind::Blockquote);
        assert_eq!(detect_line_kind("  > indented"), LineKind::Blockquote);
        assert_eq!(detect_line_kind(">"), LineKind::Blockquote);
    }

    #[test]
    fn detect_line_kind_task_item() {
        assert_eq!(detect_line_kind("- [ ] todo"), LineKind::TaskItem(false));
        assert_eq!(detect_line_kind("- [x] done"), LineKind::TaskItem(true));
        assert_eq!(detect_line_kind("- [X] done"), LineKind::TaskItem(true));
        assert_eq!(detect_line_kind("* [ ] todo"), LineKind::TaskItem(false));
        assert_eq!(
            detect_line_kind("  - [x] indented"),
            LineKind::TaskItem(true)
        );
    }

    #[test]
    fn detect_line_kind_bullet_item() {
        assert_eq!(detect_line_kind("- item"), LineKind::BulletItem);
        assert_eq!(detect_line_kind("* item"), LineKind::BulletItem);
        assert_eq!(detect_line_kind("+ item"), LineKind::BulletItem);
        assert_eq!(detect_line_kind("  - nested"), LineKind::BulletItem);
        assert_eq!(detect_line_kind("-nospace"), LineKind::Paragraph);
    }

    #[test]
    fn detect_line_kind_numbered_item() {
        assert_eq!(detect_line_kind("1. first"), LineKind::NumberedItem);
        assert_eq!(detect_line_kind("42. answer"), LineKind::NumberedItem);
        assert_eq!(detect_line_kind("  3. nested"), LineKind::NumberedItem);
        assert_eq!(detect_line_kind("1.nospace"), LineKind::Paragraph);
    }

    #[test]
    fn detect_line_kind_horizontal_rule() {
        assert_eq!(detect_line_kind("---"), LineKind::HorizontalRule);
        assert_eq!(detect_line_kind("___"), LineKind::HorizontalRule);
        assert_eq!(detect_line_kind("***"), LineKind::HorizontalRule);
        assert_eq!(detect_line_kind("--------"), LineKind::HorizontalRule);
        assert_eq!(detect_line_kind("  ---"), LineKind::HorizontalRule);
        assert_eq!(detect_line_kind("---  "), LineKind::HorizontalRule);
        assert_eq!(detect_line_kind("--"), LineKind::Paragraph);
    }

    #[test]
    fn detect_line_kind_paragraph_default() {
        assert_eq!(detect_line_kind("just text"), LineKind::Paragraph);
        assert_eq!(detect_line_kind(""), LineKind::Paragraph);
        assert_eq!(detect_line_kind("   "), LineKind::Paragraph);
    }

    #[test]
    fn detect_line_kind_table_row() {
        assert_eq!(detect_line_kind("| a | b |"), LineKind::TableRow);
        assert_eq!(detect_line_kind("|a|b|c|"), LineKind::TableRow);
        assert_eq!(detect_line_kind("  | a | b |  "), LineKind::TableRow);
        assert_eq!(detect_line_kind("| | |"), LineKind::TableRow);
    }

    #[test]
    fn detect_line_kind_table_separator() {
        assert_eq!(detect_line_kind("|---|---|"), LineKind::TableSeparator);
        assert_eq!(detect_line_kind("| --- | --- |"), LineKind::TableSeparator);
        assert_eq!(
            detect_line_kind("|:---|---:|:---:|---|"),
            LineKind::TableSeparator,
        );
        assert_eq!(detect_line_kind("|:--|--:|:-:|"), LineKind::TableSeparator);
        assert_eq!(detect_line_kind("  |---|---|  "), LineKind::TableSeparator);
    }

    #[test]
    fn detect_line_kind_non_table_pipe_line_stays_paragraph() {
        assert_eq!(detect_line_kind("use `a | b` for or"), LineKind::Paragraph,);
        assert_eq!(detect_line_kind("| trailing"), LineKind::Paragraph);
        assert_eq!(detect_line_kind("no closer |"), LineKind::Paragraph);
    }

    #[test]
    fn detect_line_kind_prefers_separator_over_row() {
        assert_eq!(detect_line_kind("|---|---|"), LineKind::TableSeparator);
        assert_ne!(detect_line_kind("|---|---|"), LineKind::TableRow);
    }

    #[test]
    fn parse_table_row_splits_cells() {
        assert_eq!(parse_table_row("| a | b | c |"), vec!["a", "b", "c"]);
        assert_eq!(parse_table_row("|a|b|c|"), vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_table_row_handles_empty_cells() {
        assert_eq!(parse_table_row("| a | | c |"), vec!["a", "", "c"]);
        assert_eq!(parse_table_row("| | | |"), vec!["", "", ""]);
    }

    #[test]
    fn parse_table_row_trims_whitespace() {
        assert_eq!(
            parse_table_row("  |   foo   |   bar   |  "),
            vec!["foo", "bar"],
        );
    }

    #[test]
    fn parse_alignments_reads_colons() {
        assert_eq!(
            parse_alignments("|:---|---:|:---:|---|"),
            vec![
                CellAlignment::Left,
                CellAlignment::Right,
                CellAlignment::Center,
                CellAlignment::Left,
            ],
        );
    }

    #[test]
    fn parse_alignments_short_dashes() {
        assert_eq!(
            parse_alignments("|:--|--:|:-:|"),
            vec![
                CellAlignment::Left,
                CellAlignment::Right,
                CellAlignment::Center,
            ],
        );
    }

    #[test]
    fn parse_alignments_defaults_to_left() {
        assert_eq!(
            parse_alignments("|---|---|---|"),
            vec![
                CellAlignment::Left,
                CellAlignment::Left,
                CellAlignment::Left,
            ],
        );
    }

    #[test]
    fn colorize_box_chars_wraps_border_runs() {
        let input = "┌─┐\nabc\n└─┘";

        let output = colorize_box_chars(input, Color::Red);

        assert!(
            output.starts_with("\x1b["),
            "border run starts with SGR: {output:?}"
        );
        assert!(output.contains("abc"), "non-border content preserved");
        assert!(output.contains("┌"));
        assert!(output.contains("└"));
    }

    #[test]
    fn colorize_box_chars_leaves_reset_color_untouched() {
        let output = colorize_box_chars("no borders here", Color::Red);

        assert_eq!(output, "no borders here");
    }

    #[test]
    fn render_table_renders_headers_rows_and_borders() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["A".into(), "B".into(), "C".into()];
        let alignments = vec![
            CellAlignment::Left,
            CellAlignment::Left,
            CellAlignment::Left,
        ];
        let rows = vec![
            vec!["1".into(), "2".into(), "3".into()],
            vec!["4".into(), "5".into(), "6".into()],
        ];

        let output = render.render_table(header, alignments, rows);

        for expected in ["A", "B", "C", "1", "2", "3", "4", "5", "6"] {
            assert!(
                output.contains(expected),
                "cell {expected:?} in output: {output:?}"
            );
        }
        assert!(
            output.chars().any(|c| matches!(c, '\u{2500}'..='\u{257F}')),
            "table has box-drawing chars: {output:?}",
        );
    }

    #[test]
    fn render_table_header_uses_bold() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["Header".into()];
        let alignments = vec![CellAlignment::Left];

        let output = render.render_table(header, alignments, vec![]);

        assert!(output.contains("\x1b[1m"), "bold SGR present: {output:?}");
        assert!(output.contains("Header"));
    }

    #[test]
    fn render_table_applies_inline_markdown_to_cells() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["H".into()];
        let alignments = vec![CellAlignment::Left];
        let rows = vec![vec!["**bold**".into()]];

        let output = render.render_table(header, alignments, rows);

        assert!(
            !output.contains("**bold**"),
            "asterisks stripped from cell: {output:?}",
        );
        assert!(output.contains("bold"));
        assert!(output.contains("\x1b[1m"), "bold SGR present: {output:?}");
    }

    #[test]
    fn render_table_respects_alignment_specifiers() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["L".into(), "R".into(), "C".into()];
        let alignments = vec![
            CellAlignment::Left,
            CellAlignment::Right,
            CellAlignment::Center,
        ];
        let rows = vec![vec!["a".into(), "b".into(), "c".into()]];

        let output = render.render_table(header, alignments, rows);

        for expected in ["L", "R", "C", "a", "b", "c"] {
            assert!(output.contains(expected), "cell {expected:?} present");
        }
    }

    #[test]
    fn render_table_handles_wide_chars_and_emoji() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["Name".into()];
        let alignments = vec![CellAlignment::Left];
        let rows = vec![vec!["🎉".into()], vec!["日本".into()]];

        let output = render.render_table(header, alignments, rows);

        assert!(output.contains("🎉"));
        assert!(output.contains("日本"));
        assert!(output.lines().count() > 4, "multi-line output: {output:?}");
    }

    #[test]
    fn render_table_borders_wrapped_in_border_color() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["a".into()];
        let alignments = vec![CellAlignment::Left];

        let output = render.render_table(header, alignments, vec![vec!["b".into()]]);

        assert!(
            output.starts_with("\x1b["),
            "output starts with border color SGR: {output:?}",
        );
    }

    #[test]
    fn state_machine_renders_full_table_and_flushes_on_paragraph() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n\nafter\n";

        let output = render.render(text);

        for cell in ["A", "B", "1", "2"] {
            assert!(output.contains(cell), "cell {cell:?} rendered: {output:?}");
        }
        assert!(
            output.chars().any(|c| matches!(c, '\u{2500}'..='\u{257F}')),
            "output has box-drawing chars: {output:?}",
        );
        assert!(output.contains("after"), "trailing paragraph preserved");
    }

    #[test]
    fn state_machine_defers_output_until_flush() {
        let options = RenderOptions::default();

        let mut render = MarkdownRender::init(options).unwrap();

        let header = render.render_line_mut("| A | B |");
        assert!(header.is_none(), "header row silently consumed");
        let sep = render.render_line_mut("|---|---|");
        assert!(sep.is_none(), "separator silently consumed");
        let data = render.render_line_mut("| 1 | 2 |");
        assert!(data.is_none(), "data row silently consumed");
    }

    #[test]
    fn finalize_emits_pending_active_table() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        render.render_line_mut("| A | B |");
        render.render_line_mut("|---|---|");
        render.render_line_mut("| 1 | 2 |");

        let tail = render.finalize();

        assert!(tail.contains("A"));
        assert!(tail.contains("1"));
        assert!(tail.contains("2"));
        assert!(tail.chars().any(|c| matches!(c, '\u{2500}'..='\u{257F}')));
    }

    #[test]
    fn finalize_flushes_pending_header_as_paragraph() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        render.render_line_mut("| A | B |");

        let tail = render.finalize();

        assert!(tail.contains("A"));
        assert!(tail.contains("B"));
        assert!(tail.contains("|"), "raw pipes preserved: {tail:?}");
    }

    #[test]
    fn finalize_is_empty_when_no_pending_table() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        render.render_line_mut("plain text");
        assert!(render.finalize().is_empty());
    }

    #[test]
    fn pipe_row_without_separator_flushes_as_paragraphs() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "| A | B |\n| C | D |\nafter\n";

        let output = render.render(text);

        assert!(
            output.contains("| A | B |"),
            "raw pipes preserved for first: {output:?}",
        );
        assert!(
            output.contains("| C | D |"),
            "raw pipes preserved for second: {output:?}",
        );
        assert!(output.contains("after"));
    }

    #[test]
    fn multiple_tables_in_one_input() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "| A |\n|---|\n| 1 |\n\n| B |\n|---|\n| 2 |\n";
        let output = render.render(text);
        let tail = render.finalize();

        let combined = format!("{output}{tail}");

        for cell in ["A", "B", "1", "2"] {
            assert!(combined.contains(cell), "cell {cell:?}: {combined:?}");
        }
    }

    #[test]
    fn render_line_immutable_does_not_mutate_table_state() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();

        let _ = render.render_line("| foo | ba");

        assert!(
            render.table_state.is_none(),
            "render_line is immutable; state stays clean",
        );
    }

    #[test]
    fn raw_markdown_mode_bypasses_table_rendering() {
        let options = RenderOptions {
            raw_markdown: true,
            ..Default::default()
        };
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "| A | B |\n|---|---|\n| 1 | 2 |\n";

        let output = render.render(text);

        assert!(
            output.contains("| A | B |"),
            "raw pipes preserved: {output:?}",
        );
        assert!(render.table_state.is_none(), "no state entered in raw mode",);
    }

    #[test]
    fn bullet_wraps_with_two_space_hanging_indent() {
        let styles = test_styles();
        let line = "- text that is long enough to wrap onto continuation lines";

        let output = render_markdown_line(line, LineKind::BulletItem, &styles, Some(20));

        assert!(output.contains('\n'), "wrapped output: {output:?}");
        let lines: Vec<&str> = output.split('\n').collect();
        assert!(lines.len() >= 2);
        for cont in &lines[1..] {
            assert!(
                cont.starts_with("  ") && !cont.starts_with("  •"),
                "continuation has 2-space indent (no bullet): {cont:?}",
            );
        }
    }

    #[test]
    fn numbered_wraps_with_digit_width_hanging_indent() {
        let styles = test_styles();
        let line = "42. text that is long enough to wrap onto continuation lines";

        let output = render_markdown_line(line, LineKind::NumberedItem, &styles, Some(22));

        assert!(output.contains('\n'), "wrapped output: {output:?}");
        let lines: Vec<&str> = output.split('\n').collect();
        for cont in &lines[1..] {
            assert!(
                cont.starts_with("    "),
                "4-space indent for `42. `: {cont:?}"
            );
        }
    }

    #[test]
    fn numbered_wraps_with_three_digit_hanging_indent() {
        let styles = test_styles();
        let line = "100. text that is long enough to wrap onto continuation lines";

        let output = render_markdown_line(line, LineKind::NumberedItem, &styles, Some(22));

        assert!(output.contains('\n'));
        let lines: Vec<&str> = output.split('\n').collect();
        for cont in &lines[1..] {
            assert!(
                cont.starts_with("     "),
                "5-space indent for `100. `: {cont:?}",
            );
        }
    }

    #[test]
    fn task_wraps_with_four_space_hanging_indent() {
        let styles = test_styles();
        let line = "- [ ] task text that is long enough to wrap around";

        let output = render_markdown_line(line, LineKind::TaskItem(false), &styles, Some(22));

        assert!(output.contains('\n'), "wrapped output: {output:?}");
        let lines: Vec<&str> = output.split('\n').collect();
        for cont in &lines[1..] {
            assert!(
                cont.starts_with("    "),
                "4-space indent for `[ ] `: {cont:?}"
            );
        }
    }

    #[test]
    fn blockquote_wraps_with_pipe_prefix_on_every_line() {
        let styles = test_styles();
        let line = "> quoted text that is long enough to wrap onto multiple continuation lines";
        let output = render_markdown_line(line, LineKind::Blockquote, &styles, Some(24));

        assert!(output.contains('\n'), "wrapped output: {output:?}");

        for wrapped in output.split('\n') {
            assert!(
                wrapped.contains("│ "),
                "pipe prefix present on line: {wrapped:?}",
            );
        }
    }

    #[test]
    fn bullet_preserves_leading_indent_when_wrapping() {
        let styles = test_styles();
        let line = "  - nested bullet text that wraps around a few times";

        let output = render_markdown_line(line, LineKind::BulletItem, &styles, Some(22));

        assert!(output.contains('\n'), "wrapped output: {output:?}");
        for wrapped in output.split('\n') {
            assert!(
                wrapped.starts_with("  "),
                "leading indent preserved: {wrapped:?}",
            );
        }
    }

    #[test]
    fn block_renderers_produce_single_line_when_wrap_width_none() {
        let styles = test_styles();
        let long_line =
            "- very long bullet text that would definitely wrap if a narrow wrap_width were set";

        let output = render_markdown_line(long_line, LineKind::BulletItem, &styles, None);

        assert!(!output.contains('\n'), "no wrapping with None: {output:?}");
    }

    #[test]
    fn bullet_wraps_with_inline_markdown_intact() {
        let styles = test_styles();
        let line = "- **bold** text with `code` that will wrap onto several lines";

        let output = render_markdown_line(line, LineKind::BulletItem, &styles, Some(22));

        assert!(output.contains('\n'), "wrapped output: {output:?}");
        assert!(
            !output.contains("**bold**"),
            "asterisks stripped: {output:?}"
        );
        assert!(!output.contains("`code`"), "backticks stripped: {output:?}");
        assert!(output.contains("bold"));
        assert!(output.contains("code"));
    }

    #[test]
    fn mixed_content_renders_all_kinds() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "# Heading\n\n\
                    Some paragraph.\n\n\
                    - bullet one\n\
                    - bullet two\n\n\
                    > a quote\n\n\
                    | A | B |\n\
                    |---|---|\n\
                    | 1 | 2 |\n\n\
                    Trailing prose.\n";
        let body = render.render(text);
        let tail = render.finalize();

        let output = format!("{body}{tail}");

        assert!(output.contains("Heading"), "heading rendered");
        assert!(output.contains("Some paragraph."));
        assert!(output.contains("•"), "bullet glyph rendered");
        assert!(output.contains("│"), "blockquote pipe rendered");
        assert!(
            output.contains("A") && output.contains("1"),
            "table cells rendered"
        );
        assert!(
            output.chars().any(|c| matches!(c, '\u{2500}'..='\u{257F}')),
            "table borders rendered",
        );
        assert!(output.contains("Trailing prose."));
    }

    #[test]
    fn table_renders_without_theme() {
        let options = RenderOptions {
            theme: None,
            ..Default::default()
        };
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["A".into()];
        let alignments = vec![CellAlignment::Left];

        let output = render.render_table(header, alignments, vec![vec!["1".into()]]);

        assert!(output.contains("A"));
        assert!(output.contains("1"));
        assert!(
            output.chars().any(|c| matches!(c, '\u{2500}'..='\u{257F}')),
            "borders present without theme: {output:?}",
        );
    }

    #[test]
    fn table_borders_pick_up_theme_color() {
        let theme = minimal_root_scope_theme();
        let styles = MarkdownStyles::from_theme(Some(&theme), true);
        assert_eq!(styles.table_border, rgb(0x77, 0x77, 0x77));

        let options = RenderOptions {
            theme: Some(theme),
            ..Default::default()
        };
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["A".into()];
        let alignments = vec![CellAlignment::Left];
        let output = render.render_table(header, alignments, vec![vec!["1".into()]]);
        assert!(
            output.starts_with("\x1b["),
            "border color SGR at start: {output:?}",
        );
    }

    #[test]
    fn table_render_does_not_emit_blank_lines_from_silent_accumulation() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "## Head\n\n\
                    | A | B |\n\
                    |---|---|\n\
                    | 1 | 2 |\n\
                    | 3 | 4 |\n\
                    | 5 | 6 |\n\
                    | 7 | 8 |\n\
                    | 9 | 10 |\n\n\
                    trailing\n";
        let output = render.render(text);
        let tail = render.finalize();
        let combined = format!("{output}{tail}");
        let mut consecutive_blank = 0usize;
        let mut max_consecutive_blank = 0usize;
        for line in combined.split('\n') {
            if line.is_empty() {
                consecutive_blank += 1;
                max_consecutive_blank = max_consecutive_blank.max(consecutive_blank);
            } else {
                consecutive_blank = 0;
            }
        }
        assert!(
            max_consecutive_blank <= 1,
            "at most one blank line between blocks, got {max_consecutive_blank}: {combined:?}",
        );
    }

    #[test]
    fn table_tolerates_column_count_mismatch() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();
        let header = vec!["A".into(), "B".into(), "C".into()];
        let alignments = vec![
            CellAlignment::Left,
            CellAlignment::Left,
            CellAlignment::Left,
        ];
        let rows = vec![vec!["1".into(), "2".into()]];

        let output = render.render_table(header, alignments, rows);

        for cell in ["A", "B", "C", "1", "2"] {
            assert!(output.contains(cell), "cell {cell:?} present: {output:?}");
        }
    }

    fn test_styles() -> MarkdownStyles {
        MarkdownStyles {
            heading: (Color::Yellow, true),
            bold: Color::Red,
            italic: Color::Green,
            inline_code_fg: Color::Cyan,
            inline_code_bg: None,
            blockquote: Color::DarkGrey,
            list_bullet: Color::Reset,
            link_text: Color::Blue,
            link_url: Color::Magenta,
            strikethrough: Color::DarkGrey,
            hrule: Color::DarkGrey,
            table_border: Color::DarkGrey,
        }
    }

    #[test]
    fn inline_code_strips_backticks() {
        let styles = test_styles();

        let result = apply_inline("hello `world` foo", &styles);

        assert!(!result.contains('`'), "backticks stripped: {result:?}");
        assert!(result.contains("world"));
        assert!(result.contains("hello "));
        assert!(result.contains(" foo"));
    }

    #[test]
    fn bold_asterisk_applied() {
        let styles = test_styles();

        let result = apply_inline("**loud**", &styles);

        assert!(!result.contains("**"), "markers stripped: {result:?}");
        assert!(result.contains("loud"));
        assert!(result.contains("\x1b[1m"), "bold SGR present: {result:?}");
    }

    #[test]
    fn bold_underscore_applied() {
        let styles = test_styles();

        let result = apply_inline("__loud__", &styles);

        assert!(!result.contains("__"), "markers stripped: {result:?}");
        assert!(result.contains("loud"));
        assert!(result.contains("\x1b[1m"));
    }

    #[test]
    fn italic_asterisk_applied() {
        let styles = test_styles();

        let result = apply_inline("*soft*", &styles);

        assert!(result.contains("soft"));
        assert!(result.contains("\x1b[3m"), "italic SGR present: {result:?}");
    }

    #[test]
    fn italic_underscore_applied() {
        let styles = test_styles();

        let result = apply_inline("_soft_", &styles);

        assert!(result.contains("soft"));
        assert!(result.contains("\x1b[3m"));
    }

    #[test]
    fn strikethrough_applied() {
        let styles = test_styles();

        let result = apply_inline("~~gone~~", &styles);

        assert!(!result.contains("~~"), "markers stripped");
        assert!(result.contains("gone"));
        assert!(result.contains("\x1b[9m"), "strikethrough SGR: {result:?}");
    }

    #[test]
    fn bold_wraps_inline_code() {
        let styles = test_styles();

        let result = apply_inline("**foo `bar` baz**", &styles);

        assert!(!result.contains('`'));
        assert!(!result.contains("**"));
        assert!(result.contains("foo"));
        assert!(result.contains("bar"));
        assert!(result.contains("baz"));
        assert!(
            result.contains("\x1b[1m"),
            "bold applied around code: {result:?}"
        );
    }

    #[test]
    fn partial_bold_stays_raw() {
        let styles = test_styles();

        let result = apply_inline("**unclosed", &styles);

        assert_eq!(result, "**unclosed");
    }

    #[test]
    fn partial_italic_stays_raw() {
        let styles = test_styles();

        assert_eq!(apply_inline("*unclosed", &styles), "*unclosed");
        assert_eq!(apply_inline("_unclosed", &styles), "_unclosed");
    }

    #[test]
    fn italic_ignores_word_internal_underscores() {
        let styles = test_styles();

        assert_eq!(apply_inline("some_var_name", &styles), "some_var_name");
    }

    #[test]
    fn italic_ignores_math_like_spaces() {
        let styles = test_styles();

        assert_eq!(apply_inline("a * b * c", &styles), "a * b * c");
    }

    #[test]
    fn links_emit_osc8_and_visible_parts() {
        let styles = test_styles();

        let result = apply_inline("[label](https://example.com)", &styles);

        assert!(
            result.contains("\x1b]8;;https://example.com\x1b\\"),
            "OSC 8 open present: {result:?}"
        );
        assert!(result.contains("\x1b]8;;\x1b\\"), "OSC 8 close present");
        assert!(result.contains("label"));
        assert!(result.contains("https://example.com"));
    }

    #[test]
    fn images_emit_labeled_link() {
        let styles = test_styles();

        let result = apply_inline("![alt text](https://img.example/x.png)", &styles);

        assert!(
            !result.contains("!["),
            "raw image marker removed: {result:?}"
        );
        assert!(result.contains("Image: alt text"));
        assert!(result.contains("https://img.example/x.png"));
        assert!(result.contains("\x1b]8;;https://img.example/x.png\x1b\\"));
    }

    #[test]
    fn image_processed_before_link() {
        let styles = test_styles();

        let result = apply_inline("![alt](https://example.com)", &styles);

        assert!(
            !result.starts_with('!'),
            "no stray ! left behind: {result:?}"
        );
        assert!(result.contains("Image:"));
    }

    #[test]
    fn plain_text_unchanged() {
        let styles = test_styles();

        assert_eq!(apply_inline("just plain text", &styles), "just plain text");
    }

    #[test]
    fn render_heading_level_1_pads_content() {
        let styles = test_styles();

        let result = render_markdown_line("# Big", LineKind::Heading(1), &styles, None);

        assert!(result.contains(" Big "), "H1 padded content: {result:?}");
        assert!(!result.contains('#'), "H1 hashes removed: {result:?}");
        assert!(result.contains("\x1b[1m"), "bold applied: {result:?}");
    }

    #[test]
    fn render_heading_level_2_through_6_keeps_hash_prefix() {
        let styles = test_styles();

        for level in 2u8..=6 {
            let hashes = "#".repeat(level as usize);
            let line = format!("{hashes} Title");
            let result = render_markdown_line(&line, LineKind::Heading(level), &styles, None);
            assert!(
                result.contains(&hashes),
                "H{level} keeps hashes: {result:?}"
            );
            assert!(result.contains("Title"));
            assert!(result.contains("\x1b[1m"), "H{level} bold: {result:?}");
        }
    }

    #[test]
    fn render_heading_preserves_leading_indent() {
        let styles = test_styles();

        let result = render_markdown_line("  ## Nested", LineKind::Heading(2), &styles, None);

        assert!(result.starts_with("  "), "indent preserved: {result:?}");
    }

    #[test]
    fn render_blockquote_uses_pipe_prefix() {
        let styles = test_styles();

        let result = render_markdown_line("> quoted", LineKind::Blockquote, &styles, None);

        assert!(result.contains("│ "), "pipe prefix: {result:?}");
        assert!(!result.contains('>'), "gt removed: {result:?}");
        assert!(result.contains("quoted"));
    }

    #[test]
    fn render_blockquote_preserves_indent() {
        let styles = test_styles();

        let result = render_markdown_line("  > deep", LineKind::Blockquote, &styles, None);

        assert!(result.starts_with("  "));
        assert!(result.contains("│ "));
    }

    #[test]
    fn render_bullet_uses_bullet_char() {
        let styles = test_styles();

        let result = render_markdown_line("- item", LineKind::BulletItem, &styles, None);

        assert!(result.contains("•"), "bullet glyph: {result:?}");
        assert!(!result.contains("- "), "dash removed: {result:?}");
        assert!(result.contains("item"));
    }

    #[test]
    fn render_bullet_supports_star_and_plus() {
        let styles = test_styles();

        let star = render_markdown_line("* one", LineKind::BulletItem, &styles, None);
        let plus = render_markdown_line("+ two", LineKind::BulletItem, &styles, None);

        assert!(star.contains("•") && star.contains("one"));
        assert!(plus.contains("•") && plus.contains("two"));
    }

    #[test]
    fn render_bullet_preserves_nested_indent() {
        let styles = test_styles();

        let result = render_markdown_line("    - nested", LineKind::BulletItem, &styles, None);

        assert!(result.starts_with("    "), "indent kept: {result:?}");
        assert!(result.contains("•"));
    }

    #[test]
    fn render_numbered_preserves_number_and_styles_dot() {
        let styles = test_styles();

        let result = render_markdown_line("42. answer", LineKind::NumberedItem, &styles, None);

        assert!(result.contains("42"), "number kept: {result:?}");
        assert!(result.contains("answer"));
        assert!(result.contains('.'), "dot present");
    }

    #[test]
    fn render_task_unchecked() {
        let styles = test_styles();

        let result = render_markdown_line("- [ ] todo", LineKind::TaskItem(false), &styles, None);

        assert!(result.contains("[ ]"), "unchecked glyph: {result:?}");
        assert!(!result.contains("- "), "no dash prefix: {result:?}");
        assert!(result.contains("todo"));
    }

    #[test]
    fn render_task_checked_uses_check_glyph() {
        let styles = test_styles();

        let result = render_markdown_line("- [x] done", LineKind::TaskItem(true), &styles, None);

        assert!(result.contains("[✓]"), "checked glyph: {result:?}");
        assert!(!result.contains("[x]"), "raw x removed: {result:?}");
        assert!(result.contains("done"));
    }

    #[test]
    fn render_hrule_emits_box_drawing() {
        let styles = test_styles();

        let result = render_markdown_line("---", LineKind::HorizontalRule, &styles, None);

        assert!(result.contains("────"), "box drawing chars: {result:?}");
        assert!(!result.contains("---"), "raw dashes removed: {result:?}");
    }

    #[test]
    fn render_paragraph_delegates_to_inline() {
        let styles = test_styles();

        let result = render_markdown_line("hello **world**", LineKind::Paragraph, &styles, None);

        assert!(!result.contains("**"), "bold markers stripped: {result:?}");
        assert!(result.contains("world"));
        assert!(
            result.contains("\x1b[1m"),
            "bold applied via inline: {result:?}"
        );
    }

    #[test]
    fn render_bullet_runs_inline_on_content() {
        let styles = test_styles();

        let result = render_markdown_line("- see `code`", LineKind::BulletItem, &styles, None);

        assert!(result.contains("•"));
        assert!(!result.contains('`'), "backticks stripped: {result:?}");
        assert!(result.contains("see "));
        assert!(result.contains("code"));
    }

    #[test]
    fn render_blockquote_runs_inline_on_content() {
        let styles = test_styles();

        let result = render_markdown_line(
            "> visit [here](https://example.com)",
            LineKind::Blockquote,
            &styles,
            None,
        );

        assert!(result.contains("│ "));
        assert!(result.contains("here"));
        assert!(result.contains("https://example.com"));
        assert!(result.contains("\x1b]8;;https://example.com\x1b\\"));
    }

    #[test]
    fn heading_can_contain_bold_inline() {
        let styles = test_styles();

        let result =
            render_markdown_line("## Announce **now**", LineKind::Heading(2), &styles, None);

        assert!(!result.contains("**"), "bold markers stripped: {result:?}");
        assert!(result.contains("now"));
        assert!(result.contains("Announce"));
        assert!(result.contains("\x1b[1m"), "bold present: {result:?}");
    }

    #[test]
    fn streaming_partial_bold_stays_raw() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();

        let partial = render.render_line("**bo");

        assert!(
            partial.contains("**bo"),
            "unclosed bold preserved: {partial:?}"
        );
    }

    #[test]
    fn streaming_partial_link_stays_raw() {
        let options = RenderOptions::default();
        let render = MarkdownRender::init(options).unwrap();

        let partial = render.render_line("[label](https://exa");

        assert!(
            partial.contains("[label]"),
            "unclosed link preserved: {partial:?}"
        );
    }

    #[test]
    fn rich_render_strips_syntax_without_theme() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();

        let output = render.render("# Heading\n\n> quoted\n\n- item\n");

        assert!(!output.contains("# Heading"), "hash removed: {output:?}");
        assert!(output.contains("Heading"));
        assert!(!output.contains("> quoted"), "gt removed");
        assert!(output.contains("│ "), "pipe prefix present");
        assert!(!output.contains("- item"), "dash removed");
        assert!(output.contains("•"), "bullet glyph present");
        assert!(output.contains("item"));
    }

    #[test]
    fn rich_and_raw_paths_diverge() {
        let raw_opts = RenderOptions {
            raw_markdown: true,
            ..Default::default()
        };
        let rich_opts = RenderOptions::default();
        let mut raw_render = MarkdownRender::init(raw_opts).unwrap();
        let mut rich_render = MarkdownRender::init(rich_opts).unwrap();
        let text = "# Heading\n\n**bold** text\n";

        let raw = raw_render.render(text);
        let rich = rich_render.render(text);

        assert_eq!(raw, text, "raw path preserves input");
        assert_ne!(rich, text, "rich path transforms input");
    }

    #[test]
    fn code_block_content_still_routes_to_syntect() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let text = "```rust\nfn main() {}\n```\n";

        let output = render.render(text);

        assert!(
            output.contains("fn main()"),
            "code content preserved: {output:?}"
        );
    }

    #[test]
    fn markdown_styles_fall_back_to_root_scopes() {
        let theme = minimal_root_scope_theme();

        let styles = MarkdownStyles::from_theme(Some(&theme), true);

        assert_eq!(styles.heading.0, rgb(0xde, 0xad, 0xbe));
        assert!(styles.heading.1);
        assert_eq!(styles.bold, rgb(0x44, 0x55, 0x66));
        assert_eq!(styles.italic, rgb(0x88, 0x88, 0x88));
        assert_eq!(styles.inline_code_fg, rgb(0xff, 0xdd, 0x00));
        assert_eq!(styles.inline_code_bg, None);
        assert_eq!(styles.blockquote, rgb(0x88, 0x88, 0x88));
        assert_eq!(styles.list_bullet, rgb(0xaa, 0x00, 0xff));
        assert_eq!(styles.link_text, rgb(0x11, 0x22, 0x33));
        assert_eq!(styles.link_url, rgb(0x00, 0xcc, 0xff));
        assert_eq!(styles.strikethrough, rgb(0xff, 0x00, 0x00));
        assert_eq!(styles.hrule, rgb(0x88, 0x88, 0x88));
        assert_eq!(styles.table_border, rgb(0x77, 0x77, 0x77));
    }
}
