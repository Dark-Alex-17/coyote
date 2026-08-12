mod abort_signal;
mod clipboard;
mod command;
mod crypto;
mod html_to_md;
mod input;
mod loader;
mod logs;
pub mod native;
mod path;
mod render_prompt;
mod request;
mod spinner;
mod variables;

pub use self::abort_signal::*;
pub use self::clipboard::set_text;
pub use self::command::*;
pub use self::crypto::*;
pub use self::html_to_md::*;
pub use self::input::*;
pub use self::loader::*;
pub use self::logs::*;
pub use self::path::*;
pub use self::render_prompt::render_prompt;
pub use self::request::*;
pub use self::spinner::*;
pub use self::variables::*;

use anyhow::{Context, Result};
use fancy_regex::Regex;
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};
use is_terminal::IsTerminal;
use nu_ansi_term::Color;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::{cmp, env, path::PathBuf, process};
use syntect::highlighting::{Highlighter, Theme};
use syntect::parsing::Scope;

pub static CODE_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?ms)```\w*(.*)```").unwrap());
pub static THINK_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^\s*<think>.*?</think>(\s*|$)").unwrap());
pub static IS_STDOUT_TERMINAL: LazyLock<bool> = LazyLock::new(|| std::io::stdout().is_terminal());
pub static HEADLESS: AtomicBool = AtomicBool::new(false);
pub static ACP_SERVER: AtomicBool = AtomicBool::new(false);

static ACP_PERMISSION_QUEUE: Mutex<VecDeque<Value>> = Mutex::new(VecDeque::new());

pub fn queue_acp_permission(notification: Value) {
    if let Ok(mut q) = ACP_PERMISSION_QUEUE.lock() {
        q.push_back(notification);
    }
}

pub fn drain_acp_permissions() -> Vec<Value> {
    ACP_PERMISSION_QUEUE
        .lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default()
}
pub static NO_COLOR: LazyLock<bool> = LazyLock::new(|| {
    env::var("NO_COLOR")
        .ok()
        .and_then(|v| parse_bool(&v))
        .unwrap_or_default()
        || !*IS_STDOUT_TERMINAL
});

static TOOL_DIM_COLOR: OnceLock<Color> = OnceLock::new();
static TOOL_FN_COLOR: OnceLock<Color> = OnceLock::new();
static TOOL_KEY_COLOR: OnceLock<Color> = OnceLock::new();
static TOOL_WARN_COLOR: OnceLock<Color> = OnceLock::new();
static REPLAY_LABEL_COLOR: OnceLock<Color> = OnceLock::new();

pub fn init_tool_colors(theme: &Theme) {
    fn resolve(theme: &Theme, scope_str: &str) -> Option<Color> {
        let scope = Scope::new(scope_str).ok()?;
        let style = Highlighter::new(theme).style_mod_for_stack(&[scope]);
        let fg = style.foreground.or(theme.settings.foreground)?;
        let mute = |ch: u8| -> u8 { ((ch as u16 + 128) / 2) as u8 };
        Some(Color::Rgb(mute(fg.r), mute(fg.g), mute(fg.b)))
    }
    if let Some(c) = resolve(theme, "comment") {
        let _ = TOOL_DIM_COLOR.set(c);
    }
    if let Some(c) = resolve(theme, "support.function") {
        let _ = TOOL_FN_COLOR.set(c);
    }
    if let Some(c) = resolve(theme, "constant.numeric") {
        let _ = TOOL_KEY_COLOR.set(c);
    }
    if let Some(c) = resolve(theme, "string") {
        let _ = TOOL_WARN_COLOR.set(c);
    }
    let replay_color = Scope::new("entity.name")
        .ok()
        .and_then(|scope| {
            let style = Highlighter::new(theme).style_mod_for_stack(&[scope]);
            style.foreground.or(theme.settings.foreground)
        })
        .map(|fg| Color::Rgb(fg.r, fg.g, fg.b));
    if let Some(c) = replay_color {
        let _ = REPLAY_LABEL_COLOR.set(c);
    }
}

pub fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub fn now_timestamp() -> i64 {
    chrono::Local::now().timestamp()
}

pub fn get_env_name(key: &str) -> String {
    format!("{}_{key}", env!("CARGO_CRATE_NAME"),).to_ascii_uppercase()
}

pub fn normalize_env_name(value: &str) -> String {
    value.replace('-', "_").to_ascii_uppercase()
}

