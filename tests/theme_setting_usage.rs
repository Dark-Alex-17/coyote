//! Real-process checks of the `theme` setting as the README, Getting-Started
//! and the config template describe it: `light` or `dark` in the config
//! picks the built-in theme, `COYOTE_THEME` is used only while the config
//! leaves `theme` null, so a config value wins over the variable, and a null
//! `theme` follows the terminal's background, dark when the terminal does
//! not answer. Each case runs the binary on a pseudo-terminal through the
//! shared harness (highlighting, and with it the theme, is off when stdout
//! is not a terminal) with the config and cache directories under a scratch
//! directory and checks that it exits 0. Unix-only; the cases run one at a
//! time for the same fork/openpty reason as the other pty suites.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::tui_harness::{
    TempDirGuard, exit_code, fresh_config_dir, spawn_with_args, write_config,
};
use serial_test::serial;

/// Runs `coyote --info` on a pty with `extra_config` appended to the dry-run
/// config and `envs` added, and returns the value of its `theme` row.
fn info_theme(extra_config: &str, envs: &[(&str, &str)]) -> String {
    let dir = fresh_config_dir("theme-setting");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, extra_config);
    let mut pty = spawn_with_args(&dir, envs, &["--info"]);
    let text = pty
        .wait_for("tty-probe-done", Duration::from_secs(30))
        .unwrap();
    assert_eq!(exit_code(&text), 0, "--info exits 0:\n{text}");
    text.lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next() == Some("theme")).then(|| parts.next().unwrap_or("").to_string())
        })
        .unwrap_or_else(|| panic!("no `theme` row in --info:\n{text}"))
}

#[test]
#[serial]
fn config_theme_light_or_dark_picks_one() {
    assert_eq!(info_theme("theme: light\n", &[]), "light");
    assert_eq!(info_theme("theme: dark\n", &[]), "dark");
}

#[test]
#[serial]
fn coyote_theme_sets_the_theme_when_the_config_leaves_it_null() {
    assert_eq!(info_theme("", &[("COYOTE_THEME", "light")]), "light");
    assert_eq!(info_theme("", &[("COYOTE_THEME", "dark")]), "dark");
}

#[test]
#[serial]
fn a_config_theme_wins_over_coyote_theme() {
    assert_eq!(
        info_theme("theme: light\n", &[("COYOTE_THEME", "dark")]),
        "light",
        "COYOTE_THEME=dark with `theme: light` in the config"
    );
    assert_eq!(
        info_theme("theme: dark\n", &[("COYOTE_THEME", "light")]),
        "dark",
        "COYOTE_THEME=light with `theme: dark` in the config"
    );
}

const WHITE: &[u8] = b"rgb:ffff/ffff/ffff";
const BLACK: &[u8] = b"rgb:0000/0000/0000";

/// Runs a one-shot dry-run prompt holding a fenced code block on a pty
/// with `extra_config` and `envs`, answers the terminal color queries
/// with `background` (and the contrasting foreground) when it is given,
/// and returns the foreground SGR sequences the highlighted block was
/// painted with: the fingerprint of the theme in use.
fn code_block_colors(
    extra_config: &str,
    envs: &[(&str, &str)],
    background: Option<&[u8]>,
) -> Vec<String> {
    let dir = fresh_config_dir("theme-render");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, extra_config);
    let mut pty = spawn_with_args(
        &dir,
        envs,
        &["'```rust\nfn main() { let x = \"hi\"; }\n```'"],
    );
    if let Some(bg) = background {
        let fg = if bg == WHITE { BLACK } else { WHITE };
        if pty
            .wait_for_raw(b"\x1b]11;?", 0, Duration::from_secs(10))
            .is_ok()
        {
            let mut reply = Vec::new();
            for (code, color) in [(b"10", fg), (b"11", bg)] {
                reply.extend_from_slice(b"\x1b]");
                reply.extend_from_slice(code);
                reply.push(b';');
                reply.extend_from_slice(color);
                reply.extend_from_slice(b"\x1b\\");
            }
            reply.extend_from_slice(b"\x1b[?62;22c");
            pty.send(&reply).unwrap();
        }
    }
    let text = pty
        .wait_for("tty-probe-done", Duration::from_secs(30))
        .unwrap();
    assert_eq!(exit_code(&text), 0, "the prompt exits 0:\n{text}");
    let raw = String::from_utf8_lossy(pty.raw()).into_owned();
    let block = &raw[raw.rfind("```rust").expect("the code block was echoed")..];
    block
        .split("\x1b[")
        .filter_map(|seq| seq.split_once('m').map(|(sgr, _)| sgr))
        .filter(|sgr| sgr.starts_with("38;"))
        .map(str::to_string)
        .collect()
}

#[test]
#[serial]
fn a_null_theme_follows_the_terminal_background_and_falls_back_to_dark() {
    let light = code_block_colors("theme: light\n", &[], None);
    let dark = code_block_colors("theme: dark\n", &[], None);
    assert!(
        !light.is_empty() && light != dark,
        "light {light:?} vs dark {dark:?}"
    );
    assert_eq!(
        code_block_colors("", &[], Some(WHITE)),
        light,
        "white background"
    );
    assert_eq!(
        code_block_colors("", &[], Some(BLACK)),
        dark,
        "black background"
    );
    assert_eq!(code_block_colors("", &[], None), dark, "silent terminal");
    assert_eq!(
        code_block_colors("theme: dark\n", &[], Some(WHITE)),
        dark,
        "`theme: dark` on a white background"
    );
    assert_eq!(
        code_block_colors("", &[("COYOTE_THEME", "light")], Some(BLACK)),
        light,
        "COYOTE_THEME=light on a black background"
    );
}
