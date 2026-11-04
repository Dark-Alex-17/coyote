use crate::mesh::lock::acquire_mesh_lock;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_cause, version_refusal};
use crate::mesh::{mesh_cache_dir, redact_hashes, write_atomically_owner_only};
use crate::utils::fs::EXCLUSIVE_LOCK_WAIT;
use crate::utils::process::ProcessStamp;

use anyhow::{Context, Result, anyhow};
use log::warn;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const INSTANCES_VERSION: u64 = 1;

/// The whole registry: the version first, then each running node by its instance id.
/// Rejects unknown fields, so any change to the layout bumps `INSTANCES_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstancesFile {
    pub version: u64,
    pub instances: BTreeMap<String, InstanceRow>,
}

/// One node running on this cache dir: the destination it serves, the session and agent
/// it serves them for, and the process that started it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstanceRow {
    pub destination: String,
    pub session: String,
    pub agent: Option<String>,
    pub pid: u32,
    pub started: SystemTime,
}

impl InstanceRow {
    /// The row this process files for a node it starts now.
    pub(crate) fn started_now(destination: String, session: String, agent: Option<String>) -> Self {
        Self {
            destination,
            session,
            agent,
            pid: std::process::id(),
            started: SystemTime::now(),
        }
    }
}

/// `<cache_dir>/mesh/instances.json`: the nodes running on this cache dir, so a session
/// can tell its own other sessions apart from foreign peers. It is local only and a
/// convenience: nothing in it reaches the mesh, and no read or write failure stops a node
/// from starting or stopping.
fn registry_path(cache_dir: &Path) -> PathBuf {
    mesh_cache_dir(cache_dir).join("instances.json")
}

/// Files `row` under `instance_id`, replacing any row the id had.
pub(crate) fn register(cache_dir: &Path, instance_id: &str, row: InstanceRow) {
    update(cache_dir, |rows| {
        rows.insert(instance_id.to_string(), row);
    });
}

pub(crate) fn unregister(cache_dir: &Path, instance_id: &str) {
    update(cache_dir, |rows| {
        rows.remove(instance_id);
    });
}

/// The rows whose process is still running, by the destination each serves. A registry
/// that cannot be used reads as empty, with a warning, and is left for the next write to
/// deal with.
pub(crate) fn live(cache_dir: &Path) -> HashMap<String, InstanceRow> {
    let path = registry_path(cache_dir);
    match read_live_rows(&path) {
        Ok(rows) => rows
            .into_values()
            .map(|row| (row.destination.clone(), row))
            .collect(),
        Err(RegistryRead::Version(found)) => {
            warn!(
                "{} Until then `.mesh peers` and `mesh__peers` name none of your sessions.",
                refusal(&path, found)
            );
            HashMap::new()
        }
        Err(RegistryRead::Corrupt { cause, error }) => {
            warn!(
                "Mesh instance registry '{}' {} ({}). `.mesh peers` and `mesh__peers` name none of your sessions until the next `.mesh on` moves the file aside.",
                path.display(),
                redact_hashes(&cause),
                redact_hashes(&error)
            );
            HashMap::new()
        }
    }
}

/// Applies `change` to the live rows under the registry's lock and writes the result,
/// which drops every row whose process has gone. A failure is only warned about.
fn update(cache_dir: &Path, change: impl FnOnce(&mut BTreeMap<String, InstanceRow>)) {
    let path = registry_path(cache_dir);
    if let Err(err) = rewrite(&path, change) {
        warn!(
            "Mesh instance registry '{}' was not updated ({}). `.mesh peers` and `mesh__peers` may not name your sessions on this computer until each one runs `.mesh on` again; nothing else reads this file.",
            path.display(),
            redact_hashes(&format!("{err:#}"))
        );
    }
}