pub fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

pub fn estimate_token_length(text: &str) -> usize {
    let weighted: usize = text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
    weighted.div_ceil(4)
}

pub fn strip_think_tag(text: &str) -> Cow<'_, str> {
    THINK_TAG_RE.replace_all(text, "")
}

pub fn extract_code_block(text: &str) -> &str {
    CODE_BLOCK_RE
        .captures(text)
        .ok()
        .and_then(|v| v?.get(1).map(|v| v.as_str().trim()))
        .unwrap_or(text)
}

pub fn convert_option_string(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn fuzzy_filter<T, F>(values: Vec<T>, get: F, pattern: &str) -> Vec<T>
where
    F: Fn(&T) -> &str,
{
    let matcher = SkimMatcherV2::default();
    let mut list: Vec<(T, i64)> = values
        .into_iter()
        .filter_map(|v| {
            let score = matcher.fuzzy_match(get(&v), pattern)?;
            Some((v, score))
        })
        .collect();
    list.sort_unstable_by_key(|b| cmp::Reverse(b.1));
    list.into_iter().map(|(v, _)| v).collect()
}

pub fn pretty_error(err: &anyhow::Error) -> String {
    let mut output = vec![];
    output.push(format!("Error: {err}"));
    let causes: Vec<_> = err.chain().skip(1).collect();
    let causes_len = causes.len();
    if causes_len > 0 {
        output.push("\nCaused by:".to_string());
        if causes_len == 1 {
            output.push(format!("    {}", indent_text(causes[0], 4).trim()));
        } else {
            for (i, cause) in causes.into_iter().enumerate() {
                output.push(format!("{i:5}: {}", indent_text(cause, 7).trim()));
            }
        }
    }
    output.join("\n")
}

pub fn indent_text<T: ToString>(s: T, size: usize) -> String {
    let indent_str = " ".repeat(size);
    s.to_string()
        .split('\n')
        .map(|line| format!("{indent_str}{line}"))
        .collect::<Vec<String>>()
        .join("\n")
}

pub fn error_text(input: &str) -> String {
    color_text(input, Color::Red)
}

pub fn warning_text(input: &str) -> String {
    color_text(input, Color::Yellow)
}

pub fn muted_warning_text(input: &str) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    let color = TOOL_WARN_COLOR.get().copied().unwrap_or(Color::Fixed(136));
    color.paint(input).to_string()
}

pub fn color_text(input: &str, color: Color) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    nu_ansi_term::Style::new()
        .fg(color)
        .paint(input)
        .to_string()
}

pub fn dimmed_text(input: &str) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    let color = TOOL_DIM_COLOR.get().copied().unwrap_or(Color::Fixed(243));
    color.paint(input).to_string()
}

pub fn cyan_bold_text(input: &str) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    let color = TOOL_FN_COLOR.get().copied().unwrap_or(Color::Fixed(73));
    nu_ansi_term::Style::new()
        .fg(color)
        .bold()
        .paint(input)
        .to_string()
}

pub fn replay_label_text(input: &str) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    let color = REPLAY_LABEL_COLOR.get().copied().unwrap_or(Color::Green);
    nu_ansi_term::Style::new()
        .fg(color)
        .bold()
        .paint(input)
        .to_string()
}

pub fn magenta_text(input: &str) -> String {
    if *NO_COLOR {
        return input.to_string();
    }
    let color = TOOL_KEY_COLOR.get().copied().unwrap_or(Color::Fixed(133));
    color.paint(input).to_string()
}

pub fn multiline_text(input: &str) -> String {
    input
        .split('\n')
        .enumerate()
        .map(|(i, v)| {
            if i == 0 {
                v.to_string()
            } else {
                format!(".. {v}")
            }
        })
        .collect::<Vec<String>>()
        .join("\n")
}

pub fn temp_file(prefix: &str, suffix: &str) -> PathBuf {
    env::temp_dir().join(format!(
        "{}-{}{prefix}{}{suffix}",
        env!("CARGO_CRATE_NAME").to_lowercase(),
        process::id(),
        uuid::Uuid::new_v4()
    ))
}

