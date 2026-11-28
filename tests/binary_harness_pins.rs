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
//! CRLF to LF, and `support::bin::isolate_from` sets exactly the pinned vars,
//! removes inherited `COYOTE_*` keys only, and lets later vars win.

mod support;

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use support::bin::{Dirs, PINNED_VARS, coyote_command, isolate_from};

// Spelled in two halves so this file's own source does not count as a
// spawn site when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const SUPPORT_BIN: &str = "tests/support/bin.rs";
// Split for the same reason: a bare `Command` around the binary path skips
// the isolation coyote_command applies.
const BARE_SPAWNS: [&str; 4] = [
    concat!("Command::new(", "support::bin::path()"),
    concat!("Command::new(", "bin::path()"),
    concat!("Command::new(", "path())"),
    concat!("Command::new(", "binary)"),
];
const PATH_CALL: &str = concat!("bin::", "path()");
const GRAPH_SESSION_HARNESSES: [&str; 2] = [
    "tests/graph_session_block_binary.rs",
    "tests/graph_session_repl_binary.rs",
];

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
fn isolate_from_pins_the_dirs_removes_inherited_coyote_keys_only_and_lets_later_vars_win() {
    let dirs = distinct_dirs();
    let mut command = Command::new("true");
    isolate_from(&mut command, &dirs, ["COYOTE_ISOLATION_PROBE", "PATH"]);
    command.env("COYOTE_LATER", "1");
    let envs = envs_of(&command);

    for (key, dir) in expected_pins(&dirs) {
        assert_eq!(
            envs.get(OsStr::new(key)),
            Some(&Some(dir.as_os_str().to_os_string())),
            "isolate_from must set {key} from Dirs"
        );
    }
    let mut set: Vec<&OsStr> = envs
        .iter()
        .filter(|(key, value)| value.is_some() && key.as_os_str() != "COYOTE_LATER")
        .map(|(key, _)| key.as_os_str())
        .collect();
    let mut listed: Vec<&OsStr> = PINNED_VARS.iter().map(OsStr::new).collect();
    set.sort_unstable();
    listed.sort_unstable();
    assert_eq!(
        set, listed,
        "PINNED_VARS must name exactly the vars isolate_from sets"
    );
    assert_eq!(
        envs.get(OsStr::new("COYOTE_ISOLATION_PROBE")),
        Some(&None),
        "isolate_from must remove an inherited COYOTE_* key"
    );
    assert_eq!(
        envs.get(OsStr::new("PATH")),
        None,
        "isolate_from must leave an inherited non-COYOTE_ key alone"
    );
    assert_eq!(
        envs.get(OsStr::new("COYOTE_LATER")),
        Some(&Some(OsString::from("1"))),
        "a variable set after isolate_from must reach the child"
    );
}

#[test]
fn coyote_command_targets_the_built_binary_with_the_dir_pins_and_removes_this_process_coyote_vars()
{
    let dirs = distinct_dirs();
    let command = coyote_command(&dirs);
    assert_eq!(command.get_program(), support::bin::path());
    let envs = envs_of(&command);
    for (key, dir) in expected_pins(&dirs) {
        assert_eq!(
            envs.get(OsStr::new(key)),
            Some(&Some(dir.as_os_str().to_os_string())),
            "coyote_command must set {key} from Dirs"
        );
    }
    let mut removed: Vec<OsString> = envs
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.clone())
        .collect();
    let mut inherited: Vec<OsString> = env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| {
            let key = key.to_string_lossy();
            key.starts_with("COYOTE_") && !PINNED_VARS.contains(&key.as_ref())
        })
        .collect();
    removed.sort();
    inherited.sort();
    assert_eq!(
        removed, inherited,
        "coyote_command must remove exactly the COYOTE_* vars this test process inherited"
    );
}

fn distinct_dirs() -> Dirs {
    let base = env::temp_dir().join("coyote-isolate-from");
    Dirs {
        config: base.join("config"),
        cache: base.join("cache"),
        home: base.join("home"),
        temp: base.join("temp"),
    }
}

fn expected_pins(dirs: &Dirs) -> [(&'static str, &PathBuf); 7] {
    [
        ("COYOTE_CONFIG_DIR", &dirs.config),
        ("COYOTE_CACHE_DIR", &dirs.cache),
        ("HOME", &dirs.home),
        ("USERPROFILE", &dirs.home),
        ("TMPDIR", &dirs.temp),
        ("TMP", &dirs.temp),
        ("TEMP", &dirs.temp),
    ]
}

fn envs_of(command: &Command) -> HashMap<OsString, Option<OsString>> {
    command
        .get_envs()
        .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
        .collect()
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
