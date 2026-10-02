//! The one versioning discipline every mesh on-disk store follows. Each store carries a
//! `version` per file or per line; a reader probes it before any other field and refuses
//! the whole store, never one record, when it is not the version this build writes. The
//! message builders here are pure so every store's refusal reads the same and the tests
//! that pin the wording have one place to look.

use serde::Deserialize;
use std::path::Path;

/// The `version` field alone. Read first so a store from another layout is refused by its
/// version rather than by whatever field that layout added or dropped.
#[derive(Deserialize)]
pub(crate) struct VersionProbe {
    pub version: u64,
}

/// What the person can do about a store this build refuses. The remedy differs by what
/// the file is worth, not by why it was refused.
#[derive(Clone, Copy)]
pub(crate) enum Remedy {
    /// The file is cache: moving it aside loses nothing.
    Cache,
    /// The file is the user's own. The text says what it holds, so they can judge what
    /// moving it aside costs.
    UserFile(&'static str),
}

impl Remedy {
    pub(crate) fn sentence(self) -> String {
        match self {
            Self::Cache => "It is cache: move the file aside to start fresh.".to_string(),
            Self::UserFile(holds) => {
                format!("Fix the file or move the file aside to start fresh; it {holds}.")
            }
        }
    }
}

fn subject(store: &str, path: &Path, line: Option<usize>) -> String {
    match line {
        Some(line) => format!("Mesh {store} '{}' line {line}", path.display()),
        None => format!("Mesh {store} '{}'", path.display()),
    }
}

/// The refusal for a store whose version is not `expected`. A newer version means a newer
/// Coyote wrote it; an older one has no migration. The baseline is the version this build
/// writes (2 for the trust file, knock records, peer table and the pending and inbound
/// records, 1 elsewhere).
pub(crate) fn version_refusal(
    store: &str,
    path: &Path,
    line: Option<usize>,
    found: u64,
    expected: u64,
    remedy: Remedy,
) -> String {
    debug_assert_ne!(found, expected);
    let cause = if found < expected {
        format!("It predates this layout and no migration exists for versions before {expected}.")
    } else {
        "A newer Coyote wrote it: upgrade Coyote to read it.".to_string()
    };
    format!(
        "{} is version {found} but this Coyote writes version {expected}. {cause} {}",
        subject(store, path, line),
        remedy.sentence()
    )
}

/// The predicate every unversioned refusal shares, whether the store refuses outright or
/// sets the file aside with this as its cause.
pub(crate) fn unversioned_cause(expected: u64) -> String {
    format!(
        "has no readable `version` field, so its shape is unknown; this Coyote writes version {expected}"
    )
}

/// The refusal for a store whose version cannot be read at all, which leaves its shape
/// unknown: nothing about it can be trusted, not even that it is a store.
pub(crate) fn unversioned_refusal(
    store: &str,
    path: &Path,
    line: Option<usize>,
    expected: u64,
    remedy: Remedy,
) -> String {
    format!(
        "{} {}. {}",
        subject(store, path, line),
        unversioned_cause(expected),
        remedy.sentence()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Every type serde reads from a mesh store, by file. A type missing here is one the
    /// version discipline does not cover, so the list is the contract, not a mirror.
    const ON_DISK_STRUCTS: &[(&str, &[&str])] = &[
        (
            "trust.rs",
            &[
                "TrustFile",
                "IdentityEntry",
                "DestinationEntry",
                "KeyChanged",
                "OverlayEntry",
            ],
        ),
        ("knocks.rs", &["KnockRecord"]),
        ("pending.rs", &["PendingRecord", "InboundRecord"]),
        ("message.rs", &["PeerMessage", "Part"]),
        ("identity.rs", &["Predecessor"]),
        ("peers.rs", &["PeerTableFile", "PeerRecord"]),
        ("protocol.rs", &["Compatibility"]),
        (
            "propagation_fetch.rs",
            &[
                "StoreFile",
                "SeenRecord",
                "DeliveredRecord",
                "DeferredRecord",
                "CursorRecord",
            ],
        ),
        (
            "shares.rs",
            &["SharesFile", "AllowEntry", "DenyEntry", "OverrideEntry"],
        ),
        ("grants.rs", &["GrantRecord", "GrantedPath"]),
    ];

    /// Types that derive `Deserialize` but need no `deny_unknown_fields`: a fieldless enum
    /// has no field map for an unknown key to hide in, and the probe is lenient by design.
    const EXEMPT: &[(&str, &str)] = &[
        // Reads `version` alone from a document whose other fields are unknown on purpose.
        ("schema.rs", "VersionProbe"),
        // Fieldless enums, serialized as bare strings.
        ("pending.rs", "PendingState"),
        ("pending.rs", "InboundKind"),
        ("message.rs", "PeerKind"),
        ("message.rs", "PeerVia"),
        ("message.rs", "Disposition"),
    ];

    /// The name a `struct`/`enum` line declares, with any generics, tuple body or brace
    /// trimmed off.
    fn declared_type(line: &str) -> Option<&str> {
        let mut words = line
            .split_whitespace()
            .skip_while(|word| word.starts_with("pub"));
        if !matches!(words.next(), Some("struct" | "enum")) {
            return None;
        }
        let word = words.next()?;
        let end = word
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(word.len());
        Some(&word[..end]).filter(|name| !name.is_empty())
    }

    /// The attribute and doc lines directly above `index`, walking upward over `#[`,
    /// `///` and the continuation lines of a multi-line attribute until any other line.
    fn header_above<'a>(lines: &[&'a str], index: usize) -> Vec<&'a str> {
        lines[..index]
            .iter()
            .rev()
            .map(|line| line.trim())
            .take_while(|line| {
                line.starts_with("#[")
                    || line.starts_with("///")
                    || line.ends_with(',')
                    || line.ends_with(")]")
            })
            .collect()
    }

    fn attributes_above<'a>(lines: &[&'a str], index: usize) -> Vec<&'a str> {
        header_above(lines, index)
            .into_iter()
            .filter(|line| line.starts_with("#["))
            .collect()
    }

    fn rust_files_under(dir: &Path, skip: &[PathBuf], out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if skip.contains(&path) {
                continue;
            }
            if path.is_dir() {
                rust_files_under(&path, skip, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// An inline module body, `mod name {`; a file module declaration `mod name;` is not
    /// a test body and leaves the scan running.
    fn opens_module_body(line: &str) -> bool {
        line.split_whitespace()
            .find(|word| !word.starts_with("pub"))
            == Some("mod")
            && line.trim_end().ends_with('{')
    }

    /// The lines of a source file outside its test-only inline modules. A `#[cfg(test)]`
    /// on an import, alias, helper or file module (`mod fuzz;`) leaves the scan running;
    /// one gating an inline module body (`mod test_support {`, `mod tests {`) skips that
    /// body, brace-matched, and the scan resumes after it.
    fn non_test_lines(source: &str) -> Vec<&str> {
        let lines: Vec<&str> = source.lines().collect();
        let mut kept = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            let line = lines[index];
            let gated_body = line.starts_with("#[cfg(test)]")
                && lines[index + 1..]
                    .iter()
                    .find(|next| !next.trim().is_empty())
                    .is_some_and(|next| opens_module_body(next));
            if !(gated_body || line.starts_with("mod tests")) {
                kept.push(line);
                index += 1;
                continue;
            }
            let mut depth = 0usize;
            let mut opened = false;
            while index < lines.len() {
                for c in lines[index].chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                }
                index += 1;
                if opened && depth == 0 {
                    break;
                }
            }
        }
        kept
    }

    #[test]
    fn non_test_lines_stops_at_the_tests_module_not_at_the_first_cfg_test() {
        let source = "use a::B;\n\
                      #[cfg(test)]\n\
                      use x::Y;\n\
                      #[cfg(test)]\n\
                      mod fixtures;\n\
                      \n\
                      #[cfg(test)]\n\
                      pub(crate) mod test_support {\n\
                          pub(crate) struct Fixture {\n\
                              pub(crate) x: u8,\n\
                          }\n\
                      }\n\
                      \n\
                      #[derive(Deserialize)]\n\
                      struct Late { x: u8 }\n\
                      \n\
                      #[cfg(test)]\n\
                      mod tests {\n\
                          struct OnlyInTests;\n\
                      }\n";
        let lines = non_test_lines(source);

        assert!(lines.contains(&"use x::Y;"), "{lines:#?}");
        assert!(lines.contains(&"mod fixtures;"), "{lines:#?}");
        assert!(lines.contains(&"struct Late { x: u8 }"), "{lines:#?}");
        assert!(
            !lines.iter().any(|line| line.contains("Fixture")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("mod tests")),
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("OnlyInTests")),
            "{lines:#?}"
        );

        let mesh = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mesh");
        let node = fs::read_to_string(mesh.join("node.rs")).unwrap();
        let scanned = non_test_lines(&node).len();
        assert!(scanned > 2000, "node.rs scan ended after {scanned} lines");
        assert!(scanned < node.lines().count(), "node.rs has no test module");
        let root = fs::read_to_string(mesh.join("mod.rs")).unwrap();
        let kept = non_test_lines(&root);
        assert!(
            kept.iter()
                .any(|line| line.starts_with("pub(crate) fn write_atomically")),
            "mod.rs production code before test_support is scanned"
        );
        assert!(
            !kept
                .iter()
                .any(|line| line.contains("mod test_support") || line.contains("mod tests")),
            "mod.rs test-only modules are skipped: {kept:#?}"
        );
    }

    #[test]
    fn every_on_disk_struct_rejects_unknown_fields() {
        let mesh = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mesh");
        for (file, names) in ON_DISK_STRUCTS {
            let source = fs::read_to_string(mesh.join(file)).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            for name in *names {
                let index = lines
                    .iter()
                    .position(|line| declared_type(line) == Some(name))
                    .unwrap_or_else(|| panic!("src/mesh/{file} declares no struct or enum {name}"));
                assert!(
                    attributes_above(&lines, index)
                        .iter()
                        .any(|line| line.contains("deny_unknown_fields")),
                    "src/mesh/{file}: {name} is read from disk but does not carry #[serde(deny_unknown_fields)]"
                );
            }
        }
    }

    #[test]
    fn every_deserializable_mesh_type_is_classified() {
        let mesh = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mesh");
        let mut files = Vec::new();
        rust_files_under(
            &mesh,
            &[mesh.join("conformance"), mesh.join("fuzz")],
            &mut files,
        );
        let mut unclassified = Vec::new();
        for path in files {
            let file = path
                .strip_prefix(&mesh)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let source = fs::read_to_string(&path).unwrap();
            let lines = non_test_lines(&source);
            for (index, line) in lines.iter().enumerate() {
                let Some(name) = declared_type(line) else {
                    continue;
                };
                let derives_deserialize = header_above(&lines, index)
                    .iter()
                    .any(|line| !line.starts_with("///") && line.contains("Deserialize"));
                if !derives_deserialize {
                    continue;
                }
                let on_disk = ON_DISK_STRUCTS
                    .iter()
                    .any(|(f, names)| *f == file && names.contains(&name));
                let exempt = EXEMPT.iter().any(|(f, n)| *f == file && *n == name);
                if !on_disk && !exempt {
                    unclassified.push(format!("src/mesh/{file} {name}"));
                }
            }
        }
        assert!(
            unclassified.is_empty(),
            "types deriving Deserialize that are neither in ON_DISK_STRUCTS nor EXEMPT: {unclassified:#?}"
        );
    }

    #[test]
    fn every_on_disk_store_version_is_pinned() {
        let versions = [
            (
                "TRUST_FILE_VERSION",
                crate::mesh::trust::TRUST_FILE_VERSION,
                2,
            ),
            (
                "KNOCK_RECORD_VERSION",
                crate::mesh::knocks::KNOCK_RECORD_VERSION,
                2,
            ),
            (
                "PENDING_RECORD_VERSION",
                crate::mesh::pending::PENDING_RECORD_VERSION,
                2,
            ),
            (
                "INBOUND_RECORD_VERSION",
                crate::mesh::pending::INBOUND_RECORD_VERSION,
                2,
            ),
            (
                "PREDECESSOR_RECORD_VERSION",
                crate::mesh::identity::PREDECESSOR_RECORD_VERSION,
                1,
            ),
            (
                "PEER_TABLE_VERSION",
                crate::mesh::peers::PEER_TABLE_VERSION,
                2,
            ),
            (
                "PROPAGATION_STORE_VERSION",
                crate::mesh::propagation_fetch::PROPAGATION_STORE_VERSION,
                1,
            ),
            (
                "SHARES_FILE_VERSION",
                crate::mesh::shares::SHARES_FILE_VERSION,
                1,
            ),
            (
                "GRANT_RECORD_VERSION",
                crate::mesh::grants::GRANT_RECORD_VERSION,
                1,
            ),
        ];
        for (name, version, pinned) in versions {
            assert_eq!(
                version, pinned,
                "{name} moved: a bump ships a migration or a refusal, and its section 19 row moves with it"
            );
        }
    }

    #[test]
    fn a_newer_version_asks_for_an_upgrade_and_names_both_versions() {
        let path = PathBuf::from("/tmp/mesh/knocks.jsonl");
        let text = version_refusal("knock cache", &path, Some(3), 2, 1, Remedy::Cache);

        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("line 3"), "{text}");
        assert!(text.contains("version 2"), "{text}");
        assert!(text.contains("version 1"), "{text}");
        assert!(text.contains("upgrade Coyote"), "{text}");
        assert!(text.contains("move the file aside"), "{text}");
        assert!(!text.contains("no migration"), "{text}");
    }

    #[test]
    fn an_older_version_says_no_migration_exists_and_names_the_baseline() {
        let path = PathBuf::from("/tmp/mesh/trust.yaml");
        let text = version_refusal(
            "trust list",
            &path,
            None,
            0,
            1,
            Remedy::UserFile("is the trust list, and a fresh one trusts nobody"),
        );

        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(!text.contains("line"), "{text}");
        assert!(text.contains("version 0"), "{text}");
        assert!(text.contains("version 1"), "{text}");
        assert!(
            text.contains("no migration exists for versions before 1"),
            "{text}"
        );
        assert!(text.contains("move the file aside"), "{text}");
        assert!(text.contains("trusts nobody"), "{text}");
        assert!(!text.contains("upgrade Coyote"), "{text}");
    }

    #[test]
    fn an_unreadable_version_is_an_unknown_shape() {
        let path = PathBuf::from("/tmp/mesh/peers.json");
        let text = unversioned_refusal("peer table", &path, None, 1, Remedy::Cache);

        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("no readable `version` field"), "{text}");
        assert!(text.contains(&unversioned_cause(1)), "{text}");
        assert!(text.contains("this Coyote writes version 1"), "{text}");
        assert!(
            text.ends_with(". It is cache: move the file aside to start fresh."),
            "{text}"
        );
    }
}
