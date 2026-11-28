//! Pins the environment every integration harness hands the real binary:
//! the built `coyote` is named in one place, `tests/support/bin.rs`, so every
//! spawn goes through `support::bin::coyote_command` (or `isolate` for a
//! wrapper such as `script`). That pins the config, cache and home dirs to
//! the harness's own, pins the temp dir to whatever the harness passes (most
//! pass the inherited one, so it is stated explicitly rather than isolated),
//! and clears every inherited `COYOTE_*` variable, the unit-test
//! workspace-memory root override among them, so nothing a test run spawns
//! lands in the developer's real config or cache or picks up their overrides.
//!
//! Also pins the support helpers themselves: `support::repo::read_path` folds
//! CRLF to LF, and the crate that checks `coyote_command`'s env holds a single
//! test.

mod support;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

// Spelled in two halves so this file's own source does not count as a
// spawn site when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const SUPPORT_BIN: &str = "tests/support/bin.rs";
// Split for the same reason: a bare `Command` around the binary path skips
// the isolation coyote_command applies.
const BARE_SPAWNS: [&str; 3] = [
    concat!("Command::new(", "support::bin::path()"),
    concat!("Command::new(", "bin::path()"),
    concat!("Command::new(", "path())"),
];
const PATH_CALL: &str = concat!("bin::", "path()");
const GRAPH_SESSION_HARNESSES: [&str; 2] = [
    "tests/graph_session_block_binary.rs",
    "tests/graph_session_repl_binary.rs",
];
const SUPPORT_ISOLATION: &str = "tests/support_isolation.rs";
// Split so this file's own source does not count as a test attribute.
const TEST_ATTR: &str = concat!("#[", "test]");

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("tests dir entry").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Every `tests/**/*.rs` file as its crate-relative `/`-separated path and
/// its text, sorted by path.
fn test_sources() -> Vec<(String, String)> {
    let root = support::repo::root();
    let mut paths = Vec::new();
    rust_sources(&root.join("tests"), &mut paths);
    let mut sources: Vec<(String, String)> = paths
        .iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, support::repo::read_path(path))
        })
        .collect();
    sources.sort();
    sources
}

#[test]
fn every_spawn_goes_through_coyote_command_which_pins_the_dirs_and_clears_inherited_coyote_vars() {
    let sources = test_sources();
    let naming: Vec<&str> = sources
        .iter()
        .filter(|(_, text)| text.contains(SPAWN_SITE))
        .map(|(rel, _)| rel.as_str())
        .collect();
    assert_eq!(
        naming,
        [SUPPORT_BIN],
        "only {SUPPORT_BIN} may name the built binary; every spawn goes through \
         support::bin::coyote_command, which pins the config, cache and home dirs, \
         pins the temp dir to what the caller passes, and clears every inherited \
         COYOTE_* variable"
    );

    let bare: Vec<&str> = sources
        .iter()
        .filter(|(rel, text)| {
            rel != SUPPORT_BIN && BARE_SPAWNS.iter().any(|spawn| text.contains(spawn))
        })
        .map(|(rel, _)| rel.as_str())
        .collect();
    assert!(
        bare.is_empty(),
        "these files wrap the binary path in a bare Command, skipping the dir pins and \
         COYOTE_* clearing; spawn through support::bin::coyote_command instead: {bare:?}"
    );

    let unisolated: Vec<&str> = sources
        .iter()
        .filter(|(rel, text)| {
            rel != SUPPORT_BIN
                && text.contains(PATH_CALL)
                && !text.contains("coyote_command(")
                && !text.contains("isolate(")
        })
        .map(|(rel, _)| rel.as_str())
        .collect();
    assert!(
        unisolated.is_empty(),
        "these files use support::bin::path() without coyote_command or isolate, so \
         their spawns skip the dir pins and COYOTE_* clearing: {unisolated:?}"
    );
}

#[test]
fn every_graph_session_harness_spawn_pins_path() {
    let root = support::repo::root();
    for rel in GRAPH_SESSION_HARNESSES {
        let text = support::repo::read_path(&root.join(rel));
        let spawns = text.matches("coyote_command(").count() + text.matches("isolate(").count();
        let pins = text.matches(".env(\"PATH\", pinned_path(").count();
        assert!(
            spawns > 0 && spawns == pins,
            "{rel}: {spawns} coyote_command or isolate spawns but {pins} PATH pins; a \
             graph-session spawn must run with only the binary dir and system shells on PATH"
        );
    }
}

#[test]
fn support_isolation_crate_holds_a_single_test() {
    let text = support::repo::read(SUPPORT_ISOLATION);
    let tests = text.matches(TEST_ATTR).count();
    assert_eq!(
        tests, 1,
        "{SUPPORT_ISOLATION} mutates the process env, so it must hold a single test; \
         a second test thread could read the env mid-mutation"
    );
}

#[test]
fn read_path_folds_crlf_to_lf() {
    let dir = env::temp_dir().join(format!("coyote-read-path-{}", process::id()));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("crlf.txt");
    fs::write(&file, "one\r\ntwo\r\n").unwrap();

    let text = support::repo::read_path(&file);
    fs::remove_dir_all(&dir).unwrap();

    assert_eq!(text, "one\ntwo\n");
}
