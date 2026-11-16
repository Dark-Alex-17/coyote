//! What the envoy remembers of a trusted peer's thread between messages: the peer's turns
//! and the envoy's answers, one record per (proved sender identity, sending destination,
//! thread), under `<cache_dir>/mesh/envoy-sessions/<instance_id>/`. The owner's own
//! transcript is never in here and neither is a system prompt; a record is only ever
//! handed back to the identity that wrote it, sending from the destination it wrote it
//! from: another instance of the same identity is another party to the conversation.
//! The store exists nowhere while `mesh.envoy_memory.enabled` is
//! false, not even as an empty directory, and every file and directory it does create is
//! readable by its owner alone. `index.json` is the enumeration: what it does not name
//! is not a conversation, and a record it names that is not on disk is forgotten on the
//! next look.

use crate::config::Session;
use crate::config::mesh_config::EnvoyMemoryConfig;
use crate::mesh::lock::{ExistingInstanceLock, InstanceLock};
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::trust::parse_hash;
use crate::mesh::{
    canonical_hash, mesh_cache_dir, parse_rfc3339, redact_hashes, rfc3339_utc, short,
    write_atomically_owner_only,
};

use log::warn;
use parking_lot::Mutex;
use rns_transport::hash::{AddressHash, Hash};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) const ENVOY_SESSION_VERSION: u64 = 1;
pub(crate) const ENVOY_SESSION_INDEX_VERSION: u64 = 1;

const STORE_NAME: &str = "envoy memory";
const INDEX_FILE: &str = "index.json";
const RECORD_EXTENSION: &str = "yaml";

/// How stale a store's last-used stamp may be before a load rewrites the index to bump
/// it; a save always does. Days-scale pruning needs no finer stamp, and a busy envoy
/// then rewrites the index for a read at most once an hour.
pub(crate) const STORE_STAMP_REFRESH: Duration = Duration::from_secs(3600);

/// The envoy memory as the trust list sees it: a revoked identity's conversations go
/// with its trust. Implemented by the node, which knows which instance's store is
/// current; a no-op while the memory is off.
pub(crate) trait EnvoyMemorySink: Send + Sync {
    /// Forgets every thread remembered of `identity` (lower-hex).
    fn forget_identity(&self, identity: &str);

    /// How many threads are remembered of `identity` (lower-hex) across every instance
    /// store, and how many stores could not be read to tell; reads only.
    fn remembered_of(&self, identity: &str) -> Result<RememberedOf, EnvoyMemoryError>;
}

/// What every instance store under a cache directory remembers, merged. A store whose
/// index cannot be read is counted rather than dropped: what it holds is unknown, and a
/// wipe that cannot reach it must say so.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Remembered {
    /// Each identity (lower-hex) to the threads remembered of it, sorted and each named
    /// once however many stores hold it.
    pub by_identity: BTreeMap<String, Vec<String>>,
    /// Record files no index names, across the stores that could be read: a save that
    /// wrote its record and never reached the index leaves one.
    pub orphans: usize,
    /// Stores whose index could not be read, so whose records are not in `by_identity`.
    pub unreadable_stores: usize,
}

impl Remembered {
    /// `true` when there is nothing on disk to forget and no store that might hold
    /// something.
    pub(crate) fn is_empty(&self) -> bool {
        self.by_identity.is_empty() && self.orphans == 0 && self.unreadable_stores == 0
    }
}

/// What a sweep removed: `removed` counts conversations in the stores that were read
/// and record files in the ones `delete_all` wiped unread; `unreadable_stores` the
/// stores the sweep could neither read nor write, left as they were.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Forgotten {
    pub removed: usize,
    pub unreadable_stores: usize,
}

/// What `prune_unused_everywhere` did: `removed` counts whole stores deleted, `stamped`
/// the stores seen with no last-used stamp and stamped now, `unreadable_stores` the
/// stores whose index could not be read, left as they were.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnusedStores {
    pub removed: usize,
    pub stamped: usize,
    pub unreadable_stores: usize,
}

/// What is remembered of one identity: `threads` counts its conversations in the stores
/// that were read; `unreadable_stores` the stores that could not say whether they hold more.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RememberedOf {
    pub threads: usize,
    pub unreadable_stores: usize,
}

/// The session `EnvoySessions::session_thread` chose for a message: the thread it is kept
/// under and, when it resumes one the store holds, that record as loaded. `resumed` is
/// `None` for a new session, kept under the message's own id.
#[derive(Debug)]
pub(crate) struct SessionThread {
    pub thread: String,
    pub resumed: Option<EnvoySessionFile>,
}

/// One `<key>.yaml`: the retained turns of one peer thread. The shape is a stable on-disk
/// record other code reads back; it rejects fields it does not know, so any change to the
/// layout bumps `ENVOY_SESSION_VERSION` and a reader refuses the file on a version it
/// does not write, except a field added with `#[serde(default)]`, which keeps the
/// version. `destination` is such a field: a record written before it existed loads with
/// it empty, which is no sender's destination, so it is never resumed; it is still
/// listed, forgotten, swept and aged out. There is no system slot by construction: what
/// the envoy is told about its owner is composed afresh on every run and never stored
/// beside a peer's words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvoySessionFile {
    pub version: u64,
    /// Lower-hex, the proved identity of the peer whose thread this is; a reader hands
    /// the record to no other.
    pub identity: String,
    /// Lower-hex, the proved destination the peer sent the thread from; a reader hands
    /// the record to no other destination of the same identity. Empty on a record
    /// written before the field existed.
    #[serde(default)]
    pub destination: String,
    pub thread: String,
    /// RFC 3339 UTC seconds, bumped on every save; what the age and recency bounds judge.
    pub last_used: String,
    pub turns: Vec<EnvoyTurn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvoyTurn {
    pub role: EnvoyRole,
    pub text: String,
}

/// Who spoke a turn: the peer (`User`) or this node's envoy (`Assistant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EnvoyRole {
    User,
    Assistant,
}

/// `index.json`: every conversation the store holds, the enumeration the bounds are
/// enforced over. Same refusal discipline as the records, under
/// `ENVOY_SESSION_INDEX_VERSION`; a refused index refuses the whole store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvoySessionIndex {
    pub version: u64,
    pub entries: Vec<EnvoySessionEntry>,
    /// RFC 3339 UTC seconds of the last time the envoy loaded or saved a thread here;
    /// what `prune_unused_everywhere` judges. Absent on an index written before the
    /// field existed, which keeps the version.
    #[serde(default)]
    pub store_last_used: Option<String>,
}

/// One conversation as the index knows it. Eviction judges the entry alone and never
/// opens the record; `turns` and `bytes` are what the record held when it was last written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvoySessionEntry {
    pub key: String,
    pub identity: String,
    /// As the record's: empty on an entry written before the field existed.
    #[serde(default)]
    pub destination: String,
    pub thread: String,
    pub last_used: String,
    pub turns: usize,
    /// UTF-8 bytes of turn text in the record.
    pub bytes: u64,
}

#[derive(Debug)]
pub(crate) enum EnvoyMemoryError {
    /// A record or the index carries a version this build does not write.
    Version {
        path: PathBuf,
        found: u64,
        expected: u64,
    },
    /// A record or the index has no readable `version`, so its shape is unknown.
    Unversioned {
        path: PathBuf,
        expected: u64,
    },
    /// The version is this build's but the document is not what the version promises.
    NotARecord {
        path: PathBuf,
        cause: String,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// The lock beside the index could not be opened or taken.
    Lock {
        path: PathBuf,
        source: io::Error,
    },
    /// The record under `key` names another identity than the one asking; it is not
    /// handed back.
    WrongIdentity {
        key: String,
        asked: String,
        found: String,
    },
    /// The record under `key` names another sending destination than the one asking;
    /// it is not handed back.
    WrongDestination {
        key: String,
        asked: String,
        found: String,
    },
    /// The identity given is not a 32-hex address hash, so it keys nothing.
    NotAnIdentity(String),
    /// The sending destination given is not a 32-hex address hash, so it keys nothing.
    NotADestination(String),
}

impl fmt::Display for EnvoyMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version {
                path,
                found,
                expected,
            } => f.write_str(&version_refusal(
                STORE_NAME,
                path,
                None,
                *found,
                *expected,
                Remedy::Cache,
            )),
            Self::Unversioned { path, expected } => f.write_str(&unversioned_refusal(
                STORE_NAME,
                path,
                None,
                *expected,
                Remedy::Cache,
            )),
            Self::NotARecord { path, cause } => write!(
                f,
                "Mesh {STORE_NAME} '{}' is not a record this Coyote reads: {cause}. {}",
                path.display(),
                Remedy::Cache.sentence()
            ),
            Self::Io { path, source } => write!(
                f,
                "Mesh {STORE_NAME} '{}' could not be read or written: {source}",
                path.display()
            ),
            Self::Lock { path, source } => write!(
                f,
                "Failed to lock mesh {STORE_NAME} lock '{}': {source}",
                path.display()
            ),
            Self::WrongIdentity { key, asked, found } => write!(
                f,
                "Mesh {STORE_NAME} record {key} belongs to identity {found}, not {asked}; refusing to hand it over"
            ),
            Self::WrongDestination { key, asked, found } => write!(
                f,
                "Mesh {STORE_NAME} record {key} belongs to destination {}, not {asked}; refusing to hand it over",
                if found.is_empty() { "(none)" } else { found }
            ),
            Self::NotAnIdentity(text) => write!(
                f,
                "'{text}' is not a 32-hex identity hash; the envoy memory is keyed by one"
            ),
            Self::NotADestination(text) => write!(
                f,
                "'{text}' is not a 32-hex destination hash; the envoy memory is keyed by one"
            ),
        }
    }
}

impl std::error::Error for EnvoyMemoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Lock { source, .. } => Some(source),
            Self::Version { .. }
            | Self::Unversioned { .. }
            | Self::NotARecord { .. }
            | Self::WrongIdentity { .. }
            | Self::WrongDestination { .. }
            | Self::NotAnIdentity(_)
            | Self::NotADestination(_) => None,
        }
    }
}

/// The file name of one session's record: the truncated SHA-256 of the identity's address
/// bytes, a zero byte, the sending destination's address bytes, a zero byte and the
/// session's thread, as 32 lowercase hex. The thread is the one
/// `EnvoySessions::session_thread` picks: the id of the message that opened the session,
/// never a thread a message named that the store did not hold. Both hashes are
/// fixed-width, so the zero bytes mark no boundary; they are fixed separators that keep
/// this digest's input distinct from `destination_address`'s, which also chains an
/// address hash. The address bytes, not their hex, so the key is the same whatever case
/// the caller spelt.
fn key_of(identity: &AddressHash, destination: &AddressHash, thread: &str) -> String {
    AddressHash::new_from_hash(&Hash::new(
        Hash::generator()
            .chain_update(identity.as_slice())
            .chain_update([0u8])
            .chain_update(destination.as_slice())
            .chain_update([0u8])
            .chain_update(thread.as_bytes())
            .finalize()
            .into(),
    ))
    .to_hex_string()
}

/// `key_of` for hex spellings, as the tests and the pinned vector name a record; `None`
/// when `identity` or `destination` is not a 32-hex address hash.
#[cfg(test)]
pub(crate) fn session_key(identity: &str, destination: &str, thread: &str) -> Option<String> {
    Some(key_of(
        &parse_hash(identity)?,
        &parse_hash(destination)?,
        thread,
    ))
}

pub(crate) fn stores_dir(cache_dir: &Path) -> PathBuf {
    mesh_cache_dir(cache_dir).join("envoy-sessions")
}

/// A sending destination for tests that file conversations without a live sender.
#[cfg(test)]
pub(crate) const TEST_DESTINATION: &str = "5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";

/// The conversations one instance's envoy remembers, keyed by instance because a fork
/// answers its own peers and must not read the original's.
pub(crate) struct EnvoySessions {
    dir: PathBuf,
    limits: EnvoyMemoryLimits,
    /// Orders every mutation through this handle; `file_lock` is the exclusion between
    /// handles and processes.
    write_lock: Mutex<()>,
    /// Whether `file_lock` creates the store directory. A store a sweep found on disk is
    /// never created again, so one deleted after it was listed is skipped, not revived.
    create_dir: bool,
}

#[derive(Debug, Clone, Copy)]
struct EnvoyMemoryLimits {
    max_sessions: usize,
    max_per_identity: usize,
    max_turns: usize,
    max_bytes: u64,
    ttl: Duration,
}

impl From<&EnvoyMemoryConfig> for EnvoyMemoryLimits {
    fn from(config: &EnvoyMemoryConfig) -> Self {
        // `MeshConfig::validate` is the gate for a store that is opened: a zero bound
        // never reaches one that saves or prunes.
        for value in [
            config.max_sessions,
            config.max_per_identity,
            config.max_turns,
            config.max_bytes,
            config.ttl_hours,
        ] {
            debug_assert!(
                value >= 1,
                "mesh.envoy_memory bounds are validated to 1 or more"
            );
        }
        Self::unchecked(config)
    }
}

impl EnvoyMemoryLimits {
    /// The config's values as they are. The sweep's stores never save or prune, so their
    /// limits are never read; they take the config of a node that is off, which
    /// `MeshConfig::validate` leaves unchecked.
    fn unchecked(config: &EnvoyMemoryConfig) -> Self {
        let count = |value: u64| usize::try_from(value).unwrap_or(usize::MAX);
        Self {
            max_sessions: count(config.max_sessions),
            max_per_identity: count(config.max_per_identity),
            max_turns: count(config.max_turns),
            max_bytes: config.max_bytes,
            ttl: Duration::from_secs(config.ttl_hours.saturating_mul(3600)),
        }
    }
}

impl EnvoySessions {
    /// `None` while `enabled` is false: a disabled store exists nowhere, not even as an
    /// empty directory. Enabled, this only names the directory; nothing is created until
    /// the first save.
    pub(crate) fn open(
        cache_dir: &Path,
        instance_id: &str,
        config: &EnvoyMemoryConfig,
    ) -> Option<Self> {
        config.enabled.then(|| Self {
            dir: stores_dir(cache_dir).join(instance_id),
            limits: EnvoyMemoryLimits::from(config),
            write_lock: Mutex::new(()),
            create_dir: true,
        })
    }

