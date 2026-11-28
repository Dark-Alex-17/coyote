//! Pins the environment every integration harness hands the real binary:
//! the built `coyote` is named in one place, `tests/support/bin.rs`, so every
//! spawn goes through `support::bin::coyote_command` (or `isolate` for a
//! wrapper such as `script`). That pins the config, cache, home and temp dirs
//! and clears every inherited `COYOTE_*` variable, the unit-test
//! workspace-memory root override among them, so nothing a test run spawns
//! lands in the developer's real config or cache or picks up their overrides.

use std::fs;
use std::path::{Path, PathBuf};

// Spelled in two halves so this file's own source does not count as a
// spawn site when the scan reaches it.
const SPAWN_SITE: &str = concat!("CARGO_BIN_EXE_", "coyote");
const SUPPORT_BIN: &str = "tests/support/bin.rs";

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

#[test]
fn every_spawn_goes_through_coyote_command_which_pins_the_dirs_and_clears_inherited_coyote_vars() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    rust_sources(&root.join("tests"), &mut sources);
    let naming: Vec<String> = sources
        .iter()
        .filter(|path| {
            fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
                .contains(SPAWN_SITE)
        })
        .map(|path| {
            path.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(
        naming,
        [SUPPORT_BIN],
        "only {SUPPORT_BIN} may name the built binary; every spawn goes through \
         support::bin::coyote_command, which pins the config, cache, home and temp dirs \
         and clears every inherited COYOTE_* variable"
    );
}
