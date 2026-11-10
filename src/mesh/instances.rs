use crate::mesh::lock::acquire_mesh_lock;
use crate::mesh::peers::PEER_TTL;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_cause, version_refusal};
use crate::mesh::{mesh_cache_dir, redact_hashes, write_atomically_owner_only};
use crate::utils::fs::EXCLUSIVE_LOCK_WAIT;
use crate::utils::process::ProcessStamp;

use anyhow::{Context, Result, anyhow};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const INSTANCES_VERSION: u64 = 2;

/// The whole registry: the version first, then each node by its instance id.
/// Rejects unknown fields, so any change to the layout bumps `INSTANCES_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstancesFile {
    pub version: u64,
    pub instances: BTreeMap<String, InstanceRow>,
}

/// One node on this cache dir: the destination it serves, the session and agent it
/// serves them for, and the process that started it, by pid and start time so a pid the
/// kernel handed to another process does not keep the row alive. `gone_at` is set when
/// the node stops, or when a write first finds its process gone: the row then stays as a
/// tombstone, so the session can still be named as gone, until it is `PEER_TTL` old.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstanceRow {
    pub destination: String,
    pub session: String,
    pub agent: Option<String>,
    pub process: ProcessStamp,
    pub started: SystemTime,
    pub gone_at: Option<SystemTime>,
}

impl InstanceRow {
    /// The row this process files for a node it starts now.
    pub(crate) fn started_now(destination: String, session: String, agent: Option<String>) -> Self {
        Self {
            destination,
            session,
            agent,
            process: ProcessStamp::current(),
            started: SystemTime::now(),
            gone_at: None,
        }
    }
}

/// `<cache_dir>/mesh/instances.json`: the nodes on this cache dir, running or recently
/// gone, so a session can tell its own other sessions apart from foreign peers. It is
/// local only and a convenience: nothing in it reaches the mesh, and no read or write
/// failure stops a node from starting or stopping.
fn registry_path(cache_dir: &Path) -> PathBuf {
    mesh_cache_dir(cache_dir).join("instances.json")
}

/// Files `row` under `instance_id`, replacing any row the id had, a tombstone included.
pub(crate) fn register(cache_dir: &Path, instance_id: &str, row: InstanceRow) {
    update(cache_dir, |rows| {
        rows.insert(instance_id.to_string(), row);
    });
}

/// Marks the row of `instance_id` gone, keeping it as a tombstone; a row already gone
/// keeps the time it went.
pub(crate) fn unregister(cache_dir: &Path, instance_id: &str) {
    let now = SystemTime::now();
    update(cache_dir, |rows| {
        if let Some(row) = rows.get_mut(instance_id) {
            row.gone_at.get_or_insert(now);
        }
    });
}

/// The registry's rows by the destination each serves, split into the sessions still
/// running and the tombstones of those that have gone.
#[derive(Debug, Default)]
pub(crate) struct Sessions {
    pub running: HashMap<String, InstanceRow>,
    pub gone: HashMap<String, InstanceRow>,
}

/// The rows whose node still runs, by the destination each serves. A tombstone is never
/// among them, so no name resolves to a session that has gone.
pub(crate) fn live(cache_dir: &Path) -> HashMap<String, InstanceRow> {
    by_destination(cache_dir).running
}

/// Every row by the destination it serves, running or gone. A registry that cannot be
/// used reads as empty, with a warning, and is left for the next write to deal with; one
/// an older Coyote wrote is cache this build replaces, so it reads as empty with a debug
/// line only.
pub(crate) fn by_destination(cache_dir: &Path) -> Sessions {
    let path = registry_path(cache_dir);
    match read_judged_rows(&path, SystemTime::now()) {
        Ok(rows) => {
            let mut sessions = Sessions::default();
            for row in rows.into_values() {
                match row.gone_at {
                    None => {
                        sessions.running.insert(row.destination.clone(), row);
                    }
                    // Of two tombstones at one destination the one that went last names it.
                    Some(gone_at) => {
                        let newer = sessions
                            .gone
                            .get(&row.destination)
                            .is_none_or(|kept| kept.gone_at < Some(gone_at));
                        if newer {
                            sessions.gone.insert(row.destination.clone(), row);
                        }
                    }
                }
            }
            sessions
        }
        Err(RegistryRead::Older(found)) => {
            debug!("{}", older_registry(&path, found));
            Sessions::default()
        }
        Err(RegistryRead::Version(found)) => {
            warn!(
                "{} Until then `.mesh people list` and `mesh__peers` name none of your sessions.",
                refusal(&path, found)
            );
            Sessions::default()
        }
        Err(RegistryRead::Corrupt { cause, error }) => {
            warn!(
                "Mesh instance registry '{}' {} ({}). `.mesh people list` and `mesh__peers` name none of your sessions until the next `.mesh on` moves the file aside.",
                path.display(),
                redact_hashes(&cause),
                redact_hashes(&error)
            );
            Sessions::default()
        }
    }
}