    /// Forgets every thread of `identity` in every instance's store under `cache_dir`,
    /// not only the one this node serves: trust is per config directory and the stores
    /// are per instance, so a revoked identity's conversations with a fork, or with an
    /// instance since re-keyed, go too. A store that cannot be read or written is logged
    /// and skipped and the sweep goes on; nothing is counted when no store exists. The
    /// sweep runs whether or not the memory is on: records may remain from a time it
    /// was, and a revocation must reach them before the memory comes back. A non-hash
    /// `identity` is `NotAnIdentity` whatever is on disk.
    pub(crate) fn delete_identity_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        identity: &str,
    ) -> Result<Forgotten, EnvoyMemoryError> {
        let identity = canonical_hash(identity)
            .ok_or_else(|| EnvoyMemoryError::NotAnIdentity(identity.to_string()))?;
        let mut removed = 0;
        let unreadable_stores = Self::sweep(cache_dir, config, "swept", |store| {
            removed += store.delete_identity(&identity)?;
            Ok(())
        })?;
        Ok(Forgotten {
            removed,
            unreadable_stores,
        })
    }

    /// Forgets `identity`'s `thread`, from whichever of its destinations it was held
    /// under, in every instance's store under `cache_dir`, with the same sweep as
    /// `delete_identity_everywhere`; `removed` is how many conversations went.
    pub(crate) fn delete_thread_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        identity: &str,
        thread: &str,
    ) -> Result<Forgotten, EnvoyMemoryError> {
        let identity = canonical_hash(identity)
            .ok_or_else(|| EnvoyMemoryError::NotAnIdentity(identity.to_string()))?;
        let mut removed = 0;
        let unreadable_stores = Self::sweep(cache_dir, config, "swept", |store| {
            removed += store.delete(&identity, thread)?;
            Ok(())
        })?;
        Ok(Forgotten {
            removed,
            unreadable_stores,
        })
    }

    /// Forgets every conversation in every instance's store under `cache_dir`, record
    /// files the index does not name included; a store whose index cannot be read is
    /// emptied unread, its record files counted. Same sweep as `delete_identity_everywhere`.
    pub(crate) fn delete_all_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
    ) -> Result<Forgotten, EnvoyMemoryError> {
        let mut removed = 0;
        let unreadable_stores = Self::sweep(cache_dir, config, "swept", |store| {
            removed += store.delete_all()?;
            Ok(())
        })?;
        Ok(Forgotten {
            removed,
            unreadable_stores,
        })
    }

    /// Every identity remembered in any instance's store under `cache_dir` with the
    /// threads remembered of it, each named once however many instance stores hold it
    /// and from however many of the identity's destinations. Same sweep as
    /// `delete_identity_everywhere`; empty when no store exists.
    pub(crate) fn remembered_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
    ) -> Result<Remembered, EnvoyMemoryError> {
        let mut by_identity: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut orphans = 0;
        let unreadable_stores = Self::sweep(cache_dir, config, "read", |store| {
            let (entries, unnamed) = store.remembered()?;
            orphans += unnamed;
            for entry in entries {
                by_identity
                    .entry(entry.identity)
                    .or_default()
                    .push(entry.thread);
            }
            Ok(())
        })?;
        for threads in by_identity.values_mut() {
            threads.sort();
            threads.dedup();
        }
        Ok(Remembered {
            by_identity,
            orphans,
            unreadable_stores,
        })
    }

    /// Deletes every instance's store under `cache_dir` whose envoy has neither loaded
    /// nor saved a thread for more than `days` days, by the store's last-used stamp and
    /// not by the age of what it holds. A store with no stamp yet is stamped `now`
    /// instead, so it is judged from first sight and never deleted on it; a stamp in the
    /// future counts as fresh. Stores named in `keep` (sessions that are running) are
    /// not touched, nor is one whose instance lock another session holds, as a session
    /// that is starting does before it registers; the lock is held while the store is
    /// judged, so no session starts on it mid-delete. A store whose index or stamp cannot
    /// be read is logged, counted and left. `days` of 0 does nothing: the config reaching
    /// here is not always validated. Same walk as `delete_identity_everywhere`, whether
    /// or not the memory is on. `stop` is asked before each store; once it says to, the
    /// walk ends and returns what it did so far, so a node leaving the mesh is not held
    /// up by the stores still to judge.
    pub(crate) fn prune_unused_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        days: u64,
        now: SystemTime,
        keep: &HashSet<String>,
        stop: &dyn Fn() -> bool,
    ) -> Result<UnusedStores, EnvoyMemoryError> {
        if days == 0 {
            return Ok(UnusedStores::default());
        }
        let unused_for = Duration::from_secs(days.saturating_mul(86_400));
        Self::judge_unused_everywhere(cache_dir, config, Some(unused_for), now, keep, stop)
    }

    /// `prune_unused_everywhere` that deletes nothing, only stamping the stores with no
    /// stamp yet: for a sweep that cannot tell which stores belong to running sessions.
    pub(crate) fn stamp_unstamped_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        now: SystemTime,
        keep: &HashSet<String>,
        stop: &dyn Fn() -> bool,
    ) -> Result<UnusedStores, EnvoyMemoryError> {
        Self::judge_unused_everywhere(cache_dir, config, None, now, keep, stop)
    }

    fn judge_unused_everywhere(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        unused_for: Option<Duration>,
        now: SystemTime,
        keep: &HashSet<String>,
        stop: &dyn Fn() -> bool,
    ) -> Result<UnusedStores, EnvoyMemoryError> {
        let mut removed = 0;
        let mut stamped = 0;
        let unreadable_stores = Self::sweep_until(cache_dir, config, "swept", stop, |store| {
            let name = store.dir.file_name().and_then(|name| name.to_str());
            if name.is_some_and(|name| keep.contains(name)) {
                return Ok(());
            }
            // Held through the removal so no session of this instance starts on the store
            // while it goes.
            let _instance_lock = match name.filter(|name| Session::is_valid_mesh_instance_id(name))
            {
                Some(name) => match InstanceLock::try_take_existing(cache_dir, name) {
                    Ok(ExistingInstanceLock::Held) => return Ok(()),
                    Ok(ExistingInstanceLock::Taken(lock)) => Some(lock),
                    Ok(ExistingInstanceLock::Absent) => None,
                    Err(source) => {
                        return Err(EnvoyMemoryError::Lock {
                            path: mesh_cache_dir(cache_dir).join(format!("{name}.lock")),
                            source,
                        });
                    }
                },
                None => None,
            };
            match store.remove_if_unused(unused_for, now)? {
                StoreUse::Removed => removed += 1,
                StoreUse::Stamped => stamped += 1,
                StoreUse::Used | StoreUse::Gone => {}
            }
            Ok(())
        })?;
        Ok(UnusedStores {
            removed,
            stamped,
            unreadable_stores,
        })
    }

    /// Hands every instance's store under `cache_dir` to `visit`, whether or not the
    /// memory is on: records may remain from a time it was. A store is a directory of its
    /// own, never one behind a symlink, that holds an index or at least one record file;
    /// any other entry is passed over uncounted. A store `visit` refuses is logged (`verb`
    /// being `swept` or `read`) and skipped; the count returned is how many were. One
    /// whose directory went while it was visited is passed over uncounted. `Err` only
    /// when the stores directory itself cannot be listed, and nothing is visited when it
    /// does not exist.
    fn sweep(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        verb: &'static str,
        visit: impl FnMut(&Self) -> Result<(), EnvoyMemoryError>,
    ) -> Result<usize, EnvoyMemoryError> {
        Self::sweep_until(cache_dir, config, verb, &|| false, visit)
    }

    /// `sweep` that asks `stop` before each store and visits none once it says to,
    /// returning what it counted so far.
    fn sweep_until(
        cache_dir: &Path,
        config: &EnvoyMemoryConfig,
        verb: &'static str,
        stop: &dyn Fn() -> bool,
        mut visit: impl FnMut(&Self) -> Result<(), EnvoyMemoryError>,
    ) -> Result<usize, EnvoyMemoryError> {
        let stores = stores_dir(cache_dir);
        if !stores.exists() {
            return Ok(0);
        }
        let io_error = |source| EnvoyMemoryError::Io {
            path: stores.clone(),
            source,
        };
        let mut skipped = 0;
        for entry in fs::read_dir(&stores).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            let is_dir = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
            if !is_dir || !looks_like_a_store(&path) {
                continue;
            }
            if stop() {
                break;
            }
            let store = Self::listed(path, config);
            if let Err(err) = visit(&store) {
                if !store.dir.exists() {
                    continue;
                }
                warn!(
                    "Mesh envoy memory under '{}' could not be {verb}: {}",
                    store.dir.display(),
                    redact_hashes(&err.to_string())
                );
                skipped += 1;
            }
        }
        Ok(skipped)
    }

    fn listed(dir: PathBuf, config: &EnvoyMemoryConfig) -> Self {
        Self {
            dir,
            limits: EnvoyMemoryLimits::unchecked(config),
            write_lock: Mutex::new(()),
            create_dir: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The record for `identity`'s `thread` sent from `destination`, or `None` when the
    /// store holds none. A record the index names but the disk lacks is dropped from the
    /// index on the way. A record under this key that names another identity or another
    /// destination is refused, never returned. One past `ttl_hours` since its last save
    /// is forgotten here and now rather than handed back; the rest of the store waits
    /// for `prune`. A store whose index exists has its last-used stamp bumped when the
    /// stamp is missing or `STORE_STAMP_REFRESH` old; a bump that cannot be written is
    /// warned about and the load goes on.
    pub(crate) fn load(
        &self,
        identity: &str,
        destination: &str,
        thread: &str,
        now: SystemTime,
    ) -> Result<Option<EnvoySessionFile>, EnvoyMemoryError> {
        let Resolved {
            identity,
            destination,
            key,
        } = resolve(identity, destination, thread)?;
        if !self.dir.exists() {
            return Ok(None);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = self.read_index()?;
        if self.index_path().exists() && stamp_is_stale(&index, now) {
            index.store_last_used = Some(rfc3339_utc(now));
            if let Err(err) = self.write_index(&index) {
                warn!(
                    "Mesh envoy memory under '{}' could not be stamped as used, so an unused-store sweep judges it by its older stamp: {}",
                    self.dir.display(),
                    redact_hashes(&err.to_string())
                );
            }
        }
        let Some(indexed) = self.owned(&index, &key, &identity, &destination)? else {
            return Ok(None);
        };
        if is_expired(
            &index.entries[indexed.position].last_used,
            now,
            self.limits.ttl,
        ) {
            index.entries.remove(indexed.position);
            self.remove_record(&key)?;
            self.write_index(&index)?;
            return Ok(None);
        }
        let Some(record) = indexed.record else {
            index.entries.remove(indexed.position);
            self.write_index(&index)?;
            return Ok(None);
        };
        Ok(Some(record))
    }

    /// The thread a message's session is kept under. A message resumes only the
    /// session `identity` already holds from `destination` under its `wire_thread`,
    /// live and its own; any other message, a root, one naming a thread the store does
    /// not hold, or one naming a thread another destination of the same identity holds,
    /// opens a new session under its own `message_id`. A record under the wire thread
    /// that cannot be read, or that names another identity or destination, is warned
    /// about and not resumed; so is a store whose index or lock cannot be used, with
    /// advice for the whole store rather than the thread. The record resumed comes back
    /// with the choice, so it is loaded once.
    pub(crate) fn session_thread(
        &self,
        identity: &str,
        destination: &str,
        wire_thread: Option<&str>,
        message_id: &str,
        now: SystemTime,
    ) -> SessionThread {
        let fresh = || SessionThread {
            thread: message_id.to_string(),
            resumed: None,
        };
        let Some(thread) = wire_thread.filter(|thread| *thread != message_id) else {
            return fresh();
        };
        match self.load(identity, destination, thread, now) {
            Ok(Some(record)) => SessionThread {
                thread: thread.to_string(),
                resumed: Some(record),
            },
            Ok(None) => fresh(),
            Err(EnvoyMemoryError::WrongIdentity { .. }) => {
                warn!(
                    "Mesh envoy memory for thread {thread} belongs to another identity than {}, so it was not resumed for this sender. A new conversation starts under message {message_id} and the other record is left alone; nothing needs to be done.",
                    short(identity)
                );
                fresh()
            }
            Err(EnvoyMemoryError::WrongDestination { .. }) => {
                warn!(
                    "Mesh envoy memory for thread {thread} belongs to another destination than {}, so it was not resumed for this sender. A new conversation starts under message {message_id} and the other record is left alone; nothing needs to be done.",
                    short(destination)
                );
                fresh()
            }
            Err(err) if self.is_record_error(&err, identity, destination, thread) => {
                warn!(
                    "Mesh envoy memory for thread {thread} could not be read ({}), so it was not resumed. A new conversation starts under message {message_id}; run `.mesh envoy memory forget {} {thread}` to clear the unreadable record.",
                    redact_hashes(&err.to_string()),
                    short(identity)
                );
                fresh()
            }
            Err(err) => {
                warn!(
                    "Mesh envoy memory could not be used ({}), so thread {thread} was not resumed. A new conversation starts under message {message_id}. Check that the cache directory is readable and writable and the disk has room; if the index cannot be read, run `.mesh envoy memory forget all` to empty the store.",
                    redact_hashes(&err.to_string())
                );
                fresh()
            }
        }
    }

    /// Whether `err` is about the one record `identity`'s `thread` from `destination`
    /// is kept in, rather than the index, the lock or the store as a whole.
    fn is_record_error(
        &self,
        err: &EnvoyMemoryError,
        identity: &str,
        destination: &str,
        thread: &str,
    ) -> bool {
        let Ok(Resolved { key, .. }) = resolve(identity, destination, thread) else {
            return false;
        };
        let record = self.record_path(&key);
        matches!(
            err,
            EnvoyMemoryError::Version { path, .. }
                | EnvoyMemoryError::Unversioned { path, .. }
                | EnvoyMemoryError::NotARecord { path, .. }
                | EnvoyMemoryError::Io { path, .. }
                if *path == record
        )
    }

    /// Writes `turns`, cut to `max_turns` and `max_bytes` oldest exchange first, as the
    /// whole of `identity`'s `thread` from `destination`, stamped `now`, creating the
    /// store on the first save; then drops every other conversation past `ttl_hours`
    /// and what the save pushed over `max_per_identity` or `max_sessions`, least
    /// recently used first and never the thread just saved. Turns that cut to nothing
    /// delete the record instead of writing an empty one. On either branch a record
    /// already under the key that this build cannot read, or that names another
    /// identity or destination, is refused: neither written over nor removed.
    pub(crate) fn save(
        &self,
        identity: &str,
        destination: &str,
        thread: &str,
        turns: Vec<EnvoyTurn>,
        now: SystemTime,
    ) -> Result<(), EnvoyMemoryError> {
        self.write(identity, destination, thread, turns, now, Write::Replace)
    }

    /// Adds `new_turns` after what the store holds of `identity`'s `thread` from
    /// `destination`, then bounds and evicts as `save` does. The read and the write are
    /// one locked step, so two writers landing together (a run's exchange and the
    /// owner's late answer) both stay. With `create`, a thread the store does not hold
    /// becomes a record of `new_turns` alone; without it nothing is written for one. A
    /// record under the key that this build cannot read, or that names another identity
    /// or destination, is refused. One past `ttl_hours` since its last save is
    /// forgotten first and counts as not held: a late turn never revives an expired
    /// thread.
    pub(crate) fn append(
        &self,
        identity: &str,
        destination: &str,
        thread: &str,
        new_turns: Vec<EnvoyTurn>,
        now: SystemTime,
        create: bool,
    ) -> Result<(), EnvoyMemoryError> {
        self.write(
            identity,
            destination,
            thread,
            new_turns,
            now,
            Write::Append {
                create,
                skip_if_last_equal: false,
            },
        )
    }

    /// `append` of one turn to a thread the store holds, written nothing when the
    /// held record already ends with that same turn: the one send retried by the owner
    /// is remembered once. Decided under the write lock, so a turn landing in between
    /// is seen.
    pub(crate) fn append_once(
        &self,
        identity: &str,
        destination: &str,
        thread: &str,
        turn: EnvoyTurn,
        now: SystemTime,
    ) -> Result<(), EnvoyMemoryError> {
        let mode = Write::Append {
            create: false,
            skip_if_last_equal: true,
        };
        self.write(identity, destination, thread, vec![turn], now, mode)
    }

    fn write(
        &self,
        identity: &str,
        destination: &str,
        thread: &str,
        mut turns: Vec<EnvoyTurn>,
        now: SystemTime,
        mode: Write,
    ) -> Result<(), EnvoyMemoryError> {
        let Resolved {
            identity,
            destination,
            key,
        } = resolve(identity, destination, thread)?;
        if !self.dir.exists() {
            // Nothing is under any key yet, so the turns given are the whole record;
            // one that comes to nothing creates no store.
            truncate(&mut turns, &self.limits);
            if turns.is_empty() || matches!(mode, Write::Append { create: false, .. }) {
                return Ok(());
            }
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = self.read_index()?;
        index.store_last_used = Some(rfc3339_utc(now));
        let indexed = match self.owned(&index, &key, &identity, &destination)? {
            Some(indexed)
                if matches!(mode, Write::Append { .. })
                    && is_expired(
                        &index.entries[indexed.position].last_used,
                        now,
                        self.limits.ttl,
                    ) =>
            {
                index.entries.remove(indexed.position);
                self.remove_record(&key)?;
                self.write_index(&index)?;
                None
            }
            indexed => indexed,
        };
        let position = indexed.as_ref().map(|indexed| indexed.position);
        let mut turns = match mode {
            Write::Replace => turns,
            Write::Append {
                create,
                skip_if_last_equal,
            } => match indexed.and_then(|indexed| indexed.record) {
                Some(record) => {
                    if skip_if_last_equal && record.turns.ends_with(&turns) {
                        return Ok(());
                    }
                    let mut held = record.turns;
                    held.extend(turns);
                    held
                }
                None if create => turns,
                None => return Ok(()),
            },
        };
        truncate(&mut turns, &self.limits);
        if turns.is_empty() {
            if let Some(position) = position {
                index.entries.remove(position);
                self.remove_record(&key)?;
                self.write_index(&index)?;
            }
            return Ok(());
        }
        let record = EnvoySessionFile {
            version: ENVOY_SESSION_VERSION,
            identity,
            destination,
            thread: thread.to_string(),
            last_used: rfc3339_utc(now),
            turns,
        };
        self.write_record(&key, &record)?;
        let entry = EnvoySessionEntry {
            key: key.clone(),
            identity: record.identity,
            destination: record.destination,
            thread: record.thread,
            last_used: record.last_used,
            turns: record.turns.len(),
            bytes: text_bytes(&record.turns),
        };
        match position {
            Some(position) => index.entries[position] = entry,
            None => index.entries.push(entry),
        }
        for evicted in evict(&mut index, &self.limits, now, Some(&key)) {
            self.remove_record(&evicted.key)?;
        }
        self.write_index(&index)
    }

    /// Forgets `identity`'s `thread` from every destination it was held under, records
    /// written before the destination was kept included, and says how many went. Each
    /// record is removed by the key its index entry names, without reading it: the
    /// caller asked for it gone, whatever it holds.
    pub(crate) fn delete(&self, identity: &str, thread: &str) -> Result<usize, EnvoyMemoryError> {
        let identity = canonical_hash(identity)
            .ok_or_else(|| EnvoyMemoryError::NotAnIdentity(identity.to_string()))?;
        self.delete_where(|entry| entry.identity == identity && entry.thread == thread)
    }

    /// Forgets every thread of `identity` from every destination, each record removed by
    /// the key its index entry names, without reading it.
    pub(crate) fn delete_identity(&self, identity: &str) -> Result<usize, EnvoyMemoryError> {
        let identity = canonical_hash(identity)
            .ok_or_else(|| EnvoyMemoryError::NotAnIdentity(identity.to_string()))?;
        self.delete_where(|entry| entry.identity == identity)
    }

    /// Forgets every conversation past `ttl_hours`, then the least recently used past
    /// `max_per_identity` and `max_sessions`, and removes record files the index does
    /// not name, each by key without reading it: the bound requires it gone, whatever
    /// it holds. Creates nothing while the store does not exist; the index is rewritten
    /// only when a conversation went.
    pub(crate) fn prune(&self, now: SystemTime) -> Result<usize, EnvoyMemoryError> {
        if !self.dir.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = self.read_index()?;
        let evicted = evict(&mut index, &self.limits, now, None);
        for entry in &evicted {
            self.remove_record(&entry.key)?;
        }
        self.remove_orphans(&index)?;
        if !evicted.is_empty() {
            self.write_index(&index)?;
        }
        Ok(evicted.len())
    }

    /// Under both locks: stamps a store that has no last-used stamp, refuses one whose
    /// stamp does not parse, removes one whose stamp is more than `unused_for` before
    /// `now` (every record file and the index, then the lock file and the directory if
    /// nothing else is left in it), and leaves any other as it is. `unused_for` of `None`
    /// removes nothing. A directory left holding no index and no record, as a concurrent
    /// sweep leaves one it removed, is `Gone`, and its lock file and directory are removed
    /// the same way.
    fn remove_if_unused(
        &self,
        unused_for: Option<Duration>,
        now: SystemTime,
    ) -> Result<StoreUse, EnvoyMemoryError> {
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        if !looks_like_a_store(&self.dir) {
            self.remove_lock_file_and_dir();
            return Ok(StoreUse::Gone);
        }
        let mut index = self.read_index()?;
        let Some(stamp) = index.store_last_used.as_deref() else {
            index.store_last_used = Some(rfc3339_utc(now));
            self.write_index(&index)?;
            return Ok(StoreUse::Stamped);
        };
        let last_used = parse_rfc3339(stamp).ok_or_else(|| EnvoyMemoryError::NotARecord {
            path: self.index_path(),
            cause: format!("its store_last_used '{stamp}' is not an RFC 3339 time"),
        })?;
        let unused = unused_for
            .is_some_and(|limit| now.duration_since(last_used).unwrap_or_default() > limit);
        if !unused {
            return Ok(StoreUse::Used);
        }
        // Fails on any record or the index it cannot remove, so `Removed` means both went.
        self.wipe_unread()?;
        self.remove_lock_file_and_dir();
        Ok(StoreUse::Removed)
    }

    /// Called holding the file lock on a store with no index and no record left.
    fn remove_lock_file_and_dir(&self) {
        // Best effort. A handle that opened the lock file before this unlink can still lock
        // the unlinked file once this one drops; it then finds no index and no record and
        // leaves the directory alone, and no session of this instance can start meanwhile
        // because the sweep holds its instance lock. Where an open file cannot be removed
        // the lock file and the directory stay behind.
        let _ = fs::remove_file(self.index_path().with_added_extension("lock"));
        let _ = fs::remove_dir(&self.dir);
    }

    /// `(conversations, distinct identities)`; `(0, 0)` while the store does not exist.
    pub(crate) fn stats(&self) -> Result<(usize, usize), EnvoyMemoryError> {
        if !self.dir.exists() {
            return Ok((0, 0));
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let index = self.read_index()?;
        let identities: HashSet<&str> = index
            .entries
            .iter()
            .map(|entry| entry.identity.as_str())
            .collect();
        Ok((index.entries.len(), identities.len()))
    }

    /// Every conversation the index names, as it names them, and how many record files
    /// it does not name; nothing while the store does not exist.
    fn remembered(&self) -> Result<(Vec<EnvoySessionEntry>, usize), EnvoyMemoryError> {
        if !self.dir.exists() {
            return Ok((Vec::new(), 0));
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let index = self.read_index()?;
        let orphans = self.orphan_records(&index)?.len();
        Ok((index.entries, orphans))
    }

    fn delete_where(
        &self,
        doomed: impl Fn(&EnvoySessionEntry) -> bool,
    ) -> Result<usize, EnvoyMemoryError> {
        if !self.dir.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = self.read_index()?;
        let removed: Vec<EnvoySessionEntry> = index
            .entries
            .extract_if(.., |entry| doomed(entry))
            .collect();
        for entry in &removed {
            self.remove_record(&entry.key)?;
        }
        if !removed.is_empty() {
            self.write_index(&index)?;
        }
        Ok(removed.len())
    }

    /// Drops every conversation the index names and every record file it does not, so a
    /// record left by a write that did not reach the index goes too. An index that cannot
    /// be read cannot say what the store holds, and the caller asked for all of it gone:
    /// every record file and the index itself are removed unread, the record files counted.
    fn delete_all(&self) -> Result<usize, EnvoyMemoryError> {
        if !self.dir.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = match self.read_index() {
            Ok(index) => index,
            Err(err) => {
                warn!(
                    "Mesh envoy memory under '{}' is being emptied unread; its index could not be read: {}",
                    self.dir.display(),
                    redact_hashes(&err.to_string())
                );
                return self.wipe_unread();
            }
        };
        let removed = mem::take(&mut index.entries);
        for entry in &removed {
            self.remove_record(&entry.key)?;
        }
        self.remove_orphans(&index)?;
        if !removed.is_empty() {
            self.write_index(&index)?;
        }
        Ok(removed.len())
    }

    /// Called under both locks by `delete_all` and `remove_if_unused`; removes every
    /// record file and the index without reading either, and counts the record files
    /// that went.
    fn wipe_unread(&self) -> Result<usize, EnvoyMemoryError> {
        let records = self.record_files()?;
        for path in &records {
            fs::remove_file(path).map_err(|source| EnvoyMemoryError::Io {
                path: path.clone(),
                source,
            })?;
        }
        let index = self.index_path();
        match fs::remove_file(&index) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(EnvoyMemoryError::Io {
                    path: index,
                    source,
                });
            }
        }
        Ok(records.len())
    }

    /// `key`'s entry and record as far as `identity` sending from `destination` may see
    /// them: `None` when the index does not name the key, the record `None` when the
    /// index names it but the disk lacks it. An entry or record under the key that names
    /// another identity or another destination is refused, never read and never written
    /// over.
    fn owned(
        &self,
        index: &EnvoySessionIndex,
        key: &str,
        identity: &str,
        destination: &str,
    ) -> Result<Option<Indexed>, EnvoyMemoryError> {
        let Some(position) = index.entries.iter().position(|entry| entry.key == key) else {
            return Ok(None);
        };
        let wrong_identity = |found: &str| EnvoyMemoryError::WrongIdentity {
            key: key.to_string(),
            asked: identity.to_string(),
            found: found.to_string(),
        };
        let wrong_destination = |found: &str| EnvoyMemoryError::WrongDestination {
            key: key.to_string(),
            asked: destination.to_string(),
            found: found.to_string(),
        };
        let entry = &index.entries[position];
        if entry.identity != identity {
            return Err(wrong_identity(&entry.identity));
        }
        if entry.destination != destination {
            return Err(wrong_destination(&entry.destination));
        }
        let record = self.read_record(key)?;
        if let Some(record) = &record {
            if record.identity != identity {
                return Err(wrong_identity(&record.identity));
            }
            if record.destination != destination {
                return Err(wrong_destination(&record.destination));
            }
        }
        Ok(Some(Indexed { position, record }))
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    fn record_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.{RECORD_EXTENSION}"))
    }

    /// An exclusive lock on `index.json.lock`, held until the returned `File` drops.
    /// Every Coyote process of one identity shares the cache directory, and the index
    /// is renamed over on every write, so the lock lives on a sibling that is never
    /// replaced. The store directory is created here, owner-only, which is why nothing
    /// takes the lock for a store that does not exist yet; a store a sweep listed is not
    /// created, so the lock fails once its directory has gone.
    fn file_lock(&self) -> Result<File, EnvoyMemoryError> {
        if self.create_dir {
            create_dirs_owner_only(&self.dir).map_err(|source| EnvoyMemoryError::Io {
                path: self.dir.clone(),
                source,
            })?;
        }
        let path = self.index_path().with_added_extension("lock");
        let mut open = OpenOptions::new();
        open.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let file = open.open(&path).map_err(|source| EnvoyMemoryError::Lock {
            path: path.clone(),
            source,
        })?;
        file.lock()
            .map_err(|source| EnvoyMemoryError::Lock { path, source })?;
        Ok(file)
    }

    fn read_index(&self) -> Result<EnvoySessionIndex, EnvoyMemoryError> {
        Ok(read_versioned(
            &self.index_path(),
            Format::Json,
            ENVOY_SESSION_INDEX_VERSION,
        )?
        .unwrap_or(EnvoySessionIndex {
            version: ENVOY_SESSION_INDEX_VERSION,
            entries: Vec::new(),
            store_last_used: None,
        }))
    }

    fn write_index(&self, index: &EnvoySessionIndex) -> Result<(), EnvoyMemoryError> {
        let path = self.index_path();
        let text = serde_json::to_string(index).map_err(|err| EnvoyMemoryError::Io {
            path: path.clone(),
            source: io::Error::other(err),
        })?;
        write_owner_only(&path, text.as_bytes())
    }

    fn read_record(&self, key: &str) -> Result<Option<EnvoySessionFile>, EnvoyMemoryError> {
        read_versioned(&self.record_path(key), Format::Yaml, ENVOY_SESSION_VERSION)
    }

    fn write_record(&self, key: &str, record: &EnvoySessionFile) -> Result<(), EnvoyMemoryError> {
        let path = self.record_path(key);
        let text = serde_yaml::to_string(record).map_err(|err| EnvoyMemoryError::Io {
            path: path.clone(),
            source: io::Error::other(err),
        })?;
        write_owner_only(&path, text.as_bytes())
    }

    fn remove_record(&self, key: &str) -> Result<bool, EnvoyMemoryError> {
        let path = self.record_path(key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(EnvoyMemoryError::Io { path, source }),
        }
    }

    /// Removes every record file `index` does not name: a record the index does not
    /// know is not a conversation, whatever it holds.
    fn remove_orphans(&self, index: &EnvoySessionIndex) -> Result<(), EnvoyMemoryError> {
        for path in self.orphan_records(index)? {
            fs::remove_file(&path).map_err(|source| EnvoyMemoryError::Io { path, source })?;
        }
        Ok(())
    }

    fn orphan_records(&self, index: &EnvoySessionIndex) -> Result<Vec<PathBuf>, EnvoyMemoryError> {
        let named: HashSet<&str> = index
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect();
        let mut records = self.record_files()?;
        records.retain(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| !named.contains(stem))
        });
        Ok(records)
    }

    /// Every `<key>.yaml` whose stem is a 32-lowercase-hex key; nothing else in the
    /// directory is a record, whatever its extension.
    fn record_files(&self) -> Result<Vec<PathBuf>, EnvoyMemoryError> {
        let io_error = |source| EnvoyMemoryError::Io {
            path: self.dir.clone(),
            source,
        };
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            if is_record_file(&path) {
                records.push(path);
            }
        }
        Ok(records)
    }
}

