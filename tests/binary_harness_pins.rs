//! Pins how the integration suites spawn the real binary, read from the
//! harness sources themselves (the suites under `tests/` and the shared
//! modules under `tests/common/`). Two rules:
//!
//! 1. A suite that imports the shared pty harness (`common::tui_harness`)
//!    spawns the binary only through it: no `Command::new(built_binary())`,
//!    no `Command::new(binary)`, no `env!("CARGO_BIN_EXE_...")` of its own.
//!    The harness is the one audited place that pins the config and cache
//!    dirs, detaches from the runner's tty and selects the UI, so a spawn
//!    written around it is a spawn nobody checked.
//! 2. A self-contained harness (a suite that spawns the binary itself) and
//!    the shared harness pin both dirs at every spawn: per file, the spawn
//!    sites, the `COYOTE_CONFIG_DIR` sites and the `COYOTE_CACHE_DIR` sites
//!    count the same, so nothing a test run spawns lands in the developer's
//!    real config or cache.
//!
//! The files that import the shared harness are listed here as well, so a
//! suite that stops importing it (and so leaves the first rule) is noticed.

use std::fs;
use std::path::{Path, PathBuf};

// Every needle is spelled in halves so this file's own source matches
// none of them when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const CONFIG_ENV: &str = ".env(\"COYOTE_CONFIG_DIR\"";
const CACHE_ENV: &str = ".env(\"COYOTE_CACHE_DIR\"";
const HARNESS_IMPORT: &str = concat!("use common::", "tui_harness");
const BARE_SPAWNS: [&str; 3] = [
    concat!("Command::new(", "built_binary("),
    concat!("Command::new(", "binary"),
    concat!("env!(\"CARGO_BIN_", "EXE_"),
];
const HARNESS_USERS: [&str; 2] = ["tui_binary.rs", "tui_binary_wizards.rs"];
// Files that spawn the binary outside the shared harness's import, as of
// 2026-11-10: 29 self-contained suites plus the shared harness itself.
// Files with no spawn and no dir pin at all are skipped before they are
// counted, so this is the number of spawning files, not of files scanned.
// A floor rather than an exact count, so adding a suite does not require
// touching this pin.
const SELF_CONTAINED_FLOOR: usize = 30;

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).expect("tests dir is readable") {
        let path = entry.expect("tests dir entry").path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    out
}

#[test]
fn harness_users_spawn_only_through_the_harness_and_every_other_spawn_pins_both_dirs() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut harness_users = Vec::new();
    let mut self_contained = Vec::new();
    for path in rust_sources(&tests_dir) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if source.contains(HARNESS_IMPORT) {
            for needle in BARE_SPAWNS {
                assert!(
                    !source.contains(needle),
                    "{}: spawns the binary with `{needle}` around the shared harness; \
                     use its spawn functions (`spawn_tui*`, `spawn_binary_under`, \
                     `ttyless_command`, `run_ttyless*`) instead",
                    path.display()
                );
            }
            harness_users.push(name);
            continue;
        }
        let spawn_sites = source.matches(SPAWN_SITE).count();
        let config_sites = source.matches(CONFIG_ENV).count();
        let cache_sites = source.matches(CACHE_ENV).count();
        if spawn_sites == 0 && config_sites == 0 && cache_sites == 0 {
            continue;
        }
        assert!(
            spawn_sites == config_sites && config_sites == cache_sites,
            "{}: {spawn_sites} binary spawn(s), {config_sites} COYOTE_CONFIG_DIR site(s), \
             {cache_sites} COYOTE_CACHE_DIR site(s); every spawn of the binary must pin both dirs",
            path.display()
        );
        self_contained.push(name);
    }
    harness_users.sort();
    assert_eq!(
        harness_users, HARNESS_USERS,
        "the suites importing the shared harness changed; update the list if that is intended"
    );
    assert!(
        self_contained.len() >= SELF_CONTAINED_FLOOR,
        "expected the known self-contained binary harnesses to be scanned, found {self_contained:?}"
    );
}
