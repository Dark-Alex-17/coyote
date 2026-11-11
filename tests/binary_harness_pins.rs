//! Pins how the integration suites spawn the real binary, read from the
//! harness sources themselves (the suites under `tests/` and the shared
//! modules under `tests/common/`). Two rules:
//!
//! 1. The suites listed in `HARNESS_USERS` (the ones importing the shared
//!    pty harness, `common::tui_harness`) spawn the binary and open ptys
//!    only through `tests/common/`: every `Command::new(` they write names
//!    its program as a string literal (`"git"`, `"bash"`), so no path to
//!    the built binary can be smuggled in under another name; and they
//!    carry no `env!("CARGO_BIN_EXE_...")` and no `Pty::spawn(` of their
//!    own. The harness is the one audited place
//!    that pins the config and cache dirs, detaches from the runner's tty
//!    and selects the UI, so a spawn written around it is a spawn nobody
//!    checked.
//! 2. Every other file (a self-contained harness that spawns the binary
//!    itself, and the shared harness) pins both dirs at every spawn: per
//!    file, the spawn sites, the `COYOTE_CONFIG_DIR` sites and the
//!    `COYOTE_CACHE_DIR` sites count the same, so nothing a test run
//!    spawns lands in the developer's real config or cache.
//!
//! The list is also the census: the set of files under `tests/` whose
//! source imports the shared harness must equal it, so a new importer has
//! to be listed (and so put under the first rule) and a listed file that
//! stops importing is noticed.

use std::fs;
use std::path::{Path, PathBuf};

// Every needle is spelled in halves so this file's own source matches
// none of them when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const CONFIG_ENV: &str = ".env(\"COYOTE_CONFIG_DIR\"";
const CACHE_ENV: &str = ".env(\"COYOTE_CACHE_DIR\"";
const HARNESS_IMPORT: &str = concat!("use common::", "tui_harness");
const COMMAND_NEW: &str = concat!("Command::", "new(");
const BARE_SPAWNS: [&str; 2] = [
    concat!("env!(\"CARGO_BIN_", "EXE_"),
    concat!("Pty::", "spawn("),
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

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn harness_users_spawn_only_through_the_harness_and_every_other_spawn_pins_both_dirs() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for name in HARNESS_USERS {
        let path = tests_dir.join(name);
        let source = read(&path);
        for (at, _) in source.match_indices(COMMAND_NEW) {
            let argument = source[at + COMMAND_NEW.len()..].trim_start();
            assert!(
                argument.starts_with('"'),
                "{}: `{COMMAND_NEW}{}` runs a program that is not a string literal; the \
                 built binary is spawned only through the shared harness (`spawn_tui*`, \
                 `spawn_binary_under`, `ttyless_command`, `run_ttyless*`)",
                path.display(),
                argument.split(')').next().unwrap_or_default()
            );
        }
        for needle in BARE_SPAWNS {
            assert!(
                !source.contains(needle),
                "{}: spawns with `{needle}` around the shared harness; use its spawn \
                 functions (`spawn_tui*`, `spawn_binary_under`, `ttyless_command`, \
                 `run_ttyless*`) instead",
                path.display()
            );
        }
    }

    let mut importers = Vec::new();
    let mut self_contained = Vec::new();
    for path in rust_sources(&tests_dir) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = read(&path);
        if source.contains(HARNESS_IMPORT) {
            importers.push(name);
            continue;
        }
        if HARNESS_USERS.contains(&name.as_str()) {
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
    importers.sort();
    assert_eq!(
        importers, HARNESS_USERS,
        "the files importing the shared harness are not the ones listed in HARNESS_USERS; \
         a new importer must be listed (and so spawn only through the harness), and a \
         listed file must import it"
    );
    assert!(
        self_contained.len() >= SELF_CONTAINED_FLOOR,
        "expected the known self-contained binary harnesses to be scanned, found {self_contained:?}"
    );
}