fn is_record_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == RECORD_EXTENSION)
        && path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| canonical_hash(stem).is_some_and(|key| key == stem))
}

/// `true` when `dir` holds an index or at least one record file, or cannot be listed to
/// tell; a directory that holds neither was never a store and is nobody's to sweep.
fn looks_like_a_store(dir: &Path) -> bool {
    if dir.join(INDEX_FILE).exists() {
        return true;
    }
    match fs::read_dir(dir) {
        Ok(entries) => entries.flatten().any(|entry| is_record_file(&entry.path())),
        Err(_) => true,
    }
}

struct Indexed {
    position: usize,
    record: Option<EnvoySessionFile>,
}

enum StoreUse {
    Used,
    Stamped,
    Removed,
    Gone,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    /// The turns given are the whole record.
    Replace,
    /// The turns given follow the ones held; `create` says whether a thread not held
    /// becomes a record of them alone, `skip_if_last_equal` whether a record that
    /// already ends with them is left as it is.
    Append {
        create: bool,
        skip_if_last_equal: bool,
    },
}

/// The canonical identity and destination a caller named, and the key they file under.
struct Resolved {
    identity: String,
    destination: String,
    key: String,
}

fn resolve(identity: &str, destination: &str, thread: &str) -> Result<Resolved, EnvoyMemoryError> {
    let identity_hash = parse_hash(identity)
        .ok_or_else(|| EnvoyMemoryError::NotAnIdentity(identity.to_string()))?;
    let destination_hash = parse_hash(destination)
        .ok_or_else(|| EnvoyMemoryError::NotADestination(destination.to_string()))?;
    Ok(Resolved {
        key: key_of(&identity_hash, &destination_hash, thread),
        identity: identity_hash.to_hex_string(),
        destination: destination_hash.to_hex_string(),
    })
}

fn text_bytes(turns: &[EnvoyTurn]) -> u64 {
    turns.iter().map(|turn| turn.text.len() as u64).sum()
}

/// Whether `stamp` is `ttl` or more before `now`, or does not parse at all. A stamp in
/// the future (clock stepped back) reads as just made.
fn is_expired(stamp: &str, now: SystemTime, ttl: Duration) -> bool {
    parse_rfc3339(stamp).is_none_or(|at| now.duration_since(at).unwrap_or_default() >= ttl)
}

/// Whether a load should rewrite `index` to bump its last-used stamp: it has none, or
/// one that does not parse, or one `STORE_STAMP_REFRESH` or more before `now`.
fn stamp_is_stale(index: &EnvoySessionIndex, now: SystemTime) -> bool {
    index
        .store_last_used
        .as_deref()
        .is_none_or(|stamp| is_expired(stamp, now, STORE_STAMP_REFRESH))
}

/// Drops whole exchanges from the front of `turns`, oldest first, until it fits
/// `max_turns` and `max_bytes`. An exchange runs from one turn up to the next the peer
/// spoke, so an answer is never kept without what it answered: `[User, Assistant,
/// Assistant]` goes as one, `[User, User, Assistant]` as `[User]` then `[User,
/// Assistant]`, and a leading run of answers goes with the first exchange. An exchange
/// larger than `max_bytes` on its own leaves nothing.
fn truncate(turns: &mut Vec<EnvoyTurn>, limits: &EnvoyMemoryLimits) {
    while turns.len() > limits.max_turns || text_bytes(turns) > limits.max_bytes {
        let next_exchange = turns
            .iter()
            .skip(1)
            .position(|turn| turn.role == EnvoyRole::User)
            .map_or(turns.len(), |offset| offset + 1);
        turns.drain(..next_exchange);
    }
}

/// Removes from `index` what the bounds no longer allow and returns it: first every
/// entry past `ttl`, then, oldest `last_used` first, the ones past `max_per_identity`
/// for their identity and the ones past `max_sessions` overall. `keep` is never
/// evicted, so a save always leaves the thread it saved. The entries left are ordered
/// newest first.
fn evict(
    index: &mut EnvoySessionIndex,
    limits: &EnvoyMemoryLimits,
    now: SystemTime,
    keep: Option<&str>,
) -> Vec<EnvoySessionEntry> {
    let kept = |entry: &EnvoySessionEntry| Some(entry.key.as_str()) == keep;
    let mut removed: Vec<EnvoySessionEntry> = index
        .entries
        .extract_if(.., |entry| {
            !kept(entry) && is_expired(&entry.last_used, now, limits.ttl)
        })
        .collect();
    index
        .entries
        .sort_by_key(|entry| parse_rfc3339(&entry.last_used));
    let mut per_identity: HashMap<&str, usize> = HashMap::new();
    for entry in &index.entries {
        *per_identity.entry(entry.identity.as_str()).or_default() += 1;
    }
    let mut over_identity: HashMap<String, usize> = per_identity
        .into_iter()
        .filter(|(_, count)| *count > limits.max_per_identity)
        .map(|(identity, count)| (identity.to_string(), count - limits.max_per_identity))
        .collect();
    removed.extend(index.entries.extract_if(.., |entry| {
        if kept(entry) {
            return false;
        }
        match over_identity.get_mut(&entry.identity) {
            Some(over) if *over > 0 => {
                *over -= 1;
                true
            }
            _ => false,
        }
    }));
    let mut over_all = index.entries.len().saturating_sub(limits.max_sessions);
    removed.extend(index.entries.extract_if(.., |entry| {
        if over_all == 0 || kept(entry) {
            return false;
        }
        over_all -= 1;
        true
    }));
    index.entries.reverse();
    removed
}

