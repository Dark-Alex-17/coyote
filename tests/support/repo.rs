//! Files of the repository the tests are built from.

use std::fs;
use std::path::PathBuf;

/// The crate root.
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The file at `rel` under the crate root, with CRLF folded to LF so a
/// Windows checkout reads the same text as a unix one.
pub fn read(rel: &str) -> String {
    let path = root().join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}
