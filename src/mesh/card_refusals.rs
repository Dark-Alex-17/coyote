//! The sessions whose last answer to a background card fetch was a refusal, kept per
//! instance in `<cache_dir>/mesh/card-refusals-<instance_id>.json` so the stop on the
//! fetch outlasts a restart.

use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::sent_threads::SENT_THREADS_MAX_ENTRIES;
use crate::mesh::{mesh_cache_dir, redact_hashes, write_atomically_owner_only};

use anyhow::{Context, Result, anyhow, bail};
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const CARD_REFUSALS_VERSION: u64 = 1;

/// Upper bound on the sessions held as refusing, the same as on the remembered threads;
/// past it the oldest stop goes first.
pub(crate) const CARD_REFUSALS_MAX_ENTRIES: usize = SENT_THREADS_MAX_ENTRIES;

/// The whole file. Rejects unknown fields, so a field added later carries
/// `#[serde(default)]` or bumps `CARD_REFUSALS_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CardRefusalsFile {
    pub version: u64,
    /// The refusing destinations, as hex, oldest first.
    pub refused: Vec<String>,
}

/// The refusing destinations in the order they refused, oldest first, no more than
/// `CARD_REFUSALS_MAX_ENTRIES` of them.
#[derive(Debug, Default)]
pub(crate) struct CardRefusals {
    order: VecDeque<String>,
    index: HashSet<String>,
}

impl CardRefusals {
    /// The refusals `oldest_first` names, each once at its newest place, the newest
    /// `CARD_REFUSALS_MAX_ENTRIES` of them.
    fn from_oldest_first(oldest_first: impl IntoIterator<Item = String>) -> Self {
        let mut refusals = Self::default();
        for destination in oldest_first {
            refusals.remove(&destination);
            refusals.insert(destination);
        }
        refusals
    }

    pub(crate) fn contains(&self, destination: &str) -> bool {
        self.index.contains(destination)
    }

    /// Adds `destination` as the newest refusal, dropping the oldest past the cap;
    /// `false` when it was already held.
    pub(crate) fn insert(&mut self, destination: String) -> bool {
        if !self.index.insert(destination.clone()) {
            return false;
        }
        self.order.push_back(destination);
        while self.order.len() > CARD_REFUSALS_MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.index.remove(&oldest);
            }
        }
        true
    }

    /// `false` when `destination` was not held.
    pub(crate) fn remove(&mut self, destination: &str) -> bool {
        if !self.index.remove(destination) {
            return false;
        }
        self.order.retain(|held| held != destination);
        true
    }

    /// Keeps only the refusals `keep` holds to; `true` when any went.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) -> bool {
        let before = self.order.len();
        self.order.retain(|held| keep(held));
        if self.order.len() == before {
            return false;
        }
        self.index = self.order.iter().cloned().collect();
        true
    }

    /// Oldest first, as the file holds them.
    pub(crate) fn to_vec(&self) -> Vec<String> {
        self.order.iter().cloned().collect()
    }
}

/// Where the instance `instance_id` keeps its refusals.
pub(crate) fn card_refusals_path(cache_dir: &Path, instance_id: &str) -> PathBuf {
    mesh_cache_dir(cache_dir).join(format!("card-refusals-{instance_id}.json"))
}

/// The refusals at `path`; a missing file holds none. A file that cannot be read or
/// parsed, or holds another version, is refused and left in place, so the node does not
/// start over it; the refusal names the path and ends with the remedy. The list read is
/// held to what `CardRefusals` keeps, in memory only until the next write.
pub(crate) fn load_card_refusals(path: &Path) -> Result<CardRefusals> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CardRefusals::default());
        }
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
    Ok(CardRefusals::from_oldest_first(file.refused))
}

/// Writes `refused`, oldest first, to `path`, readable by its owner only since it names
/// the sessions that refused this user. A failed write is only warned about: the list
/// held in memory still stops the fetch for this run.
pub(crate) fn save_card_refusals(path: &Path, refused: &[String]) {
    let file = CardRefusalsFile {
        version: CARD_REFUSALS_VERSION,
        refused: refused.to_vec(),
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

    /// The card-refusal list says which sessions refused this one, so only its owner
    /// may read it, whatever the umask.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(umask)]
    fn the_card_refusal_list_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _umask = crate::testing::UmaskGuard::zero();
        let tmp = TempDir::new("card-refusals-owner-only");
        let path = path_in(&tmp);

        save_card_refusals(&path, &[D.to_string()]);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn a_missing_file_holds_no_refusals_and_creates_nothing() {
        let tmp = TempDir::new("card-refusals-missing");
        let path = path_in(&tmp);
        assert!(load_card_refusals(&path).unwrap().to_vec().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn refusals_round_trip() {
        let tmp = TempDir::new("card-refusals-round-trip");
        let path = path_in(&tmp);
        let refused = ["e".repeat(32), D.to_string()];
        save_card_refusals(&path, &refused);
        assert_eq!(load_card_refusals(&path).unwrap().to_vec(), refused);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
    }

    /// A list an earlier build wrote, its destinations sorted and each once, still loads
    /// as it stands; one naming a destination twice keeps it once, at its newest place,
    /// and one past the cap keeps the newest.
    #[test]
    fn a_list_on_disk_loads_held_to_the_cap() {
        let tmp = TempDir::new("card-refusals-existing");
        let path = path_in(&tmp);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let e = "e".repeat(32);
        fs::write(
            &path,
            format!("{{\n  \"version\": 1,\n  \"refused\": [\n    \"{D}\",\n    \"{e}\"\n  ]\n}}"),
        )
        .unwrap();
        assert_eq!(load_card_refusals(&path).unwrap().to_vec(), [D, e.as_str()]);

        fs::write(
            &path,
            format!("{{\"version\": 1, \"refused\": [\"{D}\", \"{e}\", \"{D}\"]}}"),
        )
        .unwrap();
        assert_eq!(load_card_refusals(&path).unwrap().to_vec(), [e.as_str(), D]);

        let many: Vec<String> = (0..=CARD_REFUSALS_MAX_ENTRIES)
            .map(|index| format!("{index:032x}"))
            .collect();
        save_card_refusals(&path, &many);
        assert_eq!(load_card_refusals(&path).unwrap().to_vec(), many[1..]);
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