/// `write_atomically_owner_only` with its whole cause chain kept, since the typed error
/// shows one line.
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), EnvoyMemoryError> {
    write_atomically_owner_only(path, bytes).map_err(|err| EnvoyMemoryError::Io {
        path: path.to_path_buf(),
        source: io::Error::other(format!("{err:#}")),
    })
}

/// `create_dir_all` that creates every directory it has to create owner-only, in the
/// mkdir itself rather than by a chmod afterwards; a directory that already existed is
/// not touched.
fn create_dirs_owner_only(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[derive(Clone, Copy)]
enum Format {
    Yaml,
    Json,
}

impl Format {
    fn parse<T: DeserializeOwned>(self, text: &str) -> Result<T, String> {
        match self {
            Self::Yaml => serde_yaml::from_str(text).map_err(|err| err.to_string()),
            Self::Json => serde_json::from_str(text).map_err(|err| err.to_string()),
        }
    }
}

/// One whole-file document of `expected` version, or `None` when the file is not there.
/// The version is read on its own first so a file from a newer Coyote is named as such
/// rather than failing on whatever field the newer layout added; the document is then
/// read whole and refused on any field this build does not know. Nothing is moved aside
/// or migrated: a refused file stays where it is.
fn read_versioned<T: DeserializeOwned>(
    path: &Path,
    format: Format,
    expected: u64,
) -> Result<Option<T>, EnvoyMemoryError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(EnvoyMemoryError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let probe: VersionProbe = format
        .parse(&text)
        .map_err(|_| EnvoyMemoryError::Unversioned {
            path: path.to_path_buf(),
            expected,
        })?;
    if probe.version != expected {
        return Err(EnvoyMemoryError::Version {
            path: path.to_path_buf(),
            found: probe.version,
            expected,
        });
    }
    format
        .parse(&text)
        .map(Some)
        .map_err(|cause| EnvoyMemoryError::NotARecord {
            path: path.to_path_buf(),
            cause,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_support::TempDir;
    use crate::testing::{install_log_collector, warn_snapshot};
    use std::time::Duration;

    const IDENTITY_A: &str = "0123456789abcdef0123456789abcdef";
    const IDENTITY_B: &str = "fedcba9876543210fedcba9876543210";
    /// The destination each identity sends from in these tests.
    const DEST_A: &str = "00112233445566778899aabbccddeeff";
    const DEST_B: &str = "ffeeddccbbaa99887766554433221100";
    /// A second instance of `IDENTITY_A`.
    const DEST_A2: &str = "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf";

    fn dest(identity: &str) -> &'static str {
        if identity.eq_ignore_ascii_case(IDENTITY_B) {
            DEST_B
        } else {
            DEST_A
        }
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn enabled() -> EnvoyMemoryConfig {
        EnvoyMemoryConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn store(tmp: &TempDir, config: &EnvoyMemoryConfig) -> EnvoySessions {
        EnvoySessions::open(&tmp.path, "inst", config).expect("an enabled store opens")
    }

    fn user(text: &str) -> EnvoyTurn {
        EnvoyTurn {
            role: EnvoyRole::User,
            text: text.to_string(),
        }
    }

    fn assistant(text: &str) -> EnvoyTurn {
        EnvoyTurn {
            role: EnvoyRole::Assistant,
            text: text.to_string(),
        }
    }

    fn exchange(n: usize) -> Vec<EnvoyTurn> {
        vec![
            user(&format!("question {n}")),
            assistant(&format!("answer {n}")),
        ]
    }

    fn record_path(store: &EnvoySessions, identity: &str, thread: &str) -> PathBuf {
        store.record_path(&session_key(identity, dest(identity), thread).unwrap())
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_record_round_trips_through_yaml_with_no_system_slot() {
        let record = EnvoySessionFile {
            version: ENVOY_SESSION_VERSION,
            identity: IDENTITY_A.to_string(),
            destination: DEST_A.to_string(),
            thread: "thread-one".to_string(),
            last_used: rfc3339_utc(t(1_000)),
            turns: vec![user("hello"), assistant("hi")],
        };

        let yaml = serde_yaml::to_string(&record).unwrap();
        let back: EnvoySessionFile = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(back, record);
        let keys: Vec<String> = serde_yaml::from_str::<serde_yaml::Mapping>(&yaml)
            .unwrap()
            .keys()
            .map(|key| key.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            keys,
            [
                "version",
                "identity",
                "destination",
                "thread",
                "last_used",
                "turns"
            ]
        );
        assert!(!yaml.contains("system"), "{yaml}");
        assert!(yaml.contains("role: user\n"), "{yaml}");
        assert!(yaml.contains("role: assistant\n"), "{yaml}");
    }

    #[test]
    fn two_identities_with_the_same_thread_get_different_keys() {
        let a = session_key(IDENTITY_A, DEST_A, "thread-one").unwrap();
        let b = session_key(IDENTITY_B, DEST_A, "thread-one").unwrap();
        assert_ne!(a, b);
        assert_ne!(
            a,
            session_key(IDENTITY_A, DEST_A2, "thread-one").unwrap(),
            "another instance of the same identity keys another record"
        );
        assert_ne!(a, session_key(IDENTITY_A, DEST_A, "thread-two").unwrap());
        assert_eq!(
            a,
            session_key(&IDENTITY_A.to_ascii_uppercase(), DEST_A, "thread-one").unwrap(),
            "the key digests the identity's bytes, not its spelling"
        );
        assert_eq!(
            a,
            session_key(IDENTITY_A, &DEST_A.to_ascii_uppercase(), "thread-one").unwrap(),
            "the key digests the destination's bytes, not its spelling"
        );
        assert_eq!(session_key("not-a-hash", DEST_A, "thread-one"), None);
        assert_eq!(session_key(IDENTITY_A, "not-a-hash", "thread-one"), None);
    }

    #[test]
    fn a_disabled_store_opens_to_none_and_creates_nothing() {
        let tmp = TempDir::new("envoy-sessions-disabled");

        assert!(EnvoySessions::open(&tmp.path, "inst", &EnvoyMemoryConfig::default()).is_none());

        assert!(!tmp.path.join("mesh").exists());
    }

    #[test]
    fn an_enabled_store_creates_nothing_until_the_first_save() {
        let tmp = TempDir::new("envoy-sessions-lazy");
        let store = store(&tmp, &enabled());

        assert_eq!(store.prune(t(1_000)).unwrap(), 0);
        assert_eq!(store.stats().unwrap(), (0, 0));
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
                .unwrap(),
            None
        );
        assert_eq!(store.delete(IDENTITY_A, "thread-one").unwrap(), 0);
        assert_eq!(store.delete_identity(IDENTITY_A).unwrap(), 0);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", Vec::new(), t(1_000))
            .unwrap();

        assert!(!tmp.path.join("mesh").exists(), "{:?}", names_in(&tmp.path));
    }

    #[test]
    fn save_writes_under_envoy_sessions_instance_dir_through_the_index_lock() {
        let tmp = TempDir::new("envoy-sessions-layout");
        let store = store(&tmp, &enabled());

        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();

        let dir = tmp.path.join("mesh").join("envoy-sessions").join("inst");
        assert_eq!(store.dir(), dir);
        let key = session_key(IDENTITY_A, DEST_A, "thread-one").unwrap();
        assert_eq!(
            names_in(&dir),
            [
                format!("{key}.yaml"),
                "index.json".to_string(),
                "index.json.lock".to_string()
            ],
            "the record, the index and its lock, and no temp file"
        );
        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.identity, IDENTITY_A);
        assert_eq!(loaded.destination, DEST_A);
        assert_eq!(loaded.thread, "thread-one");
        assert_eq!(loaded.last_used, rfc3339_utc(t(1_000)));
        assert_eq!(loaded.turns, exchange(1));
        let index: EnvoySessionIndex =
            serde_json::from_str(&fs::read_to_string(dir.join("index.json")).unwrap()).unwrap();
        assert_eq!(index.version, ENVOY_SESSION_INDEX_VERSION);
        assert_eq!(
            index.entries,
            vec![EnvoySessionEntry {
                key,
                identity: IDENTITY_A.to_string(),
                destination: DEST_A.to_string(),
                thread: "thread-one".to_string(),
                last_used: rfc3339_utc(t(1_000)),
                turns: 2,
                bytes: 18,
            }]
        );
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(umask)]
    fn records_the_index_and_every_directory_are_owner_only() {
        let _umask = crate::testing::UmaskGuard::zero();
        let tmp = TempDir::new("envoy-sessions-owner-only");
        let store = store(&tmp, &enabled());

        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();

        let mesh = tmp.path.join("mesh");
        for dir in [
            &mesh,
            &mesh.join("envoy-sessions"),
            &mesh.join("envoy-sessions").join("inst"),
        ] {
            assert_eq!(mode_of(dir), 0o700, "{}", dir.display());
        }
        for file in names_in(store.dir()) {
            let path = store.dir().join(file);
            assert_eq!(mode_of(&path), 0o600, "{}", path.display());
        }
    }

    #[test]
    fn a_record_whose_identity_differs_from_the_asked_identity_is_never_loaded() {
        let tmp = TempDir::new("envoy-sessions-wrong-identity");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let key = session_key(IDENTITY_A, DEST_A, "thread-one").unwrap();
        let path = store.record_path(&key);
        let planted = fs::read_to_string(&path)
            .unwrap()
            .replace(IDENTITY_A, IDENTITY_B);
        fs::write(&path, planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(&err, EnvoyMemoryError::WrongIdentity { key: k, asked, found }
                if *k == key && asked == IDENTITY_A && found == IDENTITY_B),
            "{err:?}"
        );
        assert!(err.to_string().contains("refusing"), "{err}");
        assert_eq!(
            store
                .load(IDENTITY_B, DEST_B, "thread-one", t(1_000))
                .unwrap(),
            None,
            "B's key differs, so B sees nothing either"
        );

        let index_path = store.index_path();
        let swapped = fs::read_to_string(&index_path)
            .unwrap()
            .replace(IDENTITY_A, IDENTITY_B);
        fs::write(&index_path, swapped).unwrap();
        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::WrongIdentity { .. }),
            "an index entry under A's key naming B is refused too: {err:?}"
        );
    }

    /// Replaces `from` with `to` in the file at `path`.
    fn plant(path: &Path, from: &str, to: &str) {
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains(from), "{text}");
        fs::write(path, text.replace(from, to)).unwrap();
    }

    #[test]
    fn a_record_whose_destination_differs_from_the_asked_destination_is_never_loaded() {
        let tmp = TempDir::new("envoy-sessions-wrong-destination");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let key = session_key(IDENTITY_A, DEST_A, "thread-one").unwrap();
        let path = store.record_path(&key);
        plant(&path, DEST_A, DEST_A2);

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(&err, EnvoyMemoryError::WrongDestination { key: k, asked, found }
                if *k == key && asked == DEST_A && found == DEST_A2),
            "{err:?}"
        );
        assert!(err.to_string().contains("refusing"), "{err}");
        let planted = fs::read_to_string(&path).unwrap();
        assert!(
            matches!(
                store.append(
                    IDENTITY_A,
                    DEST_A,
                    "thread-one",
                    exchange(2),
                    t(1_001),
                    true
                ),
                Err(EnvoyMemoryError::WrongDestination { .. })
            ),
            "a record naming another destination is not written over"
        );
        assert!(matches!(
            store.save(IDENTITY_A, DEST_A, "thread-one", exchange(2), t(1_001)),
            Err(EnvoyMemoryError::WrongDestination { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), planted);
        install_log_collector();
        let warned_before = warn_snapshot().len();
        let session = store.session_thread(IDENTITY_A, DEST_A, Some("thread-one"), "m-2", t(1_001));
        assert_eq!(session.thread, "m-2");
        assert!(session.resumed.is_none());
        let warned: Vec<String> = warn_snapshot()
            .into_iter()
            .skip(warned_before)
            .filter(|line| line.contains("belongs to another destination"))
            .collect();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains(short(DEST_A)), "{}", warned[0]);
        assert!(
            !warned[0].contains(DEST_A) && !warned[0].contains(DEST_A2),
            "no full hash is logged: {}",
            warned[0]
        );

        plant(&path, DEST_A2, DEST_A);
        plant(&store.index_path(), DEST_A, DEST_A2);
        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::WrongDestination { .. }),
            "an index entry under the key naming another destination is refused too: {err:?}"
        );
    }

    #[test]
    fn another_destination_of_the_same_identity_does_not_see_the_thread() {
        let tmp = TempDir::new("envoy-sessions-other-destination");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();

        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A2, "thread-one", t(1_001))
                .unwrap(),
            None
        );
        let session =
            store.session_thread(IDENTITY_A, DEST_A2, Some("thread-one"), "m-2", t(1_001));
        assert_eq!(
            session.thread, "m-2",
            "a new session under the message's id"
        );
        assert!(session.resumed.is_none());
        let own = store.session_thread(IDENTITY_A, DEST_A, Some("thread-one"), "m-3", t(1_001));
        assert_eq!(own.thread, "thread-one");
        assert_eq!(own.resumed.unwrap().turns, exchange(1));

        store
            .append(
                IDENTITY_A,
                DEST_A2,
                "thread-one",
                exchange(2),
                t(1_002),
                true,
            )
            .unwrap();
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_002))
                .unwrap()
                .unwrap()
                .turns,
            exchange(1),
            "each destination's thread is its own record"
        );
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A2, "thread-one", t(1_002))
                .unwrap()
                .unwrap()
                .turns,
            exchange(2)
        );
        assert_eq!(store.stats().unwrap(), (2, 1));
    }

    #[test]
    fn forgetting_a_thread_or_an_identity_reaches_every_destination() {
        let tmp = TempDir::new("envoy-sessions-forget-every-destination");
        let config = enabled();
        let store = store(&tmp, &config);
        for (destination, thread) in [
            (DEST_A, "thread-one"),
            (DEST_A2, "thread-one"),
            (DEST_A, "thread-two"),
            (DEST_A2, "thread-three"),
        ] {
            store
                .save(IDENTITY_A, destination, thread, exchange(1), t(1_000))
                .unwrap();
        }
        store
            .save(IDENTITY_B, DEST_B, "thread-one", exchange(1), t(1_000))
            .unwrap();

        assert_eq!(store.delete(IDENTITY_A, "thread-one").unwrap(), 2);
        assert_eq!(store.stats().unwrap(), (3, 2));
        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, IDENTITY_A).unwrap(),
            Forgotten {
                removed: 2,
                unreadable_stores: 0,
            }
        );
        assert_eq!(threads_of(&store, IDENTITY_A), Vec::<String>::new());
        assert_eq!(threads_of(&store, IDENTITY_B), ["thread-one"]);

        for destination in [DEST_A, DEST_A2] {
            store
                .save(IDENTITY_A, destination, "thread-one", exchange(1), t(1_000))
                .unwrap();
        }
        assert_eq!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &config, IDENTITY_A, "thread-one")
                .unwrap(),
            Forgotten {
                removed: 2,
                unreadable_stores: 0,
            }
        );
        assert_eq!(
            names_in(store.dir()),
            [
                format!(
                    "{}.yaml",
                    session_key(IDENTITY_B, DEST_B, "thread-one").unwrap()
                ),
                "index.json".to_string(),
                "index.json.lock".to_string()
            ],
            "B's record, the index and its lock"
        );
    }

    /// The file name a record of `identity`'s `thread` had before the sending destination
    /// was part of the key.
    fn legacy_key(identity: &str, thread: &str) -> String {
        AddressHash::new_from_hash(&Hash::new(
            Hash::generator()
                .chain_update(parse_hash(identity).unwrap().as_slice())
                .chain_update([0u8])
                .chain_update(thread.as_bytes())
                .finalize()
                .into(),
        ))
        .to_hex_string()
    }

    /// Writes a record of `identity`'s `thread` and its index entry as a build that did
    /// not keep the destination wrote them: under the old key and with no `destination`
    /// field. The store is created first by a save of `IDENTITY_B`'s `other`.
    fn plant_legacy(store: &EnvoySessions, identity: &str, thread: &str, at: SystemTime) {
        store
            .save(IDENTITY_B, DEST_B, "other", exchange(9), at)
            .unwrap();
        let key = legacy_key(identity, thread);
        let last_used = rfc3339_utc(at);
        let record = format!(
            "version: {ENVOY_SESSION_VERSION}\nidentity: {identity}\nthread: {thread}\nlast_used: {last_used}\nturns:\n- role: user\n  text: old question\n- role: assistant\n  text: old answer\n"
        );
        fs::write(store.record_path(&key), record).unwrap();
        let mut index: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.index_path()).unwrap()).unwrap();
        index["entries"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "key": key,
                "identity": identity,
                "thread": thread,
                "last_used": last_used,
                "turns": 2,
                "bytes": 22,
            }));
        fs::write(store.index_path(), index.to_string()).unwrap();
    }

    #[test]
    fn a_record_written_before_the_destination_was_kept_never_resumes() {
        let tmp = TempDir::new("envoy-sessions-legacy-no-resume");
        let config = enabled();
        let store = store(&tmp, &config);
        plant_legacy(&store, IDENTITY_A, "thread-one", t(1_000));

        let index = store.read_index().unwrap();
        let legacy = index
            .entries
            .iter()
            .find(|entry| entry.identity == IDENTITY_A)
            .unwrap();
        assert_eq!(legacy.destination, "", "the field defaults to empty");
        assert_eq!(
            store.read_record(&legacy.key).unwrap().unwrap().destination,
            ""
        );
        for destination in [DEST_A, DEST_A2] {
            assert_eq!(
                store
                    .load(IDENTITY_A, destination, "thread-one", t(1_001))
                    .unwrap(),
                None
            );
            let session =
                store.session_thread(IDENTITY_A, destination, Some("thread-one"), "m-2", t(1_001));
            assert_eq!(session.thread, "m-2");
            assert!(session.resumed.is_none());
        }
        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                exchange(1),
                t(1_002),
                true,
            )
            .unwrap();
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_002))
                .unwrap()
                .unwrap()
                .turns,
            exchange(1),
            "the old turns are never carried into the new record"
        );
        assert!(
            store
                .record_path(&legacy_key(IDENTITY_A, "thread-one"))
                .exists()
        );
    }

    #[test]
    fn a_record_written_before_the_destination_was_kept_is_listed_forgotten_swept_and_aged_out() {
        let tmp = TempDir::new("envoy-sessions-legacy-forget");
        let config = enabled();
        let store = store(&tmp, &config);
        let legacy = store.record_path(&legacy_key(IDENTITY_A, "thread-one"));

        plant_legacy(&store, IDENTITY_A, "thread-one", t(1_000));
        let remembered = EnvoySessions::remembered_everywhere(&tmp.path, &config).unwrap();
        assert_eq!(remembered.by_identity[IDENTITY_A], ["thread-one"]);
        assert_eq!(
            remembered.orphans, 0,
            "the old record is named by the index"
        );
        assert_eq!(store.stats().unwrap(), (2, 2));
        assert_eq!(store.delete(IDENTITY_A, "thread-one").unwrap(), 1);
        assert!(!legacy.exists(), "removed by the key its entry names");

        plant_legacy(&store, IDENTITY_A, "thread-one", t(1_000));
        assert_eq!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &config, IDENTITY_A, "thread-one")
                .unwrap()
                .removed,
            1
        );
        assert!(!legacy.exists());

        plant_legacy(&store, IDENTITY_A, "thread-one", t(1_000));
        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, IDENTITY_A)
                .unwrap()
                .removed,
            1
        );
        assert!(!legacy.exists());
        assert_eq!(threads_of(&store, IDENTITY_B), ["other"]);

        plant_legacy(&store, IDENTITY_A, "thread-one", t(1_000));
        let ttl = Duration::from_secs(config.ttl_hours * 3600);
        assert_eq!(
            store.prune(t(1_000) + ttl).unwrap(),
            2,
            "the old record ages out with the rest"
        );
        assert!(!legacy.exists());
    }

    #[test]
    fn delete_removes_one_thread() {
        let tmp = TempDir::new("envoy-sessions-delete");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(2), t(1_001))
            .unwrap();

        assert_eq!(store.delete(IDENTITY_A, "thread-one").unwrap(), 1);
        assert_eq!(store.delete(IDENTITY_A, "thread-one").unwrap(), 0);

        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_001))
                .unwrap(),
            None
        );
        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert!(
            store
                .load(IDENTITY_A, DEST_A, "thread-two", t(1_001))
                .unwrap()
                .is_some()
        );
        assert_eq!(store.stats().unwrap(), (1, 1));
    }

    #[test]
    fn delete_identity_removes_every_thread_of_that_identity_and_nothing_else() {
        let tmp = TempDir::new("envoy-sessions-delete-identity");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(2), t(1_001))
            .unwrap();
        store
            .save(IDENTITY_B, DEST_B, "thread-one", exchange(3), t(1_002))
            .unwrap();

        assert_eq!(store.delete_identity(IDENTITY_A).unwrap(), 2);
        assert_eq!(store.delete_identity(IDENTITY_A).unwrap(), 0);

        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert!(!record_path(&store, IDENTITY_A, "thread-two").exists());
        assert_eq!(
            store
                .load(IDENTITY_B, DEST_B, "thread-one", t(1_002))
                .unwrap()
                .unwrap()
                .turns,
            exchange(3)
        );
        assert_eq!(store.stats().unwrap(), (1, 1));
    }

    #[test]
    fn delete_identity_everywhere_sweeps_every_instance_store_and_skips_one_that_refuses() {
        let tmp = TempDir::new("envoy-sessions-delete-everywhere");
        let config = enabled();
        let forgot = |removed, unreadable_stores| Forgotten {
            removed,
            unreadable_stores,
        };
        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, IDENTITY_A).unwrap(),
            forgot(0, 0)
        );
        assert!(matches!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, "not-a-hash"),
            Err(EnvoyMemoryError::NotAnIdentity(_))
        ));
        assert!(!tmp.path.join("mesh").exists());
        let stores: Vec<EnvoySessions> = ["inst-a", "inst-b", "inst-c"]
            .into_iter()
            .map(|instance| EnvoySessions::open(&tmp.path, instance, &config).unwrap())
            .collect();
        for (n, store) in stores.iter().enumerate() {
            store
                .save(IDENTITY_A, DEST_A, "thread-one", exchange(n), t(1_000))
                .unwrap();
            store
                .save(IDENTITY_B, DEST_B, "thread-one", exchange(n), t(1_000))
                .unwrap();
        }
        stores[0]
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(9), t(1_001))
            .unwrap();
        let refused_index = stores[2].index_path();
        let planted = fs::read_to_string(&refused_index)
            .unwrap()
            .replace("\"version\":1", "\"version\":2");
        fs::write(&refused_index, &planted).unwrap();
        fs::write(stores_dir(&tmp.path).join("notes.txt"), "not a store").unwrap();

        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, IDENTITY_A).unwrap(),
            forgot(3, 1),
            "the two stores that could be read gave up three; the refusing one is counted"
        );

        for store in &stores[..2] {
            assert_eq!(
                store
                    .load(IDENTITY_A, DEST_A, "thread-one", t(1_001))
                    .unwrap(),
                None
            );
            assert!(
                store
                    .load(IDENTITY_B, DEST_B, "thread-one", t(1_001))
                    .unwrap()
                    .is_some()
            );
        }
        assert!(!record_path(&stores[0], IDENTITY_A, "thread-two").exists());
        assert_eq!(
            fs::read_to_string(&refused_index).unwrap(),
            planted,
            "the store that refuses is left as it is"
        );
        assert!(record_path(&stores[2], IDENTITY_A, "thread-one").exists());
        assert!(matches!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, "not-a-hash"),
            Err(EnvoyMemoryError::NotAnIdentity(_))
        ));
    }

    #[test]
    fn delete_identity_everywhere_sweeps_while_the_memory_is_off() {
        let tmp = TempDir::new("envoy-sessions-delete-while-off");
        let on = store(&tmp, &enabled());
        on.save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        on.save(IDENTITY_B, DEST_B, "thread-one", exchange(2), t(1_000))
            .unwrap();

        let off = EnvoyMemoryConfig::default();
        assert!(!off.enabled);
        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &off, IDENTITY_A).unwrap(),
            Forgotten {
                removed: 1,
                unreadable_stores: 0
            }
        );

        let reopened = store(&tmp, &enabled());
        assert_eq!(
            reopened
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_001))
                .unwrap(),
            None
        );
        assert_eq!(
            reopened
                .load(IDENTITY_B, DEST_B, "thread-one", t(1_001))
                .unwrap()
                .map(|record| record.turns),
            Some(exchange(2))
        );
    }

    /// Three instance stores: A has `thread-one` in every one and `thread-two` in the
    /// first, B has `thread-one` in every one; the third store's index is planted with
    /// a version this build refuses.
    fn three_instances(tmp: &TempDir, config: &EnvoyMemoryConfig) -> Vec<EnvoySessions> {
        let stores: Vec<EnvoySessions> = ["inst-a", "inst-b", "inst-c"]
            .into_iter()
            .map(|instance| EnvoySessions::open(&tmp.path, instance, config).unwrap())
            .collect();
        for (n, store) in stores.iter().enumerate() {
            store
                .save(IDENTITY_A, DEST_A, "thread-one", exchange(n), t(1_000))
                .unwrap();
            store
                .save(IDENTITY_B, DEST_B, "thread-one", exchange(n), t(1_000))
                .unwrap();
        }
        stores[0]
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(9), t(1_001))
            .unwrap();
        let refused_index = stores[2].index_path();
        let planted = fs::read_to_string(&refused_index)
            .unwrap()
            .replace("\"version\":1", "\"version\":2");
        fs::write(&refused_index, planted).unwrap();
        fs::write(stores_dir(&tmp.path).join("notes.txt"), "not a store").unwrap();
        stores
    }

    #[test]
    fn remembered_everywhere_merges_every_instance_store_and_skips_one_that_refuses() {
        let tmp = TempDir::new("envoy-sessions-remembered-everywhere");
        let config = enabled();
        assert!(
            EnvoySessions::remembered_everywhere(&tmp.path, &config)
                .unwrap()
                .is_empty()
        );
        assert!(!tmp.path.join("mesh").exists());
        let stores = three_instances(&tmp, &config);
        let orphan = stores[1].dir().join(format!("{}.yaml", "de".repeat(16)));
        fs::write(&orphan, "version: 1\n").unwrap();

        let remembered = EnvoySessions::remembered_everywhere(&tmp.path, &config).unwrap();

        assert_eq!(
            remembered,
            Remembered {
                by_identity: BTreeMap::from([
                    (
                        IDENTITY_A.to_string(),
                        vec!["thread-one".to_string(), "thread-two".to_string()]
                    ),
                    (IDENTITY_B.to_string(), vec!["thread-one".to_string()]),
                ]),
                orphans: 1,
                unreadable_stores: 1,
            },
            "a thread held by two stores is named once; the refusing store is counted, not read"
        );
        assert!(!remembered.is_empty());
        assert!(record_path(&stores[2], IDENTITY_A, "thread-one").exists());
        assert!(orphan.exists(), "reading changes nothing");
    }

    #[test]
    fn sweep_passes_over_a_directory_that_holds_neither_an_index_nor_a_record_file() {
        let tmp = TempDir::new("envoy-sessions-sweep-not-a-store");
        let config = enabled();
        let stores = stores_dir(&tmp.path);
        let not_a_store = stores.join("inst-empty");
        fs::create_dir_all(&not_a_store).unwrap();
        fs::write(not_a_store.join("notes.txt"), "not a store").unwrap();
        fs::write(not_a_store.join("README.yaml"), "not a record").unwrap();
        let orphan_only = stores.join("inst-orphan");
        fs::create_dir_all(&orphan_only).unwrap();
        let orphan = orphan_only.join(format!("{}.yaml", "de".repeat(16)));
        fs::write(&orphan, "version: 1\n").unwrap();

        let remembered = EnvoySessions::remembered_everywhere(&tmp.path, &config).unwrap();

        assert_eq!(
            remembered,
            Remembered {
                by_identity: BTreeMap::new(),
                orphans: 1,
                unreadable_stores: 0,
            },
            "a directory with a record file is a store without an index; one without is nothing"
        );
        assert!(!remembered.is_empty());
        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &config).unwrap(),
            Forgotten {
                removed: 0,
                unreadable_stores: 0
            },
            "the orphan goes but was never a conversation"
        );
        assert!(!orphan.exists());
        assert!(not_a_store.join("README.yaml").exists());
        assert!(
            EnvoySessions::remembered_everywhere(&tmp.path, &config)
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn sweep_passes_over_a_store_behind_a_symlink() {
        let tmp = TempDir::new("envoy-sessions-sweep-symlink");
        let config = enabled();
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let elsewhere = tmp.path.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("index.json"), "{not json").unwrap();
        let linked = stores_dir(&tmp.path).join("inst-linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();

        assert_eq!(
            EnvoySessions::remembered_everywhere(&tmp.path, &config).unwrap(),
            Remembered {
                by_identity: BTreeMap::from([(
                    IDENTITY_A.to_string(),
                    vec!["thread-one".to_string()]
                )]),
                orphans: 0,
                unreadable_stores: 0,
            },
            "the linked directory is neither read nor counted"
        );
        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &config).unwrap(),
            Forgotten {
                removed: 1,
                unreadable_stores: 0
            }
        );
        assert_eq!(
            fs::read_to_string(elsewhere.join("index.json")).unwrap(),
            "{not json",
            "a wipe never follows the link"
        );
    }

    #[test]
    fn delete_thread_everywhere_removes_one_thread_from_every_instance_store() {
        let tmp = TempDir::new("envoy-sessions-delete-thread-everywhere");
        let config = enabled();
        let forgot = |removed, unreadable_stores| Forgotten {
            removed,
            unreadable_stores,
        };
        assert_eq!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &config, IDENTITY_A, "thread-one")
                .unwrap(),
            forgot(0, 0)
        );
        assert!(matches!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &config, "not-a-hash", "thread-one"),
            Err(EnvoyMemoryError::NotAnIdentity(_))
        ));
        assert!(!tmp.path.join("mesh").exists());
        let stores = three_instances(&tmp, &config);

        assert_eq!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &config, IDENTITY_A, "thread-one")
                .unwrap(),
            forgot(2, 1),
            "the two stores that could be read held it; the refusing one is skipped and counted"
        );

        for store in &stores[..2] {
            assert!(!record_path(store, IDENTITY_A, "thread-one").exists());
            assert!(record_path(store, IDENTITY_B, "thread-one").exists());
        }
        assert!(record_path(&stores[0], IDENTITY_A, "thread-two").exists());
        assert!(record_path(&stores[2], IDENTITY_A, "thread-one").exists());
        assert_eq!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &config, IDENTITY_A, "thread-one")
                .unwrap(),
            forgot(0, 1)
        );
    }

    #[test]
    fn delete_all_everywhere_empties_every_instance_store_and_wipes_one_it_cannot_read_unread() {
        let tmp = TempDir::new("envoy-sessions-delete-all-everywhere");
        let config = enabled();
        let forgot = |removed, unreadable_stores| Forgotten {
            removed,
            unreadable_stores,
        };
        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &config).unwrap(),
            forgot(0, 0)
        );
        assert!(!tmp.path.join("mesh").exists());
        let stores = three_instances(&tmp, &config);
        let refused_index = stores[2].index_path();
        let notes = stores[2].dir().join("notes.txt");
        fs::write(&notes, "keep").unwrap();

        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &config).unwrap(),
            forgot(7, 0),
            "two conversations in each readable store plus the first's second thread, and the two record files of the store wiped unread"
        );

        for store in &stores[..2] {
            assert_eq!(store.stats().unwrap(), (0, 0));
        }
        assert!(!record_path(&stores[2], IDENTITY_A, "thread-one").exists());
        assert!(!record_path(&stores[2], IDENTITY_B, "thread-one").exists());
        assert!(
            !refused_index.exists(),
            "the index that could not be read goes too"
        );
        assert!(notes.exists(), "only record files and the index are wiped");
        assert_eq!(
            EnvoySessions::remembered_everywhere(&tmp.path, &config).unwrap(),
            Remembered::default(),
            "wiped, the directory holds neither index nor record and is no store"
        );
    }

    #[test]
    fn delete_identity_everywhere_leaves_a_store_whose_index_cannot_be_read_and_counts_it() {
        let tmp = TempDir::new("envoy-sessions-delete-identity-unreadable");
        let config = enabled();
        let stores = three_instances(&tmp, &config);
        let refused_index = stores[2].index_path();
        let planted = fs::read_to_string(&refused_index).unwrap();

        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &config, IDENTITY_B).unwrap(),
            Forgotten {
                removed: 2,
                unreadable_stores: 1
            }
        );

        assert_eq!(fs::read_to_string(&refused_index).unwrap(), planted);
        assert!(record_path(&stores[2], IDENTITY_B, "thread-one").exists());
        assert_eq!(
            EnvoySessions::remembered_everywhere(&tmp.path, &config)
                .unwrap()
                .unreadable_stores,
            1
        );
    }

    #[test]
    fn delete_all_everywhere_removes_the_record_files_the_index_does_not_name() {
        let tmp = TempDir::new("envoy-sessions-delete-all-orphans");
        let config = enabled();
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let orphan = store.dir().join(format!("{}.yaml", "de".repeat(16)));
        fs::write(&orphan, "version: 1\n").unwrap();
        let not_a_record = store.dir().join("notes.txt");
        fs::write(&not_a_record, "keep").unwrap();

        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &config).unwrap(),
            Forgotten {
                removed: 1,
                unreadable_stores: 0
            },
            "the orphan was never a conversation, so it is not counted"
        );

        let left: Vec<PathBuf> = fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == RECORD_EXTENSION))
            .collect();
        assert_eq!(left, Vec::<PathBuf>::new());
        assert!(not_a_record.exists(), "only record files are swept");
        assert_eq!(store.stats().unwrap(), (0, 0));
    }

    /// The config of a node that is off is not validated, so a zero bound can reach the
    /// sweep; its stores never save or prune and take it as it is.
    #[test]
    fn the_everywhere_helpers_take_an_unvalidated_config_while_the_memory_is_off() {
        let tmp = TempDir::new("envoy-sessions-everywhere-unvalidated");
        let on = store(&tmp, &enabled());
        on.save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        on.save(IDENTITY_B, DEST_B, "thread-one", exchange(2), t(1_000))
            .unwrap();
        let off = EnvoyMemoryConfig {
            enabled: false,
            max_turns: 0,
            ..EnvoyMemoryConfig::default()
        };

        assert_eq!(
            EnvoySessions::remembered_everywhere(&tmp.path, &off)
                .unwrap()
                .by_identity,
            BTreeMap::from([
                (IDENTITY_A.to_string(), vec!["thread-one".to_string()]),
                (IDENTITY_B.to_string(), vec!["thread-one".to_string()]),
            ])
        );
        assert_eq!(
            EnvoySessions::delete_identity_everywhere(&tmp.path, &off, IDENTITY_A).unwrap(),
            Forgotten {
                removed: 1,
                unreadable_stores: 0
            }
        );
        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &off).unwrap(),
            Forgotten {
                removed: 1,
                unreadable_stores: 0
            }
        );
        assert!(
            EnvoySessions::remembered_everywhere(&tmp.path, &off)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_everywhere_helpers_read_and_wipe_while_the_memory_is_off() {
        let tmp = TempDir::new("envoy-sessions-everywhere-while-off");
        let on = store(&tmp, &enabled());
        on.save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        on.save(IDENTITY_A, DEST_A, "thread-two", exchange(2), t(1_000))
            .unwrap();
        on.save(IDENTITY_B, DEST_B, "thread-one", exchange(3), t(1_000))
            .unwrap();

        let off = EnvoyMemoryConfig::default();
        assert!(!off.enabled);
        assert_eq!(
            EnvoySessions::remembered_everywhere(&tmp.path, &off)
                .unwrap()
                .by_identity
                .get(IDENTITY_A),
            Some(&vec!["thread-one".to_string(), "thread-two".to_string()])
        );
        assert_eq!(
            EnvoySessions::delete_thread_everywhere(&tmp.path, &off, IDENTITY_A, "thread-two")
                .unwrap(),
            Forgotten {
                removed: 1,
                unreadable_stores: 0
            }
        );
        assert_eq!(
            EnvoySessions::remembered_everywhere(&tmp.path, &off)
                .unwrap()
                .by_identity
                .get(IDENTITY_A),
            Some(&vec!["thread-one".to_string()])
        );
        assert_eq!(
            EnvoySessions::delete_all_everywhere(&tmp.path, &off).unwrap(),
            Forgotten {
                removed: 2,
                unreadable_stores: 0
            }
        );
        assert!(
            EnvoySessions::remembered_everywhere(&tmp.path, &off)
                .unwrap()
                .is_empty()
        );
        assert_eq!(on.stats().unwrap(), (0, 0));
    }

    #[test]
    fn append_adds_after_the_held_turns_and_creates_only_when_asked() {
        let tmp = TempDir::new("envoy-sessions-append");
        let store = store(&tmp, &enabled());
        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                exchange(1),
                t(1_000),
                false,
            )
            .unwrap();
        assert!(
            !tmp.path.join("mesh").exists(),
            "an append to a store that does not exist creates nothing"
        );
        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                exchange(1),
                t(1_000),
                true,
            )
            .unwrap();

        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                vec![assistant("late")],
                t(1_001),
                false,
            )
            .unwrap();
        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-two",
                exchange(2),
                t(1_002),
                false,
            )
            .unwrap();
        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-three",
                exchange(3),
                t(1_003),
                true,
            )
            .unwrap();

        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_001))
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.turns,
            [exchange(1), vec![assistant("late")]].concat()
        );
        assert_eq!(loaded.last_used, rfc3339_utc(t(1_001)));
        assert_eq!(store.stats().unwrap(), (2, 1));
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-two", t(1_002))
                .unwrap(),
            None,
            "an append without create to a thread not held writes nothing"
        );
        assert!(!record_path(&store, IDENTITY_A, "thread-two").exists());
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-three", t(1_003))
                .unwrap()
                .unwrap()
                .turns,
            exchange(3)
        );
    }

    #[test]
    fn append_once_skips_a_turn_the_record_already_ends_with() {
        let tmp = TempDir::new("envoy-sessions-append-once");
        let store = store(&tmp, &enabled());
        store
            .append_once(IDENTITY_A, DEST_A, "thread-one", assistant("yes"), t(1_000))
            .unwrap();
        assert!(
            !tmp.path.join("mesh").exists(),
            "an append-once never creates a thread"
        );
        store
            .save(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                vec![user("may I?"), assistant("yes")],
                t(1_000),
            )
            .unwrap();

        store
            .append_once(IDENTITY_A, DEST_A, "thread-one", assistant("yes"), t(1_001))
            .unwrap();
        let held = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_001))
            .unwrap()
            .unwrap();
        assert_eq!(held.turns, vec![user("may I?"), assistant("yes")]);
        assert_eq!(held.last_used, rfc3339_utc(t(1_000)));

        store
            .append_once(IDENTITY_A, DEST_A, "thread-one", assistant("no"), t(1_002))
            .unwrap();
        let held = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_002))
            .unwrap()
            .unwrap();
        assert_eq!(
            held.turns,
            vec![user("may I?"), assistant("yes"), assistant("no")]
        );
        assert_eq!(held.last_used, rfc3339_utc(t(1_002)));
    }

    #[test]
    fn an_append_never_revives_an_expired_thread() {
        let tmp = TempDir::new("envoy-sessions-append-expired");
        let config = EnvoyMemoryConfig {
            ttl_hours: 1,
            ..enabled()
        };
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let late = t(1_000 + 3_601);

        store
            .append_once(IDENTITY_A, DEST_A, "thread-one", assistant("late"), late)
            .unwrap();
        assert_eq!(
            store.load(IDENTITY_A, DEST_A, "thread-one", late).unwrap(),
            None
        );
        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert_eq!(store.stats().unwrap(), (0, 0));

        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .append(IDENTITY_A, DEST_A, "thread-one", exchange(2), late, true)
            .unwrap();
        let held = store
            .load(IDENTITY_A, DEST_A, "thread-one", late)
            .unwrap()
            .unwrap();
        assert_eq!(
            held.turns,
            exchange(2),
            "with create the expired turns go and only the new ones are the thread"
        );
        assert_eq!(held.last_used, rfc3339_utc(late));
    }

    /// A thread's `ttl_hours` run from the time it was last written. A load, and a
    /// retried turn the record already ends with, extend nothing: the thread expires at
    /// the hour after its save as if neither had happened. A turn that is written starts
    /// the hour over.
    #[test]
    fn a_threads_life_runs_from_its_last_write_not_a_read_or_a_skipped_retry() {
        let tmp = TempDir::new("envoy-sessions-ttl-from-write");
        let config = EnvoyMemoryConfig {
            ttl_hours: 1,
            ..enabled()
        };
        let store = store(&tmp, &config);
        let saved = vec![user("may I?"), assistant("yes")];
        store
            .save(IDENTITY_A, DEST_A, "thread-one", saved.clone(), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", saved.clone(), t(1_000))
            .unwrap();

        // Half an hour in: a read and a retry of the turn the record ends with.
        assert!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000 + 1_800))
                .unwrap()
                .is_some()
        );
        store
            .append_once(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                assistant("yes"),
                t(1_000 + 1_800),
            )
            .unwrap();
        // The same half hour in: a turn that is written.
        store
            .append_once(
                IDENTITY_A,
                DEST_A,
                "thread-two",
                assistant("no"),
                t(1_000 + 1_800),
            )
            .unwrap();

        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000 + 3_600))
                .unwrap(),
            None,
            "neither the read nor the skipped retry kept the thread past its hour"
        );
        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        let held = store
            .load(IDENTITY_A, DEST_A, "thread-two", t(1_000 + 1_800 + 3_599))
            .unwrap()
            .expect("the written turn started the hour over");
        assert_eq!(
            held.turns,
            vec![user("may I?"), assistant("yes"), assistant("no")]
        );
        assert_eq!(held.last_used, rfc3339_utc(t(1_000 + 1_800)));
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-two", t(1_000 + 1_800 + 3_600))
                .unwrap(),
            None
        );
        assert_eq!(store.stats().unwrap(), (0, 0));
    }

    #[test]
    fn append_is_bounded_like_save() {
        let tmp = TempDir::new("envoy-sessions-append-bounded");
        let store = store(
            &tmp,
            &EnvoyMemoryConfig {
                max_turns: 4,
                ..enabled()
            },
        );
        for n in 1..=3 {
            store
                .append(
                    IDENTITY_A,
                    DEST_A,
                    "thread-one",
                    exchange(n),
                    t(1_000 + n as u64),
                    true,
                )
                .unwrap();
        }

        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_003))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.turns, [exchange(2), exchange(3)].concat());
    }

    #[test]
    fn stats_counts_conversations_and_distinct_identities() {
        let tmp = TempDir::new("envoy-sessions-stats");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(2), t(1_001))
            .unwrap();
        store
            .save(IDENTITY_B, DEST_B, "thread-one", exchange(3), t(1_002))
            .unwrap();

        assert_eq!(store.stats().unwrap(), (3, 2));

        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(4), t(1_003))
            .unwrap();
        assert_eq!(
            store.stats().unwrap(),
            (3, 2),
            "a resave replaces, not adds"
        );
    }

    #[test]
    fn an_indexed_record_missing_from_disk_is_dropped_from_the_index() {
        let tmp = TempDir::new("envoy-sessions-missing-record");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        fs::remove_file(record_path(&store, IDENTITY_A, "thread-one")).unwrap();

        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
                .unwrap(),
            None
        );
        assert_eq!(store.stats().unwrap(), (0, 0));
    }

    #[test]
    fn a_malformed_identity_keys_nothing() {
        let tmp = TempDir::new("envoy-sessions-bad-identity");
        let store = store(&tmp, &enabled());

        let not_hex = "g".repeat(32);
        for identity in ["", "abc", not_hex.as_str()] {
            let err = store
                .save(identity, DEST_A, "thread", exchange(1), t(1_000))
                .unwrap_err();
            assert!(matches!(err, EnvoyMemoryError::NotAnIdentity(_)), "{err:?}");
            assert!(store.load(identity, DEST_A, "thread", t(1_000)).is_err());
            assert!(store.delete_identity(identity).is_err());
            assert!(store.delete(identity, "thread").is_err());
        }
        for destination in ["", "abc", not_hex.as_str()] {
            let err = store
                .save(IDENTITY_A, destination, "thread", exchange(1), t(1_000))
                .unwrap_err();
            assert!(
                matches!(err, EnvoyMemoryError::NotADestination(_)),
                "{err:?}"
            );
            assert!(
                matches!(
                    store.load(IDENTITY_A, destination, "thread", t(1_000)),
                    Err(EnvoyMemoryError::NotADestination(_))
                ),
                "a malformed destination keys nothing"
            );
        }
        assert!(!tmp.path.join("mesh").exists());
    }

    fn limits(config: &EnvoyMemoryConfig) -> EnvoyMemoryLimits {
        EnvoyMemoryLimits::from(config)
    }

    fn roles(turns: &[EnvoyTurn]) -> Vec<EnvoyRole> {
        turns.iter().map(|turn| turn.role).collect()
    }

    fn threads_of(store: &EnvoySessions, identity: &str) -> Vec<String> {
        let mut threads: Vec<String> = store
            .read_index()
            .unwrap()
            .entries
            .into_iter()
            .filter(|entry| entry.identity == identity)
            .map(|entry| entry.thread)
            .collect();
        threads.sort();
        threads
    }

    #[test]
    fn truncation_drops_the_oldest_turn_through_the_next_user_boundary() {
        let config = EnvoyMemoryConfig {
            max_turns: 2,
            ..enabled()
        };
        let mut turns = vec![user("q1"), assistant("a1"), assistant("a1b"), user("q2")];
        truncate(&mut turns, &limits(&config));
        assert_eq!(
            roles(&turns),
            [EnvoyRole::User],
            "a question and both its answers go as one"
        );
        assert_eq!(turns[0].text, "q2");

        let mut turns = vec![user("q1"), user("q2"), assistant("a2"), user("q3")];
        truncate(&mut turns, &limits(&config));
        assert_eq!(
            turns,
            [user("q3")],
            "[User] then [User, Assistant] are two units; both had to go to fit two turns"
        );

        let mut turns = vec![user("q1"), user("q2"), assistant("a2")];
        truncate(&mut turns, &limits(&config));
        assert_eq!(turns, [user("q2"), assistant("a2")]);

        let mut turns = vec![assistant("a0"), assistant("a0b"), user("q1"), user("q2")];
        truncate(&mut turns, &limits(&config));
        assert_eq!(
            turns,
            [user("q1"), user("q2")],
            "a leading run of answers goes with the first exchange"
        );

        let mut turns = vec![user("q1"), assistant("a1")];
        truncate(&mut turns, &limits(&config));
        assert_eq!(
            turns,
            [user("q1"), assistant("a1")],
            "within bounds, untouched"
        );
    }

    #[test]
    fn turns_over_max_turns_are_truncated_on_save() {
        let tmp = TempDir::new("envoy-sessions-max-turns");
        let config = EnvoyMemoryConfig {
            max_turns: 4,
            ..enabled()
        };
        let store = store(&tmp, &config);
        let turns: Vec<EnvoyTurn> = (1..=3).flat_map(exchange).collect();

        store
            .save(IDENTITY_A, DEST_A, "thread-one", turns, t(1_000))
            .unwrap();

        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.turns,
            [exchange(2), exchange(3)].concat(),
            "the oldest exchange went"
        );
        let entry = &store.read_index().unwrap().entries[0];
        assert_eq!(entry.turns, 4);
        assert_eq!(entry.bytes, text_bytes(&loaded.turns));
    }

    #[test]
    fn bytes_over_max_bytes_are_truncated_on_save() {
        let tmp = TempDir::new("envoy-sessions-max-bytes");
        let config = EnvoyMemoryConfig {
            max_bytes: 40,
            ..enabled()
        };
        let store = store(&tmp, &config);
        let turns: Vec<EnvoyTurn> = (1..=3).flat_map(exchange).collect();
        assert_eq!(text_bytes(&turns), 54);

        store
            .save(IDENTITY_A, DEST_A, "thread-one", turns, t(1_000))
            .unwrap();

        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.turns, [exchange(2), exchange(3)].concat());
        assert_eq!(text_bytes(&loaded.turns), 36);
        assert_eq!(store.read_index().unwrap().entries[0].bytes, 36);
    }

    /// A cap that dropping one turn would satisfy still loses the whole exchange: the
    /// byte bound cuts at the same boundary as the turn bound.
    #[test]
    fn byte_truncation_drops_the_whole_exchange_not_one_turn() {
        let tmp = TempDir::new("envoy-sessions-max-bytes-unit");
        let config = EnvoyMemoryConfig {
            max_bytes: 45,
            ..enabled()
        };
        let store = store(&tmp, &config);
        let turns: Vec<EnvoyTurn> = (1..=3).flat_map(exchange).collect();
        assert_eq!(text_bytes(&turns), 54);
        assert_eq!(
            text_bytes(&turns[1..]),
            44,
            "dropping the first question alone would fit the cap"
        );

        store
            .save(IDENTITY_A, DEST_A, "thread-one", turns, t(1_000))
            .unwrap();

        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.turns,
            [exchange(2), exchange(3)].concat(),
            "the first answer went with its question"
        );
        assert_eq!(text_bytes(&loaded.turns), 36);

        let mut turns = vec![user("q1"), assistant("a1"), assistant("a1b"), user("q2")];
        assert_eq!(text_bytes(&turns[1..]), 7);
        truncate(
            &mut turns,
            &limits(&EnvoyMemoryConfig {
                max_bytes: 7,
                ..enabled()
            }),
        );
        assert_eq!(
            turns,
            [user("q2")],
            "both answers went with the question they answered"
        );
    }

    #[test]
    fn a_single_unit_over_max_bytes_leaves_nothing_and_deletes_the_record() {
        let tmp = TempDir::new("envoy-sessions-oversized-unit");
        let config = EnvoyMemoryConfig {
            max_bytes: 20,
            ..enabled()
        };
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        assert!(record_path(&store, IDENTITY_A, "thread-one").exists());

        store
            .save(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                vec![user("q"), assistant(&"a".repeat(32))],
                t(1_001),
            )
            .unwrap();

        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_001))
                .unwrap(),
            None
        );
        assert_eq!(store.stats().unwrap(), (0, 0));
    }

    #[test]
    fn the_least_recently_used_session_goes_first_over_max_sessions() {
        let tmp = TempDir::new("envoy-sessions-max-sessions");
        let config = EnvoyMemoryConfig {
            max_sessions: 2,
            ..enabled()
        };
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_B, DEST_B, "thread-one", exchange(2), t(1_002))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(3), t(1_001))
            .unwrap();

        assert_eq!(store.stats().unwrap(), (2, 2));
        assert!(
            !record_path(&store, IDENTITY_A, "thread-one").exists(),
            "the oldest last_used went, wherever it sat in the index"
        );
        assert!(record_path(&store, IDENTITY_B, "thread-one").exists());
        assert!(
            record_path(&store, IDENTITY_A, "thread-two").exists(),
            "the thread just saved is never the one evicted, even when its stamp is older"
        );
    }

    #[test]
    fn the_least_recently_used_session_of_one_identity_goes_first_over_max_per_identity() {
        let tmp = TempDir::new("envoy-sessions-max-per-identity");
        let config = EnvoyMemoryConfig {
            max_per_identity: 2,
            ..enabled()
        };
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_B, DEST_B, "thread-one", exchange(1), t(900))
            .unwrap();
        store
            .save(IDENTITY_B, DEST_B, "thread-two", exchange(2), t(901))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(3), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(4), t(1_001))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-three", exchange(5), t(1_002))
            .unwrap();

        assert_eq!(
            threads_of(&store, IDENTITY_A),
            ["thread-one", "thread-three"],
            "A's least recently used thread went"
        );
        assert!(!record_path(&store, IDENTITY_A, "thread-two").exists());
        assert_eq!(
            threads_of(&store, IDENTITY_B),
            ["thread-one", "thread-two"],
            "B's older threads are not A's to lose"
        );
        assert_eq!(store.stats().unwrap(), (4, 2));
    }

    #[test]
    fn a_session_older_than_ttl_is_not_loaded_and_is_removed() {
        let tmp = TempDir::new("envoy-sessions-ttl");
        let config = EnvoyMemoryConfig {
            ttl_hours: 1,
            ..enabled()
        };
        let store = store(&tmp, &config);
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-two", exchange(2), t(1_000))
            .unwrap();

        assert!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000 + 3_599))
                .unwrap()
                .is_some(),
            "one second short of the ttl is still a conversation"
        );
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000 + 3_600))
                .unwrap(),
            None
        );

        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert_eq!(
            threads_of(&store, IDENTITY_A),
            ["thread-two"],
            "only the thread looked at is judged; the rest waits for prune"
        );
        assert!(record_path(&store, IDENTITY_A, "thread-two").exists());
    }

    #[test]
    fn prune_drops_expired_sessions_enforces_the_caps_and_removes_orphan_files() {
        let tmp = TempDir::new("envoy-sessions-prune");
        let lenient = store(&tmp, &enabled());
        for (identity, thread, at) in [
            (IDENTITY_A, "expired", 0),
            (IDENTITY_A, "a-old", 1_000),
            (IDENTITY_A, "a-mid", 1_001),
            (IDENTITY_A, "a-new", 1_002),
            (IDENTITY_B, "b-old", 1_003),
            (IDENTITY_B, "b-new", 1_004),
        ] {
            lenient
                .save(identity, dest(identity), thread, exchange(1), t(at))
                .unwrap();
        }
        assert_eq!(lenient.stats().unwrap(), (6, 2));
        let store = store(
            &tmp,
            &EnvoyMemoryConfig {
                max_sessions: 3,
                max_per_identity: 2,
                ttl_hours: 1,
                ..enabled()
            },
        );
        let orphan = store.dir().join(format!("{}.yaml", "ab".repeat(16)));
        fs::write(&orphan, "version: 1\n").unwrap();
        let index_before = fs::read_to_string(store.index_path()).unwrap();

        assert_eq!(store.prune(t(3_700)).unwrap(), 3);

        assert_eq!(
            threads_of(&store, IDENTITY_A),
            ["a-new"],
            "expired went first, then A over its cap, then the globally oldest"
        );
        assert_eq!(threads_of(&store, IDENTITY_B), ["b-new", "b-old"]);
        assert_eq!(store.stats().unwrap(), (3, 2));
        for thread in ["expired", "a-old", "a-mid"] {
            assert!(
                !record_path(&store, IDENTITY_A, thread).exists(),
                "{thread}"
            );
        }
        for (identity, thread) in [
            (IDENTITY_A, "a-new"),
            (IDENTITY_B, "b-old"),
            (IDENTITY_B, "b-new"),
        ] {
            assert!(record_path(&store, identity, thread).exists(), "{thread}");
        }
        assert!(!orphan.exists());
        assert_ne!(
            fs::read_to_string(store.index_path()).unwrap(),
            index_before
        );

        let index_after = fs::read_to_string(store.index_path()).unwrap();
        assert_eq!(store.prune(t(3_700)).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(store.index_path()).unwrap(),
            index_after,
            "nothing to drop, nothing rewritten"
        );
    }

    fn planted_store(tag: &str) -> (TempDir, EnvoySessions) {
        let tmp = TempDir::new(tag);
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        (tmp, store)
    }

    fn assert_refused_in_place(store: &EnvoySessions, path: &Path, planted: &str) {
        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("envoy memory"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(
            store
                .save(IDENTITY_A, DEST_A, "thread-one", exchange(2), t(1_001))
                .is_err()
        );
        assert!(
            store
                .append(
                    IDENTITY_A,
                    DEST_A,
                    "thread-one",
                    exchange(2),
                    t(1_001),
                    false
                )
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            planted,
            "a refused file is left where it is"
        );
        assert!(
            !names_in(store.dir())
                .iter()
                .any(|name| name.contains("corrupt") || name.ends_with(".tmp")),
            "{:?}",
            names_in(store.dir())
        );
    }

    #[test]
    fn a_newer_record_version_refuses_naming_the_file() {
        let (_tmp, store) = planted_store("envoy-sessions-newer-record");
        let path = record_path(&store, IDENTITY_A, "thread-one");
        let planted = fs::read_to_string(&path).unwrap().replace(
            &format!("version: {ENVOY_SESSION_VERSION}\n"),
            &format!("version: {}\nfuture_field: 1\n", ENVOY_SESSION_VERSION + 1),
        );
        fs::write(&path, &planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::Version { found, expected, .. }
                if found == ENVOY_SESSION_VERSION + 1 && expected == ENVOY_SESSION_VERSION),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains(&format!("version {}", ENVOY_SESSION_VERSION + 1)),
            "{text}"
        );
        assert!(text.contains("upgrade Coyote"), "{text}");
        assert_refused_in_place(&store, &path, &planted);
    }

    #[test]
    fn a_pre_baseline_record_version_refuses_as_having_no_migration() {
        let (_tmp, store) = planted_store("envoy-sessions-older-record");
        let path = record_path(&store, IDENTITY_A, "thread-one");
        let planted = fs::read_to_string(&path).unwrap().replace(
            &format!("version: {ENVOY_SESSION_VERSION}\n"),
            "version: 0\n",
        );
        fs::write(&path, &planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err()
            .to_string();
        assert!(err.contains("version 0"), "{err}");
        assert!(err.contains("no migration"), "{err}");
        assert!(!err.contains("upgrade Coyote"), "{err}");
        assert_refused_in_place(&store, &path, &planted);
    }

    #[test]
    fn a_record_without_a_version_refuses() {
        let (_tmp, store) = planted_store("envoy-sessions-unversioned-record");
        let path = record_path(&store, IDENTITY_A, "thread-one");
        let planted = fs::read_to_string(&path)
            .unwrap()
            .replace(&format!("version: {ENVOY_SESSION_VERSION}\n"), "");
        fs::write(&path, &planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::Unversioned { .. }),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("no readable `version` field"),
            "{err}"
        );
        assert_refused_in_place(&store, &path, &planted);
    }

    #[test]
    fn a_record_with_an_unknown_field_is_refused() {
        let (_tmp, store) = planted_store("envoy-sessions-unknown-record-field");
        let path = record_path(&store, IDENTITY_A, "thread-one");
        let planted = format!("{}added_later: true\n", fs::read_to_string(&path).unwrap());
        fs::write(&path, &planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::NotARecord { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("added_later"), "{err}");
        assert_refused_in_place(&store, &path, &planted);
    }

    #[test]
    fn a_newer_index_version_refuses_the_whole_store() {
        let (_tmp, store) = planted_store("envoy-sessions-newer-index");
        let path = store.index_path();
        let mut index: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        index["version"] = serde_json::json!(ENVOY_SESSION_INDEX_VERSION + 1);
        index["future_field"] = serde_json::json!(1);
        let planted = index.to_string();
        fs::write(&path, &planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::Version { found, expected, .. }
                if found == ENVOY_SESSION_INDEX_VERSION + 1 && expected == ENVOY_SESSION_INDEX_VERSION),
            "{err:?}"
        );
        assert!(store.prune(t(2_000)).is_err());
        assert!(store.stats().is_err());
        assert!(store.delete(IDENTITY_A, "thread-one").is_err());
        assert!(store.delete_identity(IDENTITY_A).is_err());
        assert!(
            record_path(&store, IDENTITY_A, "thread-one").exists(),
            "no record is touched behind a refused index"
        );
        assert_refused_in_place(&store, &path, &planted);
    }

    #[test]
    fn an_index_with_an_unknown_field_refuses_the_whole_store() {
        let (_tmp, store) = planted_store("envoy-sessions-unknown-index-field");
        let path = store.index_path();
        let original: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let mut index = original.clone();
        index["added_later"] = serde_json::json!(true);
        let planted = index.to_string();
        fs::write(&path, &planted).unwrap();

        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::NotARecord { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("added_later"), "{err}");
        assert_refused_in_place(&store, &path, &planted);

        let mut entry_field = original;
        entry_field["entries"][0]["added_later"] = serde_json::json!(1);
        fs::write(&path, entry_field.to_string()).unwrap();
        let err = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::NotARecord { .. }),
            "an unknown field on an entry refuses too: {err:?}"
        );
    }

    #[test]
    fn an_unknown_record_version_is_refused_in_place_never_migrated() {
        let (_tmp, store) = planted_store("envoy-sessions-never-migrated");
        let path = record_path(&store, IDENTITY_A, "thread-one");
        let planted = fs::read_to_string(&path).unwrap().replace(
            &format!("version: {ENVOY_SESSION_VERSION}\n"),
            &format!("version: {}\n", ENVOY_SESSION_VERSION + 7),
        );
        fs::write(&path, &planted).unwrap();
        let before = names_in(store.dir());

        for _ in 0..3 {
            assert!(
                store
                    .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
                    .is_err()
            );
            assert!(
                store
                    .save(IDENTITY_A, DEST_A, "thread-one", exchange(2), t(1_001))
                    .is_err()
            );
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), planted);
        assert_eq!(names_in(store.dir()), before, "no sibling appears");
    }

    #[test]
    fn a_save_that_truncates_to_nothing_still_refuses_an_unreadable_record() {
        let tmp = TempDir::new("envoy-sessions-empty-save-refuses");
        let store = store(
            &tmp,
            &EnvoyMemoryConfig {
                max_bytes: 20,
                ..enabled()
            },
        );
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let path = record_path(&store, IDENTITY_A, "thread-one");
        let planted = fs::read_to_string(&path).unwrap().replace(
            &format!("version: {ENVOY_SESSION_VERSION}\n"),
            &format!("version: {}\n", ENVOY_SESSION_VERSION + 1),
        );
        fs::write(&path, &planted).unwrap();
        let oversized = vec![user("q"), assistant(&"a".repeat(32))];

        let err = store
            .save(IDENTITY_A, DEST_A, "thread-one", oversized, t(1_001))
            .unwrap_err();

        assert!(
            matches!(err, EnvoyMemoryError::Version { found, expected, .. }
                if found == ENVOY_SESSION_VERSION + 1 && expected == ENVOY_SESSION_VERSION),
            "{err:?}"
        );
        assert!(
            path.exists(),
            "the save that cut to nothing did not unlink it"
        );
        assert_eq!(
            threads_of(&store, IDENTITY_A),
            ["thread-one"],
            "nor drop it from the index"
        );
        assert_refused_in_place(&store, &path, &planted);
    }

    #[test]
    fn prune_removes_orphan_record_files() {
        let tmp = TempDir::new("envoy-sessions-orphans");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let orphan = store.dir().join(format!("{}.yaml", "ab".repeat(16)));
        fs::write(&orphan, "version: 1\n").unwrap();
        let not_a_record = store.dir().join("notes.txt");
        fs::write(&not_a_record, "keep").unwrap();

        store.prune(t(1_000)).unwrap();

        assert!(!orphan.exists());
        assert!(not_a_record.exists(), "only record files are swept");
        assert!(record_path(&store, IDENTITY_A, "thread-one").exists());
    }

    #[test]
    fn prune_on_a_missing_directory_creates_nothing() {
        let tmp = TempDir::new("envoy-sessions-prune-missing");
        let store = store(&tmp, &enabled());

        assert_eq!(store.prune(t(1_000)).unwrap(), 0);

        assert!(!tmp.path.join("mesh").exists());
    }

    #[test]
    fn errors_render_the_shared_refusal_wording() {
        let path = PathBuf::from("/tmp/mesh/envoy-sessions/inst/index.json");
        let version = EnvoyMemoryError::Version {
            path: path.clone(),
            found: 3,
            expected: 1,
        };
        assert_eq!(
            version.to_string(),
            version_refusal("envoy memory", &path, None, 3, 1, Remedy::Cache)
        );
        let unversioned = EnvoyMemoryError::Unversioned {
            path: path.clone(),
            expected: 1,
        };
        assert_eq!(
            unversioned.to_string(),
            unversioned_refusal("envoy memory", &path, None, 1, Remedy::Cache)
        );
        let io = EnvoyMemoryError::Io {
            path,
            source: io::Error::other("disk full"),
        };
        assert!(std::error::Error::source(&io).is_some());
        assert!(io.to_string().contains("disk full"), "{io}");
    }

    #[test]
    fn a_message_whose_thread_is_its_own_id_opens_a_new_session_under_it() {
        let tmp = TempDir::new("envoy-sessions-thread-own-id");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "m-1", exchange(1), t(1_000))
            .unwrap();

        for wire_thread in [Some("m-1"), None] {
            let session = store.session_thread(IDENTITY_A, DEST_A, wire_thread, "m-1", t(1_000));

            assert_eq!(session.thread, "m-1", "{wire_thread:?}");
            assert!(session.resumed.is_none(), "{wire_thread:?}");
        }
    }

    #[test]
    fn a_thread_held_for_the_sender_is_resumed_with_its_record() {
        let tmp = TempDir::new("envoy-sessions-thread-held");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "m-1", exchange(1), t(1_000))
            .unwrap();

        let session = store.session_thread(IDENTITY_A, DEST_A, Some("m-1"), "m-2", t(1_000));

        assert_eq!(session.thread, "m-1");
        let record = session.resumed.expect("the held record comes back");
        assert_eq!(record.thread, "m-1");
        assert_eq!(record.turns, exchange(1));
    }

    #[test]
    fn a_thread_the_store_does_not_hold_opens_a_new_session_under_the_message_id() {
        let tmp = TempDir::new("envoy-sessions-thread-not-held");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "m-1", exchange(1), t(1_000))
            .unwrap();

        let session =
            store.session_thread(IDENTITY_A, DEST_A, Some("build-status"), "m-2", t(1_000));

        assert_eq!(session.thread, "m-2");
        assert!(session.resumed.is_none());
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "build-status", t(1_000))
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_thread_whose_record_names_another_identity_is_not_resumed_and_is_left_alone() {
        install_log_collector();
        let tmp = TempDir::new("envoy-sessions-thread-other-identity");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "t-other", exchange(1), t(1_000))
            .unwrap();
        let path = record_path(&store, IDENTITY_A, "t-other");
        let planted = fs::read_to_string(&path)
            .unwrap()
            .replace(IDENTITY_A, IDENTITY_B);
        fs::write(&path, &planted).unwrap();

        let session = store.session_thread(IDENTITY_A, DEST_A, Some("t-other"), "m-2", t(1_000));

        assert_eq!(session.thread, "m-2");
        assert!(session.resumed.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), planted);
        let expected = format!(
            "Mesh envoy memory for thread t-other belongs to another identity than {}, so it was not resumed for this sender. A new conversation starts under message m-2 and the other record is left alone; nothing needs to be done.",
            short(IDENTITY_A)
        );
        let warns = warn_snapshot();
        assert!(warns.contains(&expected), "captured: {warns:#?}");
    }

    #[test]
    fn a_thread_whose_record_cannot_be_read_is_not_resumed_and_names_the_forget_command() {
        install_log_collector();
        let tmp = TempDir::new("envoy-sessions-thread-unreadable");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "t-unreadable", exchange(1), t(1_000))
            .unwrap();
        fs::write(record_path(&store, IDENTITY_A, "t-unreadable"), "{not yaml").unwrap();

        let session =
            store.session_thread(IDENTITY_A, DEST_A, Some("t-unreadable"), "m-2", t(1_000));

        assert_eq!(session.thread, "m-2");
        assert!(session.resumed.is_none());
        let forget = format!(
            ".mesh envoy memory forget {} t-unreadable",
            short(IDENTITY_A)
        );
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message
                .starts_with("Mesh envoy memory for thread t-unreadable could not be read (")
                && message.contains(
                    "so it was not resumed. A new conversation starts under message m-2"
                )
                && message.contains(&forget)),
            "captured: {warns:#?}"
        );
    }

    /// An index that cannot be read is no fault of one thread, so the warning points
    /// at the whole store instead of forgetting the thread.
    #[test]
    fn a_store_whose_index_cannot_be_read_is_not_resumed_and_names_forget_all() {
        install_log_collector();
        let tmp = TempDir::new("envoy-sessions-index-unreadable");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "t-held", exchange(1), t(1_000))
            .unwrap();
        fs::write(store.dir().join(INDEX_FILE), "{not json").unwrap();

        let session = store.session_thread(IDENTITY_A, DEST_A, Some("t-held"), "m-3", t(1_000));

        assert_eq!(session.thread, "m-3");
        assert!(session.resumed.is_none());
        let forget_thread = format!(".mesh envoy memory forget {} t-held", short(IDENTITY_A));
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message
                .starts_with("Mesh envoy memory could not be used (")
                && message.contains(
                    "so thread t-held was not resumed. A new conversation starts under message m-3"
                )
                && message.contains("`.mesh envoy memory forget all`")
                && !message.contains(&forget_thread)),
            "captured: {warns:#?}"
        );
    }

    const DAY: u64 = 86_400;

    fn stamp_of(store: &EnvoySessions) -> Option<String> {
        store.read_index().unwrap().store_last_used
    }

    /// Rewrites `store`'s index as a build before the stamp existed wrote it.
    fn unstamp(store: &EnvoySessions) {
        let mut index: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(store.index_path()).unwrap()).unwrap();
        index.as_object_mut().unwrap().remove("store_last_used");
        fs::write(store.index_path(), index.to_string()).unwrap();
    }

    fn keep_none() -> HashSet<String> {
        HashSet::new()
    }

    fn never() -> bool {
        false
    }

    /// A sweep asks whether to stop before it judges each store and judges none once
    /// told to, so a node that leaves the mesh mid-sweep is not held up by the rest.
    #[test]
    fn prune_unused_stops_between_stores_once_told_to() {
        let tmp = TempDir::new("envoy-sessions-prune-stop");
        let config = enabled();
        let stores: Vec<EnvoySessions> = ["inst-a", "inst-b", "inst-c"]
            .into_iter()
            .map(|instance| {
                let store = EnvoySessions::open(&tmp.path, instance, &config).unwrap();
                store
                    .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
                    .unwrap();
                store
            })
            .collect();
        let now = t(400 * DAY);

        let swept = EnvoySessions::prune_unused_everywhere(
            &tmp.path,
            &config,
            1,
            now,
            &keep_none(),
            &|| true,
        )
        .unwrap();
        assert_eq!(swept, UnusedStores::default(), "told to stop at once");
        assert!(stores.iter().all(|store| store.dir().exists()));

        let asked = std::cell::Cell::new(0);
        let after_one = || {
            asked.set(asked.get() + 1);
            asked.get() > 1
        };
        let swept = EnvoySessions::prune_unused_everywhere(
            &tmp.path,
            &config,
            1,
            now,
            &keep_none(),
            &after_one,
        )
        .unwrap();
        assert_eq!(swept.removed, 1, "{swept:?}");
        assert_eq!(asked.get(), 2, "asked before each store until told to stop");
        assert_eq!(
            stores.iter().filter(|store| store.dir().exists()).count(),
            2
        );

        let stamp_only = std::cell::Cell::new(0);
        let swept = EnvoySessions::stamp_unstamped_everywhere(
            &tmp.path,
            &config,
            now,
            &keep_none(),
            &|| {
                stamp_only.set(stamp_only.get() + 1);
                true
            },
        )
        .unwrap();
        assert_eq!(swept, UnusedStores::default());
        assert_eq!(stamp_only.get(), 1, "the stamping walk stops the same way");
    }

    #[test]
    fn a_save_an_append_and_a_load_stamp_the_store_as_used() {
        let tmp = TempDir::new("envoy-sessions-store-stamp");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        assert_eq!(stamp_of(&store), Some(rfc3339_utc(t(1_000))));

        store
            .append(
                IDENTITY_A,
                DEST_A,
                "thread-one",
                exchange(2),
                t(2_000),
                false,
            )
            .unwrap();
        assert_eq!(stamp_of(&store), Some(rfc3339_utc(t(2_000))));

        store
            .load(IDENTITY_A, DEST_A, "thread-one", t(2_000 + 1_800))
            .unwrap();
        assert_eq!(
            stamp_of(&store),
            Some(rfc3339_utc(t(2_000))),
            "a load inside the refresh window leaves the index alone"
        );
        store
            .load(IDENTITY_A, DEST_A, "thread-one", t(2_000 + 3_600))
            .unwrap();
        assert_eq!(stamp_of(&store), Some(rfc3339_utc(t(2_000 + 3_600))));

        unstamp(&store);
        assert_eq!(stamp_of(&store), None, "an index without the field reads");
        let session = store.session_thread(IDENTITY_A, DEST_A, Some("thread-one"), "m-2", t(6_000));
        assert!(session.resumed.is_some());
        assert_eq!(stamp_of(&store), Some(rfc3339_utc(t(6_000))));
        assert_eq!(store.read_index().unwrap().version, 1);
    }

    #[test]
    fn a_load_stamps_no_store_whose_index_does_not_exist() {
        let tmp = TempDir::new("envoy-sessions-stamp-no-index");
        let store = store(&tmp, &enabled());
        assert_eq!(
            store
                .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
                .unwrap(),
            None
        );
        assert!(!tmp.path.join("mesh").exists());
        let orphan = store.dir().join(format!("{}.yaml", "de".repeat(16)));
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(&orphan, "version: 1\n").unwrap();
        store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000))
            .unwrap();
        assert!(!store.index_path().exists());
    }

    /// Two stores whose records are a year old: one last used a year ago, one loaded
    /// yesterday. Only the one unused for the configured days goes, whole.
    #[test]
    fn prune_unused_removes_a_store_unused_for_n_days_and_keeps_one_used_yesterday() {
        let tmp = TempDir::new("envoy-sessions-prune-unused");
        let config = EnvoyMemoryConfig {
            ttl_hours: 2 * 366 * 24,
            ..enabled()
        };
        let now = t(400 * DAY);
        let year_ago = t(35 * DAY);
        let old = EnvoySessions::open(&tmp.path, "inst-old", &config).unwrap();
        let recent = EnvoySessions::open(&tmp.path, "inst-recent", &config).unwrap();
        for store in [&old, &recent] {
            store
                .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), year_ago)
                .unwrap();
        }
        let yesterday = now - Duration::from_secs(DAY);
        assert!(
            recent
                .load(IDENTITY_A, DEST_A, "thread-one", yesterday)
                .unwrap()
                .is_some()
        );

        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                30,
                now,
                &keep_none(),
                &never
            )
            .unwrap(),
            UnusedStores {
                removed: 1,
                stamped: 0,
                unreadable_stores: 0,
            }
        );

        assert!(
            !old.dir().exists(),
            "the whole store goes, lock and directory"
        );
        assert_eq!(stamp_of(&recent), Some(rfc3339_utc(yesterday)));
        assert_eq!(
            recent
                .load(IDENTITY_A, DEST_A, "thread-one", now)
                .unwrap()
                .unwrap()
                .last_used,
            rfc3339_utc(year_ago),
            "the year-old thread stays"
        );
        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                0,
                t(900 * DAY),
                &keep_none(),
                &never
            )
            .unwrap(),
            UnusedStores::default(),
            "0 days does nothing"
        );
        assert!(recent.dir().exists());
    }

    #[test]
    fn prune_unused_stamps_an_unstamped_store_and_never_deletes_it_on_sight() {
        let tmp = TempDir::new("envoy-sessions-prune-unstamped");
        let config = enabled();
        let store = EnvoySessions::open(&tmp.path, "inst-old", &config).unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        unstamp(&store);
        let orphan_only = stores_dir(&tmp.path).join("inst-orphan");
        fs::create_dir_all(&orphan_only).unwrap();
        fs::write(
            orphan_only.join(format!("{}.yaml", "de".repeat(16))),
            "version: 1\n",
        )
        .unwrap();
        let now = t(400 * DAY);

        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                now,
                &keep_none(),
                &never
            )
            .unwrap(),
            UnusedStores {
                removed: 0,
                stamped: 2,
                unreadable_stores: 0,
            }
        );
        assert_eq!(stamp_of(&store), Some(rfc3339_utc(now)));
        assert!(record_path(&store, IDENTITY_A, "thread-one").exists());
        assert!(orphan_only.join(INDEX_FILE).exists());

        let later = now + Duration::from_secs(DAY);
        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                later,
                &keep_none(),
                &never
            )
            .unwrap()
            .removed,
            0,
            "exactly the days since the stamp is not more than them"
        );
        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                later + Duration::from_secs(1),
                &keep_none(),
                &never
            )
            .unwrap()
            .removed,
            2
        );
        assert!(!store.dir().exists());
        assert!(!orphan_only.exists());
    }

    #[test]
    fn prune_unused_keeps_the_running_stores_and_a_stamp_in_the_future() {
        let tmp = TempDir::new("envoy-sessions-prune-keep");
        let config = enabled();
        let running = EnvoySessions::open(&tmp.path, "inst-running", &config).unwrap();
        running
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let ahead = EnvoySessions::open(&tmp.path, "inst-ahead", &config).unwrap();
        ahead
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(900 * DAY))
            .unwrap();
        let keep = HashSet::from(["inst-running".to_string()]);

        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                t(400 * DAY),
                &keep,
                &never
            )
            .unwrap(),
            UnusedStores::default()
        );
        assert_eq!(stamp_of(&running), Some(rfc3339_utc(t(1_000))));
        assert!(record_path(&running, IDENTITY_A, "thread-one").exists());
        assert!(record_path(&ahead, IDENTITY_A, "thread-one").exists());
    }

    #[test]
    fn prune_unused_leaves_a_store_whose_index_cannot_be_read_and_counts_it() {
        let tmp = TempDir::new("envoy-sessions-prune-unreadable");
        let config = enabled();
        let mut planted = Vec::new();
        for (instance, to) in [
            ("inst-newer", "\"version\":2"),
            ("inst-unknown", "\"version\":1,\"bogus\":1"),
        ] {
            let store = EnvoySessions::open(&tmp.path, instance, &config).unwrap();
            store
                .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
                .unwrap();
            unstamp(&store);
            plant(&store.index_path(), "\"version\":1", to);
            planted.push((
                store,
                fs::read_to_string(store_index(&tmp, instance)).unwrap(),
            ));
        }

        for days in [1, 3_650] {
            assert_eq!(
                EnvoySessions::prune_unused_everywhere(
                    &tmp.path,
                    &config,
                    days,
                    t(400 * DAY),
                    &keep_none(),
                    &never
                )
                .unwrap(),
                UnusedStores {
                    removed: 0,
                    stamped: 0,
                    unreadable_stores: 2,
                }
            );
        }
        for (store, text) in &planted {
            assert_eq!(&fs::read_to_string(store.index_path()).unwrap(), text);
            assert_refused_in_place(store, &store.index_path(), text);
        }
    }

    #[test]
    fn prune_unused_refuses_a_stamp_that_does_not_parse_in_place() {
        let tmp = TempDir::new("envoy-sessions-prune-bad-stamp");
        let config = enabled();
        let store = EnvoySessions::open(&tmp.path, "inst-bad-stamp", &config).unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        plant(&store.index_path(), &rfc3339_utc(t(1_000)), "not a time");
        let text = fs::read_to_string(store.index_path()).unwrap();

        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                t(400 * DAY),
                &keep_none(),
                &never
            )
            .unwrap(),
            UnusedStores {
                removed: 0,
                stamped: 0,
                unreadable_stores: 1,
            }
        );
        assert_eq!(fs::read_to_string(store.index_path()).unwrap(), text);
        assert!(record_path(&store, IDENTITY_A, "thread-one").exists());
    }

    /// A file named like a record that cannot be removed keeps the index, and the store
    /// is counted as unreadable rather than removed.
    #[test]
    fn prune_unused_counts_no_store_removed_while_its_records_remain() {
        let tmp = TempDir::new("envoy-sessions-prune-stuck-record");
        let config = enabled();
        let store = EnvoySessions::open(&tmp.path, "inst-stuck", &config).unwrap();
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let stuck = store.dir().join(format!("{}.yaml", "ef".repeat(16)));
        fs::create_dir(&stuck).unwrap();

        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                t(400 * DAY),
                &keep_none(),
                &never
            )
            .unwrap(),
            UnusedStores {
                removed: 0,
                stamped: 0,
                unreadable_stores: 1,
            }
        );
        assert!(store.index_path().exists());
        assert!(stuck.exists());
    }

    /// A store a sweep listed but another process removed since is not brought back:
    /// neither a missing directory nor one left with no index and no record.
    #[test]
    fn prune_unused_never_recreates_a_store_that_went_after_it_was_listed() {
        let tmp = TempDir::new("envoy-sessions-prune-vanished");
        let config = enabled();
        let gone = stores_dir(&tmp.path).join("inst-gone");
        let listed = EnvoySessions::listed(gone.clone(), &config);
        let unused_for = Some(Duration::from_secs(DAY));

        assert!(listed.remove_if_unused(unused_for, t(400 * DAY)).is_err());
        assert!(!gone.exists());

        fs::create_dir_all(&gone).unwrap();
        assert!(matches!(
            listed.remove_if_unused(unused_for, t(400 * DAY)).unwrap(),
            StoreUse::Gone
        ));
        assert!(!gone.join(INDEX_FILE).exists());
        assert!(
            !gone.exists(),
            "no lock file is left in an emptied directory"
        );
    }

    /// The sweep leaves no instance lock file behind for a store whose session never ran
    /// here, and counts a store whose instance lock cannot be opened as unreadable.
    #[cfg(unix)]
    #[test]
    fn prune_unused_creates_no_instance_lock_and_counts_one_it_cannot_open() {
        let tmp = TempDir::new("envoy-sessions-prune-instance-lock");
        let config = enabled();
        let swept_id = "ab".repeat(16);
        let refused_id = "cd".repeat(16);
        for id in [&swept_id, &refused_id] {
            EnvoySessions::open(&tmp.path, id, &config)
                .unwrap()
                .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
                .unwrap();
        }
        let refused_lock = mesh_cache_dir(&tmp.path).join(format!("{refused_id}.lock"));
        fs::create_dir_all(&refused_lock).unwrap();

        assert_eq!(
            EnvoySessions::prune_unused_everywhere(
                &tmp.path,
                &config,
                1,
                t(400 * DAY),
                &keep_none(),
                &never
            )
            .unwrap(),
            UnusedStores {
                removed: 1,
                stamped: 0,
                unreadable_stores: 1,
            }
        );
        assert!(
            !mesh_cache_dir(&tmp.path)
                .join(format!("{swept_id}.lock"))
                .exists()
        );
        assert!(
            stores_dir(&tmp.path)
                .join(&refused_id)
                .join(INDEX_FILE)
                .exists()
        );
    }

    /// The index is a symlink, which reads but is never written through, so the stamp
    /// cannot be written; the load still hands the record back and says so.
    #[cfg(unix)]
    #[test]
    fn a_load_whose_stamp_cannot_be_written_still_loads_and_warns() {
        install_log_collector();
        let tmp = TempDir::new("envoy-sessions-stamp-unwritable");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, DEST_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let real = tmp.path.join("real-index.json");
        fs::rename(store.index_path(), &real).unwrap();
        std::os::unix::fs::symlink(&real, store.index_path()).unwrap();

        let loaded = store
            .load(IDENTITY_A, DEST_A, "thread-one", t(1_000 + 2 * 3_600))
            .unwrap();

        assert!(loaded.is_some());
        assert_eq!(stamp_of(&store), Some(rfc3339_utc(t(1_000))));
        let warns = warn_snapshot();
        assert!(
            warns
                .iter()
                .any(|message| message.contains("could not be stamped as used")),
            "captured: {warns:#?}"
        );
    }

    fn store_index(tmp: &TempDir, instance: &str) -> PathBuf {
        stores_dir(&tmp.path).join(instance).join(INDEX_FILE)
    }
}