/// What the registry says of the session that runs, or ran, an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstanceSession {
    /// Its node is still running, under this session name.
    Running(String),
    /// Its node stopped, or its process has gone, and its tombstone is not yet
    /// `PEER_TTL` old.
    Gone(String),
}

/// Every row the registry holds, running or a tombstone, by instance id. A registry that
/// cannot be read names none, logged at debug level; `by_destination` warns about it
/// where the sessions are listed.
pub(crate) fn sessions_by_instance(cache_dir: &Path) -> HashMap<String, InstanceSession> {
    let path = registry_path(cache_dir);
    let rows = match read_judged_rows(&path, SystemTime::now()) {
        Ok(rows) => rows,
        Err(RegistryRead::Older(found)) => {
            debug!("{}", older_registry(&path, found));
            return HashMap::new();
        }
        Err(RegistryRead::Version(found)) => {
            debug!("{}", refusal(&path, found));
            return HashMap::new();
        }
        Err(RegistryRead::Corrupt { cause, error }) => {
            debug!(
                "Mesh instance registry '{}' {} ({}), so no session is named",
                path.display(),
                redact_hashes(&cause),
                redact_hashes(&error)
            );
            return HashMap::new();
        }
    };
    rows.into_iter()
        .map(|(instance_id, row)| {
            let session = if row.gone_at.is_some() {
                InstanceSession::Gone(row.session)
            } else {
                InstanceSession::Running(row.session)
            };
            (instance_id, session)
        })
        .collect()
}