fn rewrite(path: &Path, change: impl FnOnce(&mut BTreeMap<String, InstanceRow>)) -> Result<()> {
    let _held = acquire_mesh_lock(
        &path.with_added_extension("lock"),
        "mesh instance registry",
        EXCLUSIVE_LOCK_WAIT,
    )?;
    let mut rows = match read_live_rows(path) {
        Ok(rows) => rows,
        Err(RegistryRead::Version(found)) => return Err(anyhow!(refusal(path, found))),
        Err(RegistryRead::Corrupt { cause, error }) => {
            set_aside_corrupt(path, &cause, &error);
            BTreeMap::new()
        }
    };
    change(&mut rows);
    let file = InstancesFile {
        version: INSTANCES_VERSION,
        instances: rows,
    };
    let json =
        serde_json::to_vec(&file).context("Failed to serialize the mesh instance registry")?;
    write_atomically_owner_only(path, &json)
}

fn refusal(path: &Path, found: u64) -> String {
    version_refusal(
        "instance registry",
        path,
        None,
        found,
        INSTANCES_VERSION,
        Remedy::Cache,
    )
}

enum RegistryRead {
    Version(u64),
    /// `cause` says in plain words what is wrong with the file; `error` is the parser's
    /// own account.
    Corrupt {
        cause: String,
        error: String,
    },
}

/// The rows at `path` whose pid is still in use; a missing file has none.
fn read_live_rows(path: &Path) -> Result<BTreeMap<String, InstanceRow>, RegistryRead> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(err) => {
            return Err(RegistryRead::Corrupt {
                cause: "could not be read".to_string(),
                error: err.to_string(),
            });
        }
    };
    let mut rows = parse_instances(&bytes)?.instances;
    rows.retain(|_, row| ProcessStamp::pid_exists(row.pid));
    Ok(rows)
}

/// Reads the version alone first so a file from another layout is refused by its version
/// rather than set aside as corrupt for whatever field that layout added or dropped.
fn parse_instances(bytes: &[u8]) -> Result<InstancesFile, RegistryRead> {
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|err| RegistryRead::Corrupt {
            cause: unversioned_cause(INSTANCES_VERSION),
            error: err.to_string(),
        })?;
    if probe.version != INSTANCES_VERSION {
        return Err(RegistryRead::Version(probe.version));
    }
    serde_json::from_slice(bytes).map_err(|err| RegistryRead::Corrupt {
        cause: format!("is not a version-{INSTANCES_VERSION} instance registry"),
        error: err.to_string(),
    })
}

