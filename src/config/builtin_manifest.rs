//! Per-directory tracking of files the builtin installer shipped, so a later
//! release that stops shipping an asset can remove the stale copy without
//! ever touching user-created files that live in the same directory.
//!
//! Unlike agent definition files, hook filenames are open-ended: no closed
//! candidate list can distinguish "previously bundled" from "user-authored".
//! The manifest records exactly what the installer shipped; anything absent
//! from it is user-owned and untouchable.

use crate::config::ensure_parent_exists;
use crate::function::write_file_atomic;

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

pub(crate) const BUILTIN_MANIFEST_FILE: &str = ".builtin-manifest";

pub(crate) fn is_builtin_manifest_name(name: &str) -> bool {
    name.trim_end_matches(['.', ' '])
        .to_uppercase()
        .eq_ignore_ascii_case(BUILTIN_MANIFEST_FILE)
}

/// Reconciles `dir` against the currently shipped set of builtin filenames:
/// a file is removed ONLY if it appears in the previous manifest AND is
/// absent from `shipped`. The manifest is then rewritten to the names this
/// installer actually owns: files written this run (`written`), plus shipped
/// names it already owned, plus stale files whose removal failed (kept so a
/// later run retries). A shipped name the installer declined to write (e.g., a
/// pre-existing user file with a colliding name) is never claimed.
///
/// Failure modes degrade toward keeping files: a missing or unreadable
/// manifest yields no deletions, and entries that are not plain filenames
/// are ignored.
pub(crate) fn reconcile_builtin_dir(
    dir: &Path,
    shipped: &BTreeSet<String>,
    written: &BTreeSet<String>,
) -> Result<()> {
    let previous = read_manifest(dir);
    let mut retained: BTreeSet<String> = written.clone();
    retained.extend(shipped.intersection(&previous).cloned());
    for name in previous.difference(shipped) {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        info!(
            "Removing stale builtin file no longer shipped: {}",
            path.display()
        );
        if let Err(err) = fs::remove_file(&path) {
            warn!(
                "Failed to remove stale builtin file {}: {err}",
                path.display()
            );
            retained.insert(name.clone());
        }
    }

    write_manifest(dir, &retained)
}

fn read_manifest(dir: &Path) -> BTreeSet<String> {
    let path = dir.join(BUILTIN_MANIFEST_FILE);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            debug!(
                "No readable builtin manifest at {} ({err}); skipping stale-file removal",
                path.display()
            );
            return BTreeSet::new();
        }
    };

    content
        .lines()
        .map(str::trim)
        .filter(|line| {
            if line.is_empty() || is_builtin_manifest_name(line) {
                return false;
            }
            if !is_plain_file_name(line) {
                debug!(
                    "Ignoring suspicious builtin manifest entry in {}: {line:?}",
                    path.display()
                );
                return false;
            }
            true
        })
        .map(str::to_string)
        .collect()
}

/// Manifest entries name direct children of the manifest's directory and
/// nothing else; anything path-like is rejected so a corrupted or crafted
/// manifest can never direct a deletion outside that directory. The same
/// component rules the bundle store applies to recorded paths are reused so
/// Windows drive prefixes (`C:x`), ADS colons, trailing dots/spaces, and
/// reserved device names are rejected too.
fn is_plain_file_name(name: &str) -> bool {
    !name.contains(['/', '\\'])
        && name != "."
        && name != ".."
        && super::install_remote::is_safe_component(name)
}

