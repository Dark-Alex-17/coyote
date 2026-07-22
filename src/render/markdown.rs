use crate::utils::decode_bin;

use ansi_colours::AsRGB;
use anyhow::{Context, Result, anyhow};
use crossterm::style::{Color, Stylize};
use crossterm::terminal;
use fancy_regex::Regex;
use std::collections::HashMap;
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

static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(#{1,6}) +.+").unwrap());
static BLOCKQUOTE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*>").unwrap());
static TASK_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*+] \[([ xX])\] +.+").unwrap());
static BULLET_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*+] +.+").unwrap());
static NUMBERED_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\d+\. +.+").unwrap());
static HRULE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(-{3,}|_{3,}|\*{3,})\s*$").unwrap());

static INLINE_CODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`([^`\n]+)`").unwrap());
static IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)").unwrap());
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
static BOLD_AST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\*\*([^*\n]+)\*\*").unwrap());
static BOLD_US_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"__([^_\n]+)__").unwrap());
static ITALIC_AST_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?<![*\w])\*(?!\s)([^*\n]+?)(?<!\s)\*(?!\*)").unwrap()
});
static ITALIC_US_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?<![_\w])_(?!\s)([^_\n]+?)(?<!\s)_(?!_)").unwrap()
});
static STRIKETHROUGH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"~~([^~\n]+)~~").unwrap());
static CODE_PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x00C(\d+)\x00").unwrap());

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Heading(u8),
    Blockquote,
    TaskItem(bool),
    BulletItem,
    NumberedItem,
    HorizontalRule,
    Paragraph,
}

#[allow(dead_code)]
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
    LineKind::Paragraph
}

fn regex_replace<F>(text: &str, re: &Regex, mut f: F) -> String
where
    F: FnMut(&fancy_regex::Captures) -> String,
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

#[allow(dead_code)]
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

pub struct MarkdownRender {
    options: RenderOptions,
    syntax_set: SyntaxSet,
    code_color: Option<Color>,
    md_syntax: SyntaxReference,
    code_syntax: Option<SyntaxReference>,
    prev_line_type: LineType,
    wrap_width: Option<u16>,
    #[allow(dead_code)]
    styles: MarkdownStyles,
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
            options,
        })
    }

    pub fn render(&mut self, text: &str) -> String {
        text.split('\n')
            .map(|line| self.render_line_mut(line))
            .collect::<Vec<String>>()
            .join("\n")
    }

    pub fn render_line(&self, line: &str) -> String {
        let (_, _, code_syntax, is_code) = self.check_line(line);
        if is_code {
            self.highlight_code_line(line, &code_syntax)
        } else {
            self.highlight_line(line, &self.md_syntax, false)
        }
    }

    fn render_line_mut(&mut self, line: &str) -> String {
        let (line_type, _, code_syntax, is_code) = self.check_line(line);
        let output = if is_code {
            self.highlight_code_line(line, &code_syntax)
        } else {
            self.highlight_line(line, &self.md_syntax, false)
        };
        self.prev_line_type = line_type;
        self.code_syntax = code_syntax;
        output
    }

    fn check_line(
        &self,
        line: &str,
    ) -> (LineType, LineKind, Option<SyntaxReference>, bool) {
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
    #[allow(dead_code)]
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
    for scope_name in std::iter::once(primary).chain(fallbacks.iter().copied()) {
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

#[allow(dead_code)]
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
        let strikethrough =
            resolve_scope_style(theme, "markup.deleted", &["invalid"], truecolor);
        let hrule = resolve_scope_style(theme, "comment", &["punctuation"], truecolor);

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
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        let output = render.render(TEXT);
        assert_eq!(TEXT, output);
    }

    #[test]
    fn no_wrap_code() {
        let options = RenderOptions::default();
        let mut render = MarkdownRender::init(options).unwrap();
        render.wrap_width = Some(80);
        let output = render.render(TEXT);
        assert_eq!(TEXT_NO_WRAP_CODE, output);
    }

    #[test]
    fn wrap_all() {
        let options = RenderOptions {
            wrap_code: true,
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
        theme.scopes.push(theme_item(
            "string",
            syntect_rgb(0xff, 0xdd, 0x00),
            None,
        ));
        theme.scopes.push(theme_item(
            "comment",
            syntect_rgb(0x88, 0x88, 0x88),
            None,
        ));
        theme.scopes.push(theme_item(
            "keyword",
            syntect_rgb(0xaa, 0x00, 0xff),
            None,
        ));
        theme.scopes.push(theme_item(
            "constant",
            syntect_rgb(0x00, 0xcc, 0xff),
            None,
        ));
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
        theme.scopes.push(theme_item(
            "invalid",
            syntect_rgb(0xff, 0x00, 0x00),
            None,
        ));
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
        theme.scopes.push(theme_item(
            "comment",
            syntect_rgb(0x33, 0x44, 0x55),
            None,
        ));
        let resolved =
            resolve_scope_style(&theme, "markup.italic", &["nope", "comment"], true);
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
        assert!(result.contains("\x1b[1m"), "bold applied around code: {result:?}");
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
        assert!(!result.contains("!["), "raw image marker removed: {result:?}");
        assert!(result.contains("Image: alt text"));
        assert!(result.contains("https://img.example/x.png"));
        assert!(result.contains("\x1b]8;;https://img.example/x.png\x1b\\"));
    }

    #[test]
    fn image_processed_before_link() {
        let styles = test_styles();
        let result = apply_inline("![alt](https://example.com)", &styles);
        assert!(!result.starts_with('!'), "no stray ! left behind: {result:?}");
        assert!(result.contains("Image:"));
    }

    #[test]
    fn plain_text_unchanged() {
        let styles = test_styles();
        assert_eq!(apply_inline("just plain text", &styles), "just plain text");
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
    }
}
