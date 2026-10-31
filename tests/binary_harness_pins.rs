//! Pins the environment every integration harness hands the real binary:
//! each spawn of the built `coyote` must point the child at a temp config
//! dir and a temp cache dir, so nothing a test run spawns lands in the
//! developer's real config or cache. The pin reads the harness sources
//! themselves, keyed on the spawn site, so a new harness that boots the
//! binary without pinning either dir fails here before it ever runs.

use std::fs;
use std::path::Path;

// Spelled in two halves so this file's own source does not count as a
// spawn site when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const CONFIG_ENV: &str = ".env(\"COYOTE_CONFIG_DIR\"";
const CACHE_ENV: &str = ".env(\"COYOTE_CACHE_DIR\"";
const XDG_ENV: &str = ".env(\"XDG_CACHE_HOME\"";
const PATH_PIN: &str = ".env(\"PATH\", pinned_path(";
const MEMORY_ROOT_SCRUB: &str = ".env_remove(TEST_WORKSPACE_MEMORY_ROOT_ENV)";
const MEMORY_ROOT_MIRROR: &str = "const TEST_WORKSPACE_MEMORY_ROOT_ENV: &str = ";
/// The spellings a harness uses to build a command around the binary.
const BINARY_COMMANDS: [&str; 3] = [
    "Command::new(binary)",
    "Command::new(COYOTE_BIN)",
    "Command::new(env!(",
];

/// The graph-session harnesses run fixture hooks and scripts through the
/// binary, so on top of the dir pins every spawn there pins PATH and scrubs
/// the unit-test workspace-memory override.
const GRAPH_SESSION_HARNESSES: [&str; 2] = [
    "graph_session_block_binary.rs",
    "graph_session_repl_binary.rs",
];

#[test]
fn every_binary_spawn_in_the_harnesses_pins_the_config_dir_and_the_cache_dir() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut pinned = Vec::new();
    for entry in fs::read_dir(&tests_dir).expect("tests dir is readable") {
        let path = entry.expect("tests dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let spawn_sites = source.matches(SPAWN_SITE).count();
        let config_sites = source.matches(CONFIG_ENV).count();
        let cache_sites = source.matches(CACHE_ENV).count();
        let xdg_sites = source.matches(XDG_ENV).count();
        if spawn_sites == 0 && config_sites == 0 && cache_sites == 0 {
            continue;
        }
        // Two surpluses are legitimate. The candidate harnesses also pin
        // `COYOTE_CACHE_DIR` on the python battery `Command`s they spawn,
        // which never run the binary. And a spawn of the binary may pin its
        // cache through `XDG_CACHE_HOME` instead, the engine's middle cache
        // tier, so XDG sites count as cache pins.
        assert!(
            config_sites >= spawn_sites && cache_sites + xdg_sites >= spawn_sites,
            "{}: {spawn_sites} binary spawn(s), {config_sites} COYOTE_CONFIG_DIR site(s), \
             {cache_sites} COYOTE_CACHE_DIR site(s), {xdg_sites} XDG_CACHE_HOME site(s); every \
             spawn of the binary must pin the config dir and one of the two cache dirs, so the \
             config count and the combined cache count must each be at least the spawn count",
            path.display()
        );
        pinned.push(path.file_name().unwrap().to_string_lossy().into_owned());
    }
    assert!(
        pinned.len() >= 9,
        "expected the known binary harnesses to be scanned, found {pinned:?}"
    );
}

#[test]
fn every_graph_session_harness_spawn_pins_path_and_scrubs_the_memory_root_override() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let memory_source = fs::read_to_string(root.join("src/config/memory.rs"))
        .expect("src/config/memory.rs is readable");
    let source_name = memory_source
        .lines()
        .find_map(|line| {
            line.split_once("WORKSPACE_MEMORY_ROOT_TEST_ENV: &str = ")
                .map(|(_, value)| value.trim_end_matches(';'))
        })
        .expect("src/config/memory.rs declares the workspace-memory root override");
    for name in GRAPH_SESSION_HARNESSES {
        let path = root.join("tests").join(name);
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let spawn_sites = source.matches(SPAWN_SITE).count();
        let command_sites: usize = BINARY_COMMANDS
            .iter()
            .map(|command| source.matches(command).count())
            .sum();
        let path_sites = source.matches(PATH_PIN).count();
        let scrub_sites = source.matches(MEMORY_ROOT_SCRUB).count();
        assert!(
            spawn_sites > 0
                && spawn_sites == command_sites
                && command_sites == path_sites
                && path_sites == scrub_sites,
            "{name}: {spawn_sites} binary spawn(s), {command_sites} binary command(s), \
             {path_sites} PATH pin(s), {scrub_sites} memory-root scrub(s); every spawn must pin \
             PATH and scrub the override"
        );
        let mirror = format!("{MEMORY_ROOT_MIRROR}{source_name};");
        assert!(
            source.contains(&mirror),
            "{name} must declare `{mirror}` to mirror src/config/memory.rs"
        );
    }
}