fn write_manifest(dir: &Path, names: &BTreeSet<String>) -> Result<()> {
    let path = dir.join(BUILTIN_MANIFEST_FILE);
    if names.is_empty() {
        if path.is_file() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove builtin manifest {}", path.display()))?;
        }
        return Ok(());
    }

    let mut content = names.iter().cloned().collect::<Vec<_>>().join("\n");
    content.push('\n');
    ensure_parent_exists(&path)?;
    write_file_atomic(&path, &content, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;
    use std::path::PathBuf;

    fn fresh_dir(label: &str) -> PathBuf {
        let dir = utils::temp_file(label, "");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shipped(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn removes_only_manifest_listed_files_absent_from_shipped() {
        let dir = fresh_dir("builtin-manifest-stale-");
        fs::write(dir.join(BUILTIN_MANIFEST_FILE), "old.sh\nkept.sh\n").unwrap();
        fs::write(dir.join("old.sh"), "stale").unwrap();
        fs::write(dir.join("kept.sh"), "current").unwrap();
        fs::write(dir.join("user.sh"), "user-owned").unwrap();

        reconcile_builtin_dir(&dir, &shipped(&["kept.sh"]), &shipped(&[])).unwrap();

        assert!(!dir.join("old.sh").exists(), "stale bundled file removed");
        assert!(dir.join("kept.sh").exists(), "still-shipped file kept");
        assert!(dir.join("user.sh").exists(), "unlisted user file untouched");
        assert_eq!(
            fs::read_to_string(dir.join(BUILTIN_MANIFEST_FILE)).unwrap(),
            "kept.sh\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_manifest_deletes_nothing_and_writes_shipped_set() {
        let dir = fresh_dir("builtin-manifest-missing-");
        fs::write(dir.join("user.sh"), "user-owned").unwrap();

        reconcile_builtin_dir(&dir, &shipped(&["new.sh"]), &shipped(&["new.sh"])).unwrap();

        assert!(dir.join("user.sh").exists());
        assert_eq!(
            fs::read_to_string(dir.join(BUILTIN_MANIFEST_FILE)).unwrap(),
            "new.sh\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A shipped name the installer skipped (pre-existing user file with a
    /// colliding name) is never claimed by the manifest, so a later release
    /// dropping that hook can never delete the user's file.
    #[test]
    fn skipped_preexisting_user_file_is_never_claimed() {
        let dir = fresh_dir("builtin-manifest-unclaimed-");
        fs::write(dir.join("user.sh"), "user-owned").unwrap();

        reconcile_builtin_dir(&dir, &shipped(&["user.sh"]), &shipped(&[])).unwrap();
        assert!(
            !dir.join(BUILTIN_MANIFEST_FILE).exists(),
            "nothing was written this run and nothing was owned before"
        );

        reconcile_builtin_dir(&dir, &shipped(&[]), &shipped(&[])).unwrap();
        assert!(
            dir.join("user.sh").exists(),
            "the unclaimed user file survives the hook being dropped"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn suspicious_manifest_entries_never_direct_deletions() {
        let dir = fresh_dir("builtin-manifest-suspicious-");
        let outside = fresh_dir("builtin-manifest-outside-");
        let escape = format!(
            "../{}/victim.sh",
            outside.file_name().unwrap().to_string_lossy()
        );
        fs::write(
            dir.join(BUILTIN_MANIFEST_FILE),
            format!("{escape}\nsub/nested.sh\nC:victim.sh\ntrailing.\n..\n.\n\n"),
        )
        .unwrap();
        fs::write(outside.join("victim.sh"), "keep").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/nested.sh"), "keep").unwrap();

        reconcile_builtin_dir(&dir, &BTreeSet::new(), &BTreeSet::new()).unwrap();

        assert!(outside.join("victim.sh").exists());
        assert!(dir.join("sub/nested.sh").exists());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn unreadable_manifest_deletes_nothing() {
        let dir = fresh_dir("builtin-manifest-unreadable-");
        fs::write(dir.join(BUILTIN_MANIFEST_FILE), [0xff, 0xfe, 0x00]).unwrap();
        fs::write(dir.join("old.sh"), "keep").unwrap();

        reconcile_builtin_dir(&dir, &shipped(&["new.sh"]), &shipped(&["new.sh"])).unwrap();

        assert!(
            dir.join("old.sh").exists(),
            "a bad manifest must fail safe toward keeping files"
        );
        assert_eq!(
            fs::read_to_string(dir.join(BUILTIN_MANIFEST_FILE)).unwrap(),
            "new.sh\n",
            "manifest self-heals to the current shipped set"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_shipped_set_removes_manifest_and_creates_nothing() {
        let dir = fresh_dir("builtin-manifest-empty-");
        fs::write(dir.join(BUILTIN_MANIFEST_FILE), "old.sh\n").unwrap();
        fs::write(dir.join("old.sh"), "stale").unwrap();
        fs::write(dir.join("user.sh"), "user-owned").unwrap();

        reconcile_builtin_dir(&dir, &BTreeSet::new(), &BTreeSet::new()).unwrap();

        assert!(!dir.join("old.sh").exists());
        assert!(dir.join("user.sh").exists());
        assert!(!dir.join(BUILTIN_MANIFEST_FILE).exists());

        let absent = dir.join("never-created");
        reconcile_builtin_dir(&absent, &BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert!(!absent.exists(), "no directory is created for an empty set");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_listing_itself_never_deletes_it() {
        let dir = fresh_dir("builtin-manifest-self-");
        fs::write(
            dir.join(BUILTIN_MANIFEST_FILE),
            format!("{BUILTIN_MANIFEST_FILE}\n.BUILTIN-MANIFEST\nkept.sh\n"),
        )
        .unwrap();
        fs::write(dir.join("kept.sh"), "current").unwrap();

        reconcile_builtin_dir(&dir, &shipped(&["kept.sh"]), &shipped(&[])).unwrap();

        assert!(dir.join(BUILTIN_MANIFEST_FILE).exists());
        assert!(dir.join("kept.sh").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_name_matcher_covers_filesystem_aliases() {
        assert!(is_builtin_manifest_name(".builtin-manifest"));
        assert!(is_builtin_manifest_name(".BUILTIN-MANIFEST"));
        assert!(is_builtin_manifest_name(".Builtin-Manifest."));
        assert!(is_builtin_manifest_name(".builtin-manifest  "));
        assert!(is_builtin_manifest_name(".builtin-manifeſt"));
        assert!(!is_builtin_manifest_name("builtin-manifest"));
        assert!(!is_builtin_manifest_name(".builtin-manifesto"));
        assert!(!is_builtin_manifest_name("notify.sh"));
    }
}
