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
//!    own. The harness is the one audited place that pins the config and
//!    cache dirs, detaches from the runner's tty and selects the UI, so a
//!    spawn written around it is a spawn nobody checked. One gap stays
//!    open on purpose: `built_binary()` is public because the suites hand
//!    it to the harness's own helpers (`spawn_binary_under`,
//!    `ttyless_command`, `run_ttyless_binary`), so a suite could still
//!    pass it as an argument to a literal program
//!    (`Command::new("bash").arg(built_binary())`). That is a spawn for
//!    review to catch; this lint does not look for it.
//! 2. Every other file (a self-contained harness that spawns the binary
//!    itself, and the shared harness) pins both dirs at every spawn: per
//!    file, the spawn sites, the `COYOTE_CONFIG_DIR` sites and the
//!    `COYOTE_CACHE_DIR` sites count the same, so nothing a test run
//!    spawns lands in the developer's real config or cache.
//!
//! The list is also the census: the set of files under `tests/` whose
//! code names the shared harness must equal it, so a new importer has to
//! be listed (and so put under the first rule) and a listed file that
//! stops importing is noticed. A file names the harness when the module
//! name appears outside comments and `mod` declarations, whichever path
//! it is reached by (`use common::{pty, tui_harness::*}`,
//! `crate::common::tui_harness`, `super::tui_harness`).

use std::fs;
use std::path::{Path, PathBuf};

// Every needle is spelled in halves so this file's own source matches
// none of them when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const CONFIG_ENV: &str = ".env(\"COYOTE_CONFIG_DIR\"";
const CACHE_ENV: &str = ".env(\"COYOTE_CACHE_DIR\"";
const HARNESS: &str = concat!("tui_", "harness");
const COMMAND_NEW: &str = concat!("Command::", "new(");
const BARE_SPAWNS: [&str; 2] = [
    concat!("env!(\"CARGO_BIN_", "EXE_"),
    concat!("Pty::", "spawn("),
];
const HARNESS_USERS: [&str; 3] = [
    "tui_binary.rs",
    "tui_binary_menus.rs",
    "tui_binary_wizards.rs",
];
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

/// The source without its comment lines and `mod` declarations: a doc
/// comment pointing at the harness, or `common/mod.rs` declaring it, names
/// the module without using it.
fn code_only(source: &str) -> String {
    source
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            let declares_module =
                (line.starts_with("mod ") || line.starts_with("pub mod ")) && line.ends_with(';');
            !(line.starts_with("//") || declares_module)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn uses_harness(source: &str) -> bool {
    code_only(source).contains(HARNESS)
}

/// Spawn sites, `COYOTE_CONFIG_DIR` sites and `COYOTE_CACHE_DIR` sites.
fn site_counts(source: &str) -> [usize; 3] {
    [SPAWN_SITE, CONFIG_ENV, CACHE_ENV].map(|needle| source.matches(needle).count())
}

/// Everything `name` gets wrong under the rule its source puts it under,
/// plus its standing in the census.
fn violations(name: &str, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let listed = HARNESS_USERS.contains(&name);
    if uses_harness(source) {
        if !listed {
            out.push(
                "imports the shared harness but is not listed in HARNESS_USERS; list it so it \
                 spawns only through the harness"
                    .to_string(),
            );
        }
        for (at, _) in source.match_indices(COMMAND_NEW) {
            let argument = source[at + COMMAND_NEW.len()..].trim_start();
            if !argument.starts_with('"') {
                out.push(format!(
                    "`{COMMAND_NEW}{}` runs a program that is not a string literal; the built \
                     binary is spawned only through the shared harness (`spawn_tui*`, \
                     `spawn_binary_under`, `ttyless_command`, `run_ttyless*`)",
                    argument.split(')').next().unwrap_or_default()
                ));
            }
        }
        for needle in BARE_SPAWNS {
            if source.contains(needle) {
                out.push(format!(
                    "spawns with `{needle}` around the shared harness; use its spawn functions \
                     (`spawn_tui*`, `spawn_binary_under`, `ttyless_command`, `run_ttyless*`) \
                     instead"
                ));
            }
        }
        return out;
    }
    if listed {
        out.push("is listed in HARNESS_USERS but does not import the shared harness".to_string());
    }
    let [spawn_sites, config_sites, cache_sites] = site_counts(source);
    if spawn_sites == 0 && config_sites == 0 && cache_sites == 0 {
        return out;
    }
    if spawn_sites != config_sites || config_sites != cache_sites {
        out.push(format!(
            "{spawn_sites} binary spawn(s), {config_sites} COYOTE_CONFIG_DIR site(s), \
             {cache_sites} COYOTE_CACHE_DIR site(s); every spawn of the binary must pin both dirs"
        ));
    }
    out
}

#[test]
fn harness_users_spawn_only_through_the_harness_and_every_other_spawn_pins_both_dirs() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut found = Vec::new();
    let mut self_contained = Vec::new();
    for path in rust_sources(&tests_dir) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = read(&path);
        found.extend(
            violations(&name, &source)
                .into_iter()
                .map(|violation| format!("{}: {violation}", path.display())),
        );
        if !uses_harness(&source) && site_counts(&source).iter().any(|&n| n > 0) {
            self_contained.push(name);
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
    assert!(
        self_contained.len() >= SELF_CONTAINED_FLOOR,
        "expected the known self-contained binary harnesses to be scanned, found {self_contained:?}"
    );
}

#[test]
fn a_harness_user_spawning_a_non_literal_program_is_named_once() {
    let source = format!("use common::{HARNESS}::*;\n{COMMAND_NEW}&binary).arg(\"--help\");\n");
    let found = violations(HARNESS_USERS[0], &source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains(&format!("`{COMMAND_NEW}&binary`")),
        "{found:?}"
    );
}

#[test]
fn a_harness_user_opening_its_own_pty_is_flagged() {
    let source = format!(
        "use common::{{pty, {HARNESS}::*}};\nlet pty = {}\"bash\");\n",
        BARE_SPAWNS[1]
    );
    let found = violations(HARNESS_USERS[1], &source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains(BARE_SPAWNS[1]), "{found:?}");
}

#[test]
fn a_self_contained_suite_pinning_fewer_dirs_than_it_spawns_is_flagged() {
    let source = format!(
        "let a = env!(\"{SPAWN_SITE}\");\nlet b = env!(\"{SPAWN_SITE}\");\ncmd{CONFIG_ENV}, dir);\n"
    );
    let found = violations("self_contained.rs", &source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].contains("2 binary spawn(s), 1 COYOTE_CONFIG_DIR site(s), 0 COYOTE_CACHE_DIR"),
        "{found:?}"
    );
}

#[test]
fn a_harness_user_spawning_only_literal_programs_through_the_harness_is_clean() {
    let source = format!(
        "use common::{{pty, {HARNESS}::*}};\n\
         let out = {COMMAND_NEW}\"git\").arg(\"status\").output();\n\
         let tui = spawn_tui(&dir);\n"
    );
    let found = violations(HARNESS_USERS[2], &source);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_file_naming_the_harness_only_in_a_comment_is_not_a_user() {
    let source = format!(
        "//! Mirrors the bash wrapper `tests/common/{HARNESS}.rs` holds.\n\
         // crate::common::{HARNESS} is not imported here.\n\
         let pty = {}\"bash\");\n",
        BARE_SPAWNS[1]
    );
    assert!(!uses_harness(&source));
    let found = violations("lone.rs", &source);
    assert!(found.is_empty(), "{found:?}");
}
