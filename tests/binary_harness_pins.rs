//! Pins the environment every integration harness hands the real binary:
//! a harness that points the child at a temp config dir must point it at a
//! temp cache dir too, so nothing a test run spawns lands in the developer's
//! real cache. The pin reads the harness sources themselves, so a new harness
//! that sets only `COYOTE_CONFIG_DIR` fails here before it ever runs.

use std::fs;
use std::path::Path;

const CONFIG_ENV: &str = ".env(\"COYOTE_CONFIG_DIR\"";
const CACHE_ENV: &str = ".env(\"COYOTE_CACHE_DIR\"";

#[test]
fn every_binary_harness_that_pins_the_config_dir_pins_the_cache_dir_too() {
    let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut pinned = Vec::new();
    for entry in fs::read_dir(&tests_dir).expect("tests dir is readable") {
        let path = entry.expect("tests dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let config_sites = source.matches(CONFIG_ENV).count();
        let cache_sites = source.matches(CACHE_ENV).count();
        if config_sites == 0 && cache_sites == 0 {
            continue;
        }
        assert_eq!(
            config_sites,
            cache_sites,
            "{}: {config_sites} COYOTE_CONFIG_DIR site(s) but {cache_sites} COYOTE_CACHE_DIR site(s); \
             every Command that pins the config dir must pin the cache dir beside it",
            path.display()
        );
        pinned.push(path.file_name().unwrap().to_string_lossy().into_owned());
    }
    assert!(
        pinned.len() >= 9,
        "expected the known binary harnesses to be scanned, found {pinned:?}"
    );
}