pub fn is_url(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

/// 127.0.0.1 means something different to a proxy than it does to us, so a tool
/// that intercepts proxied traffic answers for a local service that is running
/// fine, and the failure reads as a fault in Coyote or in that service.
const LOCAL_NO_PROXY: &str =
    "localhost,127.0.0.0/8,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,.local";

/// Applies proxy settings, keeping local traffic direct. `configured` is a
/// client's own `extra.proxy`, where `"-"` means no proxy at all; with nothing
/// configured an ambient `*_PROXY` is still honoured for public hosts.
pub fn apply_proxy(
    mut builder: reqwest::ClientBuilder,
    configured: Option<&str>,
) -> Result<reqwest::ClientBuilder> {
    // reqwest offers no way to add rules to the proxies it auto-detects, so
    // detection is disabled and redone below.
    builder = builder.no_proxy();

    let configured = configured.map(str::trim).filter(|p| !p.is_empty());
    if configured == Some("-") {
        return Ok(builder);
    }
    let exempt = no_proxy_rules();
    if let Some(url) = configured {
        let proxy = reqwest::Proxy::all(url)
            .with_context(|| format!("Invalid proxy `{url}`"))?
            .no_proxy(reqwest::NoProxy::from_string(&exempt));
        return Ok(builder.proxy(proxy));
    }

    // Split per scheme, because HTTP_PROXY and HTTPS_PROXY are allowed to differ.
    for (is_https, keys) in [
        (
            true,
            ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"],
        ),
        (
            false,
            ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"],
        ),
    ] {
        let Some(url) = first_env(&keys) else {
            continue;
        };
        let proxy = if is_https {
            reqwest::Proxy::https(&url)
        } else {
            reqwest::Proxy::http(&url)
        };
        let proxy = proxy
            .with_context(|| format!("Invalid proxy `{url}`"))?
            .no_proxy(reqwest::NoProxy::from_string(&exempt));
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Replacing reqwest's auto-detection loses its `NO_PROXY` handling, so that is
/// merged back in here.
fn no_proxy_rules() -> String {
    let mut rules = LOCAL_NO_PROXY.to_string();
    let extra = env::var("NO_PROXY")
        .or_else(|_| env::var("no_proxy"))
        .unwrap_or_default();
    if !extra.trim().is_empty() {
        rules.push(',');
        rules.push_str(extra.trim());
    }
    rules
}

pub fn decode_bin<T: serde::de::DeserializeOwned>(data: &[u8]) -> Result<T> {
    let (v, _) = bincode::serde::decode_from_slice(data, bincode::config::legacy())?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invalid rule syntax makes `from_string` return `None`, which silently drops
    /// every exemption and sends local traffic back through the proxy.
    #[test]
    fn the_local_no_proxy_rules_are_valid() {
        assert!(
            reqwest::NoProxy::from_string(LOCAL_NO_PROXY).is_some(),
            "reqwest rejected LOCAL_NO_PROXY, so nothing would be exempt"
        );
    }

    #[test]
    fn local_rules_cover_loopback_and_private_ranges() {
        for host in [
            "localhost",
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
        ] {
            assert!(
                LOCAL_NO_PROXY.contains(host),
                "{host} must stay exempt from proxying"
            );
        }
    }

    #[test]
    fn a_dash_means_no_proxy_at_all() {
        assert!(apply_proxy(reqwest::ClientBuilder::new(), Some("-")).is_ok());
        assert!(apply_proxy(reqwest::ClientBuilder::new(), Some(" - ")).is_ok());
    }

    #[test]
    fn an_unparseable_proxy_is_reported() {
        let err = apply_proxy(reqwest::ClientBuilder::new(), Some("not a url"))
            .unwrap_err()
            .to_string();

        assert!(err.contains("Invalid proxy"), "got: {err}");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn test_safe_join_path() {
        assert_eq!(
            safe_join_path("/home/user/dir1", "files/file1"),
            Some(PathBuf::from("/home/user/dir1/files/file1"))
        );
        assert!(safe_join_path("/home/user/dir1", "/files/file1").is_none());
        assert!(safe_join_path("/home/user/dir1", "../file1").is_none());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_safe_join_path() {
        assert_eq!(
            safe_join_path("C:\\Users\\user\\dir1", "files/file1"),
            Some(PathBuf::from("C:\\Users\\user\\dir1\\files\\file1"))
        );
        assert!(safe_join_path("C:\\Users\\user\\dir1", "/files/file1").is_none());
        assert!(safe_join_path("C:\\Users\\user\\dir1", "../file1").is_none());
    }
}