/// Warns about a registry that cannot be used and renames it to `<path>.corrupt`
/// (replacing any earlier one), so the write that found it starts a fresh one.
fn set_aside_corrupt(path: &Path, cause: &str, error: &str) {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Mesh instance registry '{}' {} ({}). Starting it afresh, so `.mesh peers` and `mesh__peers` name your other running sessions only once each runs `.mesh on` again. Nothing else needs to be done; delete '{}' once you no longer need it.",
        path.display(),
        redact_hashes(cause),
        redact_hashes(error),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        warn!(
            "Mesh instance registry '{}' could not be moved to '{}' ({}), so it is replaced in place. Remove the file at '{}' if this warning repeats.",
            path.display(),
            aside.display(),
            redact_hashes(&err.to_string()),
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::testing::{install_log_collector, warn_snapshot};

    const ID_A: &str = "0123456789abcdef0123456789abcdef";
    const ID_B: &str = "fedcba9876543210fedcba9876543210";

    /// A pid no process can hold on the hosts the tests run on.
    const DEAD_PID: u32 = u32::MAX;

    fn row(destination: &str, session: &str) -> InstanceRow {
        InstanceRow::started_now(
            destination.to_string(),
            session.to_string(),
            Some("coder".to_string()),
        )
    }

    fn written(tmp: &TempDir) -> InstancesFile {
        serde_json::from_slice(&fs::read(registry_path(&tmp.path)).unwrap()).unwrap()
    }

    fn write_raw(tmp: &TempDir, bytes: &[u8]) -> PathBuf {
        let path = registry_path(&tmp.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_registry_is_instances_json_under_the_mesh_cache() {
        let tmp = TempDir::new("instances-path");
        assert_eq!(
            registry_path(&tmp.path),
            tmp.path.join("mesh").join("instances.json")
        );
    }

    #[test]
    fn a_registered_row_reads_back_by_its_destination() {
        let tmp = TempDir::new("instances-register");
        let filed = row(&"aa".repeat(16), "temp");

        register(&tmp.path, ID_A, filed.clone());

        assert_eq!(live(&tmp.path).get(&"aa".repeat(16)), Some(&filed));
        let file = written(&tmp);
        assert_eq!(file.version, INSTANCES_VERSION);
        assert_eq!(file.instances.get(ID_A), Some(&filed));
    }

    #[test]
    fn unregister_removes_the_row() {
        let tmp = TempDir::new("instances-unregister");
        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        unregister(&tmp.path, ID_A);

        assert!(live(&tmp.path).is_empty());
        assert!(written(&tmp).instances.is_empty());
    }

    #[test]
    fn two_instances_are_filed_side_by_side() {
        let tmp = TempDir::new("instances-two");

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "a"));
        register(&tmp.path, ID_B, row(&"bb".repeat(16), "temp"));

        let rows = live(&tmp.path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[&"aa".repeat(16)].session, "a");
        assert_eq!(rows[&"bb".repeat(16)].session, "temp");
        assert_eq!(written(&tmp).instances.len(), 2);
    }

    #[test]
    fn a_row_whose_process_is_gone_is_pruned_on_read_and_dropped_by_the_next_write() {
        assert!(
            !ProcessStamp::pid_exists(DEAD_PID),
            "pid {DEAD_PID} is in use"
        );
        let tmp = TempDir::new("instances-dead-pid");
        let dead = InstanceRow {
            pid: DEAD_PID,
            ..row(&"dd".repeat(16), "gone")
        };
        let file = InstancesFile {
            version: INSTANCES_VERSION,
            instances: BTreeMap::from([(ID_B.to_string(), dead)]),
        };
        write_raw(&tmp, &serde_json::to_vec(&file).unwrap());

        assert!(live(&tmp.path).is_empty(), "a dead row is not live");
        assert!(
            written(&tmp).instances.contains_key(ID_B),
            "a read leaves the file alone"
        );

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        let instances = written(&tmp).instances;
        assert!(!instances.contains_key(ID_B), "{:?}", instances.keys());
        assert!(instances.contains_key(ID_A));
    }

    #[test]
    fn another_version_is_refused_and_left_as_it_was() {
        install_log_collector();
        let tmp = TempDir::new("instances-version");
        let bytes = br#"{"version":2,"instances":{}}"#;
        let path = write_raw(&tmp, bytes);

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "a refused file is not overwritten"
        );
        assert!(!path.with_extension("json.corrupt").exists());
        assert!(live(&tmp.path).is_empty());
        let path_text = path.display().to_string();
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message.contains(&path_text)
                && message.contains("is version 2 but this Coyote writes version 1")
                && message.contains("was not updated")),
            "no refusal names {path_text}; captured: {warns:#?}"
        );
    }

    #[test]
    fn a_corrupt_registry_is_set_aside_and_started_afresh() {
        install_log_collector();
        let tmp = TempDir::new("instances-corrupt");
        let path = write_raw(&tmp, b"{not json");

        assert!(live(&tmp.path).is_empty());
        assert!(path.exists(), "a read does not move the file");

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        let aside = path.with_extension("json.corrupt");
        assert_eq!(fs::read(&aside).unwrap(), b"{not json");
        assert!(written(&tmp).instances.contains_key(ID_A));
        let path_text = path.display().to_string();
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message.contains(&path_text)
                && message.contains(&unversioned_cause(INSTANCES_VERSION))
                && message.contains("Starting it afresh")),
            "no warning names {path_text}; captured: {warns:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(umask)]
    fn the_registry_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _umask = crate::testing::UmaskGuard::zero();
        let tmp = TempDir::new("instances-owner-only");

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        let mode = fs::metadata(registry_path(&tmp.path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