/// Applies `change` to the judged rows under the registry's lock and writes the result,
/// which files a tombstone for every row whose process has gone and drops every
/// tombstone `PEER_TTL` old. A failure is only warned about.
fn update(cache_dir: &Path, change: impl FnOnce(&mut BTreeMap<String, InstanceRow>)) {
    let path = registry_path(cache_dir);
    if let Err(err) = rewrite(&path, change) {
        warn!(
            "Mesh instance registry '{}' was not updated ({}). `.mesh people list` and `mesh__peers` may not name your sessions on this computer until each one runs `.mesh on` again; nothing else reads this file.",
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
    let mut rows = match read_judged_rows(path, SystemTime::now()) {
        Ok(rows) => rows,
        Err(RegistryRead::Older(found)) => {
            set_aside_older(path, found);
            BTreeMap::new()
        }
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
    /// A version below `INSTANCES_VERSION`: an earlier Coyote's registry, which the next
    /// write sets aside and starts afresh.
    Older(u64),
    /// A version above `INSTANCES_VERSION`: a newer Coyote's registry, never overwritten.
    Version(u64),
    /// `cause` says in plain words what is wrong with the file; `error` is the parser's
    /// own account.
    Corrupt { cause: String, error: String },
}

/// The rows at `path` as of `now`: a row whose process is gone reads as a tombstone gone
/// at `now`, judged on one process-table refresh over the rows not already gone, and a
/// tombstone `PEER_TTL` old is dropped; a missing file has none. A row stamped without a
/// start time stays running while its pid is in use, since it cannot be told from the
/// process that filed it.
fn read_judged_rows(
    path: &Path,
    now: SystemTime,
) -> Result<BTreeMap<String, InstanceRow>, RegistryRead> {
    let mut rows = read_rows(path)?;
    let stamps: Vec<ProcessStamp> = rows
        .values()
        .filter(|row| row.gone_at.is_none())
        .map(|row| row.process)
        .collect();
    if !stamps.is_empty() {
        let alive = ProcessStamp::alive_or_start_unknown_set(&stamps);
        for row in rows.values_mut() {
            if row.gone_at.is_none() && !alive.contains(&row.process) {
                row.gone_at = Some(now);
            }
        }
    }
    rows.retain(|_, row| {
        !row.gone_at
            .is_some_and(|gone_at| tombstone_expired(gone_at, now))
    });
    Ok(rows)
}

/// A `gone_at` in the future (the clock stepped back) reads as just gone, not as expired.
fn tombstone_expired(gone_at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(gone_at).unwrap_or_default() >= PEER_TTL
}

/// Every row at `path` as filed; a missing file has none.
fn read_rows(path: &Path) -> Result<BTreeMap<String, InstanceRow>, RegistryRead> {
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
    Ok(parse_instances(&bytes)?.instances)
}

/// Reads the version alone first so a file from another layout is refused by its version
/// rather than set aside as corrupt for whatever field that layout added or dropped.
fn parse_instances(bytes: &[u8]) -> Result<InstancesFile, RegistryRead> {
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|err| RegistryRead::Corrupt {
            cause: unversioned_cause(INSTANCES_VERSION),
            error: err.to_string(),
        })?;
    if probe.version < INSTANCES_VERSION {
        return Err(RegistryRead::Older(probe.version));
    }
    if probe.version != INSTANCES_VERSION {
        return Err(RegistryRead::Version(probe.version));
    }
    serde_json::from_slice(bytes).map_err(|err| RegistryRead::Corrupt {
        cause: format!("is not a version-{INSTANCES_VERSION} instance registry"),
        error: err.to_string(),
    })
}

/// Where a registry an older Coyote wrote is moved: `instances.json.v<found>`.
fn older_path(path: &Path, found: u64) -> PathBuf {
    path.with_extension(format!("json.v{found}"))
}

fn older_registry(path: &Path, found: u64) -> String {
    format!(
        "Mesh instance registry '{}' is version {found}, written by an earlier Coyote; this one writes version {INSTANCES_VERSION}, so it names no session until the next write (such as `.mesh on`) sets the file aside as '{}' and starts afresh",
        path.display(),
        older_path(path, found).display()
    )
}

/// Moves a registry an older Coyote wrote to `older_path` (replacing any earlier one), so
/// the write that found it starts a fresh one. The file is cache: sessions re-register as
/// each runs `.mesh on` under this build, so this is logged at debug level only.
fn set_aside_older(path: &Path, found: u64) {
    let aside = older_path(path, found);
    debug!(
        "Mesh instance registry '{}' is version {found}, written by an earlier Coyote; moving it to '{}' and starting afresh at version {INSTANCES_VERSION}",
        path.display(),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        debug!(
            "Mesh instance registry '{}' could not be moved to '{}' ({}), so it is replaced in place",
            path.display(),
            aside.display(),
            redact_hashes(&err.to_string())
        );
    }
}

/// Warns about a registry that cannot be used and renames it to `<path>.corrupt`
/// (replacing any earlier one), so the write that found it starts a fresh one.
fn set_aside_corrupt(path: &Path, cause: &str, error: &str) {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Mesh instance registry '{}' {} ({}). Starting it afresh, so `.mesh people list` and `mesh__peers` name your other running sessions only once each runs `.mesh on` again. Nothing else needs to be done; delete '{}' once you no longer need it.",
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
    fn unregister_keeps_the_row_as_a_tombstone_that_is_never_live() {
        let tmp = TempDir::new("instances-unregister");
        let filed = row(&"aa".repeat(16), "temp");
        register(&tmp.path, ID_A, filed.clone());
        let before = SystemTime::now();

        unregister(&tmp.path, ID_A);

        assert!(live(&tmp.path).is_empty());
        let tombstone = written(&tmp).instances[ID_A].clone();
        let gone_at = tombstone
            .gone_at
            .expect("the row is kept with its gone time");
        assert!(gone_at >= before, "{gone_at:?} before {before:?}");
        assert_eq!(
            InstanceRow {
                gone_at: None,
                ..tombstone
            },
            filed
        );
        let sessions = by_destination(&tmp.path);
        assert!(sessions.running.is_empty());
        assert_eq!(sessions.gone[&"aa".repeat(16)].session, "temp");
        assert_eq!(
            sessions_by_instance(&tmp.path).get(ID_A),
            Some(&InstanceSession::Gone("temp".to_string()))
        );

        unregister(&tmp.path, ID_A);
        assert_eq!(
            written(&tmp).instances[ID_A].gone_at,
            Some(gone_at),
            "a second stop keeps the time the row went"
        );
    }

    /// A node that joins again under the same instance id replaces its tombstone.
    #[test]
    fn registering_over_a_tombstone_makes_the_row_live_again() {
        let tmp = TempDir::new("instances-revive");
        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));
        unregister(&tmp.path, ID_A);

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        assert_eq!(live(&tmp.path)[&"aa".repeat(16)].session, "temp");
        assert!(by_destination(&tmp.path).gone.is_empty());
        assert_eq!(written(&tmp).instances[ID_A].gone_at, None);
    }

    /// A tombstone is pruned once it is `PEER_TTL` old: a read no longer names it, and the
    /// next write drops it. One just under the age, or stamped in the future, stays.
    #[test]
    fn a_tombstone_peer_ttl_old_is_pruned_on_read_and_dropped_by_the_next_write() {
        let tmp = TempDir::new("instances-tombstone-ttl");
        let now = SystemTime::now();
        let gone = |destination: &str, session: &str, gone_at| InstanceRow {
            gone_at: Some(gone_at),
            ..row(destination, session)
        };
        let expired_id = "11".repeat(16);
        let fresh_id = "22".repeat(16);
        let future_id = "33".repeat(16);
        let file = InstancesFile {
            version: INSTANCES_VERSION,
            instances: BTreeMap::from([
                (
                    expired_id.clone(),
                    gone(&"e1".repeat(16), "expired", now - PEER_TTL),
                ),
                (
                    fresh_id.clone(),
                    gone(
                        &"f1".repeat(16),
                        "fresh",
                        now - PEER_TTL + std::time::Duration::from_secs(60),
                    ),
                ),
                (
                    future_id.clone(),
                    gone(
                        &"f2".repeat(16),
                        "future",
                        now + std::time::Duration::from_secs(60),
                    ),
                ),
            ]),
        };
        write_raw(&tmp, &serde_json::to_vec(&file).unwrap());

        let sessions = sessions_by_instance(&tmp.path);
        assert_eq!(sessions.get(&expired_id), None, "{sessions:?}");
        assert_eq!(
            sessions.get(&fresh_id),
            Some(&InstanceSession::Gone("fresh".to_string()))
        );
        assert_eq!(
            sessions.get(&future_id),
            Some(&InstanceSession::Gone("future".to_string()))
        );
        assert!(
            !by_destination(&tmp.path)
                .gone
                .contains_key(&"e1".repeat(16))
        );

        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        let instances = written(&tmp).instances;
        assert!(
            !instances.contains_key(&expired_id),
            "{:?}",
            instances.keys()
        );
        assert!(instances.contains_key(&fresh_id));
        assert!(instances.contains_key(&future_id));
    }

    /// `.mesh off` in a session that keeps running: the process is alive, the row is not.
    #[test]
    fn a_tombstone_whose_process_still_runs_reads_gone() {
        let tmp = TempDir::new("instances-off-but-running");
        register(&tmp.path, ID_A, row(&"aa".repeat(16), "a"));
        register(&tmp.path, ID_B, row(&"bb".repeat(16), "b"));

        unregister(&tmp.path, ID_A);

        let sessions = by_destination(&tmp.path);
        assert_eq!(
            sessions.running.keys().collect::<Vec<_>>(),
            [&"bb".repeat(16)]
        );
        assert_eq!(sessions.gone.keys().collect::<Vec<_>>(), [&"aa".repeat(16)]);
        assert_eq!(
            sessions_by_instance(&tmp.path).get(ID_A),
            Some(&InstanceSession::Gone("a".to_string()))
        );
    }

    /// Two tombstones at one destination: the session that went last names it, whatever
    /// order their instance ids sort in.
    #[test]
    fn of_two_tombstones_at_one_destination_the_latest_names_it() {
        let tmp = TempDir::new("instances-two-tombstones");
        let now = SystemTime::now();
        let gone = |session: &str, ago: u64| InstanceRow {
            gone_at: Some(now - std::time::Duration::from_secs(ago)),
            ..row(&"aa".repeat(16), session)
        };
        let file = InstancesFile {
            version: INSTANCES_VERSION,
            instances: BTreeMap::from([
                (ID_A.to_string(), gone("later", 10)),
                (ID_B.to_string(), gone("earlier", 60)),
            ]),
        };
        write_raw(&tmp, &serde_json::to_vec(&file).unwrap());

        assert_eq!(
            by_destination(&tmp.path).gone[&"aa".repeat(16)].session,
            "later"
        );
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
    fn a_row_whose_process_is_gone_reads_gone_and_the_next_write_files_its_tombstone() {
        assert!(
            !ProcessStamp::pid_exists(DEAD_PID),
            "pid {DEAD_PID} is in use"
        );
        let tmp = TempDir::new("instances-dead-pid");
        let dead = InstanceRow {
            process: ProcessStamp {
                pid: DEAD_PID,
                start: 0,
            },
            ..row(&"dd".repeat(16), "gone")
        };
        let file = InstancesFile {
            version: INSTANCES_VERSION,
            instances: BTreeMap::from([(ID_B.to_string(), dead)]),
        };
        write_raw(&tmp, &serde_json::to_vec(&file).unwrap());

        assert!(live(&tmp.path).is_empty(), "a dead row is not live");
        assert_eq!(
            by_destination(&tmp.path).gone[&"dd".repeat(16)].session,
            "gone"
        );
        assert!(
            written(&tmp).instances[ID_B].gone_at.is_none(),
            "a read leaves the file alone"
        );

        let before = SystemTime::now();
        register(&tmp.path, ID_A, row(&"aa".repeat(16), "temp"));

        let instances = written(&tmp).instances;
        let gone_at = instances[ID_B]
            .gone_at
            .expect("the write files the dead row as a tombstone");
        assert!(gone_at >= before, "{gone_at:?} before {before:?}");
        assert!(instances.contains_key(ID_A));
    }

    #[test]
    fn sessions_by_instance_names_every_filed_row_and_whether_it_still_runs() {
        let tmp = TempDir::new("instances-sessions");
        assert!(sessions_by_instance(&tmp.path).is_empty());
        let dead = InstanceRow {
            process: ProcessStamp {
                pid: DEAD_PID,
                start: 0,
            },
            ..row(&"dd".repeat(16), "gone")
        };
        let file = InstancesFile {
            version: INSTANCES_VERSION,
            instances: BTreeMap::from([
                (ID_A.to_string(), row(&"aa".repeat(16), "a")),
                (ID_B.to_string(), dead),
            ]),
        };
        write_raw(&tmp, &serde_json::to_vec(&file).unwrap());

        let sessions = sessions_by_instance(&tmp.path);

        assert_eq!(
            sessions.get(ID_A),
            Some(&InstanceSession::Running("a".to_string()))
        );
        assert_eq!(
            sessions.get(ID_B),
            Some(&InstanceSession::Gone("gone".to_string()))
        );
        assert_eq!(sessions.len(), 2);
    }

    mod instances_stamp_tests {
        use super::*;
        use crate::utils::process::alive_set_calls;

        /// The pid is this test's own, so it is in use; only the start time says the
        /// row was filed by an earlier process that held it.
        #[test]
        fn a_live_pid_with_another_start_time_is_pruned() {
            let current = ProcessStamp::current();
            let tmp = TempDir::new("instances-recycled-pid");
            let recycled = InstanceRow {
                process: ProcessStamp {
                    pid: current.pid,
                    start: current.start + 1,
                },
                ..row(&"dd".repeat(16), "recycled")
            };
            let file = InstancesFile {
                version: INSTANCES_VERSION,
                instances: BTreeMap::from([(ID_B.to_string(), recycled)]),
            };
            write_raw(&tmp, &serde_json::to_vec(&file).unwrap());

            assert!(live(&tmp.path).is_empty());
        }

        #[test]
        fn one_read_looks_the_process_table_up_once() {
            let tmp = TempDir::new("instances-one-lookup");
            register(&tmp.path, ID_A, row(&"aa".repeat(16), "a"));
            register(&tmp.path, ID_B, row(&"bb".repeat(16), "b"));

            let before = alive_set_calls();
            let rows = live(&tmp.path);

            assert_eq!(alive_set_calls() - before, 1);
            assert_eq!(rows.len(), 2);
        }

        mod start_unknown_tests {
            use super::*;

            /// A row filed without a start time under a pid in use is kept, and each
            /// read still costs one process-table refresh.
            #[test]
            fn a_live_pid_stamped_start_zero_is_kept_on_one_refresh_per_read() {
                let tmp = TempDir::new("instances-start-unknown");
                let unknown = InstanceRow {
                    process: ProcessStamp {
                        pid: std::process::id(),
                        start: 0,
                    },
                    ..row(&"dd".repeat(16), "unknown")
                };
                let file = InstancesFile {
                    version: INSTANCES_VERSION,
                    instances: BTreeMap::from([(ID_B.to_string(), unknown)]),
                };
                write_raw(&tmp, &serde_json::to_vec(&file).unwrap());
                register(&tmp.path, ID_A, row(&"aa".repeat(16), "a"));

                for _ in 0..2 {
                    let before = alive_set_calls();
                    let rows = live(&tmp.path);
                    assert_eq!(alive_set_calls() - before, 1);
                    assert_eq!(rows.len(), 2, "{rows:?}");
                }
            }
        }
    }

    #[test]
    fn a_newer_version_is_refused_and_left_as_it_was() {
        install_log_collector();
        let tmp = TempDir::new("instances-version");
        let bytes = br#"{"version":3,"instances":{}}"#;
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
                && message.contains("is version 3 but this Coyote writes version 2")
                && message.contains("A newer Coyote wrote it")
                && message.contains("was not updated")),
            "no refusal names {path_text}; captured: {warns:#?}"
        );
    }

    /// A registry an earlier Coyote wrote (version 1, before tombstones) reads as empty
    /// with a debug line, not a warning, and the next write moves it to
    /// `instances.json.v1` and files the new row, so name addressing works at once.
    #[test]
    fn an_older_version_is_set_aside_with_a_debug_line_and_the_next_write_starts_afresh() {
        install_log_collector();
        let tmp = TempDir::new("instances-older");
        let bytes = br#"{"version":1,"instances":{"0123456789abcdef0123456789abcdef":{"destination":"aa","session":"old","agent":null,"process":{"pid":1,"start":0},"started":{"secs_since_epoch":0,"nanos_since_epoch":0}}}}"#;
        let path = write_raw(&tmp, bytes);
        let path_text = path.display().to_string();

        assert!(live(&tmp.path).is_empty());
        assert!(sessions_by_instance(&tmp.path).is_empty());
        assert!(path.exists(), "a read does not move the file");
        let debugs = crate::testing::debug_snapshot();
        assert!(
            debugs.iter().any(|message| message.contains(&path_text)
                && message.contains("names no session until the next write")),
            "a read logs why it names no session: {debugs:#?}"
        );
        let destination = "bb".repeat(16);
        register(&tmp.path, ID_A, row(&destination, "temp"));

        let aside = path.with_extension("json.v1");
        assert_eq!(
            fs::read(&aside).unwrap(),
            bytes,
            "the old file is kept aside"
        );
        assert!(!path.with_extension("json.corrupt").exists());
        let file = written(&tmp);
        assert_eq!(file.version, INSTANCES_VERSION);
        assert_eq!(file.instances.len(), 1, "only the new row is filed");
        assert!(live(&tmp.path).contains_key(&destination));
        assert!(
            !warn_snapshot()
                .iter()
                .any(|message| message.contains(&path_text)),
            "an older registry is cache replaced quietly"
        );
        let debugs = crate::testing::debug_snapshot();
        assert!(
            debugs.iter().any(|message| message.contains(&path_text)
                && message.contains("is version 1, written by an earlier Coyote")
                && message.contains("moving it to")),
            "no debug line names {path_text}; captured: {debugs:#?}"
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
