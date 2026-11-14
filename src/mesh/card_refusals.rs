//! The sessions whose last answer to a background card fetch was a refusal, kept per
//! instance in `<cache_dir>/mesh/card-refusals-<instance_id>.json` so the stop on the
//! fetch outlasts a restart.

use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::{mesh_cache_dir, redact_hashes, write_atomically_owner_only};

use anyhow::{Context, Result, anyhow, bail};
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const CARD_REFUSALS_VERSION: u64 = 1;

/// The whole file. Rejects unknown fields, so a field added later carries
/// `#[serde(default)]` or bumps `CARD_REFUSALS_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CardRefusalsFile {
    pub version: u64,
    /// The refusing destinations, as hex.
    pub refused: BTreeSet<String>,
}

/// Where the instance `instance_id` keeps its refusals.
pub(crate) fn card_refusals_path(cache_dir: &Path, instance_id: &str) -> PathBuf {
    mesh_cache_dir(cache_dir).join(format!("card-refusals-{instance_id}.json"))
}

/// The refusals at `path`; a missing file holds none. A file that cannot be read or
/// parsed, or holds another version, is refused and left in place, so the node does not
/// start over it; the refusal names the path and ends with the remedy.
pub(crate) fn load_card_refusals(path: &Path) -> Result<HashSet<String>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => bail!(
            "Mesh card-refusal list '{}' could not be read: {err}. {}",
            path.display(),
            Remedy::Cache.sentence()
        ),
    };
    let probe: VersionProbe = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow!(unversioned_refusal(
            "card-refusal list",
            path,
            None,
            CARD_REFUSALS_VERSION,
            Remedy::Cache,
        ))
    })?;
    if probe.version != CARD_REFUSALS_VERSION {
        bail!(version_refusal(
            "card-refusal list",
            path,
            None,
            probe.version,
            CARD_REFUSALS_VERSION,
            Remedy::Cache
        ));
    }
    let file: CardRefusalsFile = serde_json::from_slice(&bytes).map_err(|err| {
        anyhow!(
            "Mesh card-refusal list '{}' is not a version-{CARD_REFUSALS_VERSION} card-refusal list: {err}. {}",
            path.display(),
            Remedy::Cache.sentence()
        )
    })?;
    Ok(file.refused.into_iter().collect())
}

/// Writes `refused` to `path`, readable by its owner only since it names the sessions
/// that refused this user. A failed write is only warned about: the set held in memory
/// still stops the fetch for this run.
pub(crate) fn save_card_refusals(path: &Path, refused: &HashSet<String>) {
    let file = CardRefusalsFile {
        version: CARD_REFUSALS_VERSION,
        refused: refused.iter().cloned().collect(),
    };
    let written = serde_json::to_vec_pretty(&file)
        .context("Failed to serialize the mesh card-refusal list")
        .and_then(|json| write_atomically_owner_only(path, &json));
    if let Err(err) = written {
        warn!(
            "Failed to persist mesh card-refusal list: {}",
            redact_hashes(&format!("{err:#}"))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;

    const D: &str = "dddddddddddddddddddddddddddddddd";

    fn path_in(tmp: &TempDir) -> PathBuf {
        card_refusals_path(&tmp.path, "0123456789abcdef0123456789abcdef")
    }

    #[test]
    fn a_missing_file_holds_no_refusals_and_creates_nothing() {
        let tmp = TempDir::new("card-refusals-missing");
        let path = path_in(&tmp);
        assert!(load_card_refusals(&path).unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn refusals_round_trip() {
        let tmp = TempDir::new("card-refusals-round-trip");
        let path = path_in(&tmp);
        let refused = HashSet::from([D.to_string(), "e".repeat(32)]);
        save_card_refusals(&path, &refused);
        assert_eq!(load_card_refusals(&path).unwrap(), refused);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    fn assert_refused_in_place(tag: &str, bytes: &[u8], says: &str) {
        let tmp = TempDir::new(tag);
        let path = path_in(&tmp);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let err = format!("{:#}", load_card_refusals(&path).unwrap_err());
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(says), "{err}");
        assert!(err.ends_with(&Remedy::Cache.sentence()), "{err}");
        let words = err.replace(&path.display().to_string(), "").to_lowercase();
        for word in ["peer", "owner", "node", "person"] {
            assert!(
                !words.contains(word),
                "user-facing text says {word:?}: {err}"
            );
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "the file is left untouched"
        );
    }

    #[test]
    fn a_corrupt_file_is_refused_in_place() {
        assert_refused_in_place(
            "card-refusals-corrupt",
            b"{\"version\": 1, \"refused\": [",
            "card-refusal list",
        );
        assert_refused_in_place(
            "card-refusals-unknown-field",
            b"{\"version\": 1, \"refused\": [], \"later\": true}",
            "is not a version-1 card-refusal list",
        );
        assert_refused_in_place(
            "card-refusals-unversioned",
            b"{\"refused\": []}",
            "has no readable `version` field",
        );
    }

    #[test]
    fn another_version_is_refused_in_place() {
        assert_refused_in_place(
            "card-refusals-newer",
            b"{\"version\": 2, \"refused\": [\"dd\"]}",
            "upgrade Coyote",
        );
    }
}
