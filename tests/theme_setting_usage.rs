//! Real-process checks of the `theme` setting as the README, Getting-Started
//! and the config template describe it: `light` or `dark` in the config
//! picks the built-in theme, and `COYOTE_THEME` is used only while the config
//! leaves `theme` null, so a config value wins over the variable. Each case
//! runs `coyote --info` on a pseudo-terminal through the shared harness
//! (highlighting, and with it the theme, is off when stdout is not a
//! terminal) with the config and cache directories under a scratch
//! directory. Unix-only; the cases run one at a time for the same
//! fork/openpty reason as the other pty suites.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::tui_harness::{TempDirGuard, fresh_config_dir, spawn_tui_with_args, write_config};
use serial_test::serial;

/// Runs `coyote --info` on a pty with `extra_config` appended to the dry-run
/// config and `envs` added, and returns the value of its `theme` row.
fn info_theme(extra_config: &str, envs: &[(&str, &str)]) -> String {
    let dir = fresh_config_dir("theme-setting");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, extra_config);
    let mut env = vec![("COYOTE_UI", "")];
    env.extend_from_slice(envs);
    let mut pty = spawn_tui_with_args(&dir, &env, &["--info"]);
    let text = pty
        .wait_for("tty-probe-done", Duration::from_secs(30))
        .unwrap();
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
