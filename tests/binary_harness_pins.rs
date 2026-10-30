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
        // A file that never spawns the binary is not one of its harnesses: a script
        // harness may hand the script under test its own config dir.
        if spawn_sites == 0 {
            continue;
        }
        assert!(
            spawn_sites == config_sites && config_sites == cache_sites,
            "{}: {spawn_sites} binary spawn(s), {config_sites} COYOTE_CONFIG_DIR site(s), \
             {cache_sites} COYOTE_CACHE_DIR site(s); every spawn of the binary must pin both dirs",
            path.display()
        );
        pinned.push(path.file_name().unwrap().to_string_lossy().into_owned());
    }
    assert!(
        pinned.len() >= 9,
        "expected the known binary harnesses to be scanned, found {pinned:?}"
    );
}
