//! What the envoy remembers of a trusted peer's thread between messages: the peer's turns
//! and the envoy's answers, one record per (proved sender identity, thread), under
//! `<cache_dir>/mesh/envoy-sessions/<instance_id>/`. The owner's own transcript is never
//! in here and neither is a system prompt; a record is only ever handed back to the
//! identity that wrote it. The store exists nowhere while `mesh.envoy_memory.enabled` is
//! false, not even as an empty directory, and every file and directory it does create is
//! readable by its owner alone. `index.json` is the enumeration: what it does not name
//! is not a conversation, and a record it names that is not on disk is forgotten on the
//! next look.

use crate::config::mesh_config::EnvoyMemoryConfig;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::trust::parse_hash;
use crate::mesh::{canonical_hash, mesh_cache_dir, rfc3339_utc, write_atomically_owner_only};

use parking_lot::Mutex;
use rns_transport::hash::{AddressHash, Hash};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::Digest;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(crate) const ENVOY_SESSION_VERSION: u64 = 1;
pub(crate) const ENVOY_SESSION_INDEX_VERSION: u64 = 1;

const STORE_NAME: &str = "envoy memory";
const INDEX_FILE: &str = "index.json";
const RECORD_EXTENSION: &str = "yaml";

/// One `<key>.yaml`: the retained turns of one peer thread. The shape is a stable on-disk
/// record other code reads back; it rejects fields it does not know, so any change to the
/// layout bumps `ENVOY_SESSION_VERSION` and a reader refuses the file on a version it
/// does not write. There is no system slot by construction: what the envoy is told about
/// its owner is composed afresh on every run and never stored beside a peer's words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvoySessionFile {
    pub version: u64,
    /// Lower-hex, the proved identity of the peer whose thread this is; a reader hands
    /// the record to no other.
    pub identity: String,
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
}

/// One conversation as the index knows it: enough to judge every bound without opening
/// the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvoySessionEntry {
    pub key: String,
    pub identity: String,
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
    /// The identity given is not a 32-hex address hash, so it keys nothing.
    NotAnIdentity(String),
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
            Self::NotAnIdentity(text) => write!(
                f,
                "'{text}' is not a 32-hex identity hash; the envoy memory is keyed by one"
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
            | Self::NotAnIdentity(_) => None,
        }
    }
}

/// The file name of one thread's record: the truncated SHA-256 of the identity's address
/// bytes, a zero byte and the thread, as 32 lowercase hex. The zero byte keeps an
/// identity whose thread name happens to extend another's from colliding with it; the
/// identity bytes, not its hex, so the key is the same whatever case the caller spelt.
/// `None` when `identity` is not a 32-hex address hash.
pub(crate) fn session_key(identity: &str, thread: &str) -> Option<String> {
    let identity = parse_hash(identity)?;
    Some(
        AddressHash::new_from_hash(&Hash::new(
            Hash::generator()
                .chain_update(identity.as_slice())
                .chain_update([0u8])
                .chain_update(thread.as_bytes())
                .finalize()
                .into(),
        ))
        .to_hex_string(),
    )
}

/// The conversations one instance's envoy remembers, keyed by instance because a fork
/// answers its own peers and must not read the original's.
pub(crate) struct EnvoySessions {
    dir: PathBuf,
    /// Orders the read-modify-write of every mutation within this process; `file_lock`
    /// does the same across processes.
    write_lock: Mutex<()>,
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
            dir: mesh_cache_dir(cache_dir)
                .join("envoy-sessions")
                .join(instance_id),
            write_lock: Mutex::new(()),
        })
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The record for `identity`'s `thread`, or `None` when the store holds none. A
    /// record the index names but the disk lacks is dropped from the index on the way.
    /// A record under this key that names another identity is refused, never returned.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by the envoy run flow once it retains conversation memory"
        )
    )]
    pub(crate) fn load(
        &self,
        identity: &str,
        thread: &str,
        _now: SystemTime,
    ) -> Result<Option<EnvoySessionFile>, EnvoyMemoryError> {
        let (identity, key) = resolve(identity, thread)?;
        if !self.dir.exists() {
            return Ok(None);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = self.read_index()?;
        let Some(indexed) = self.owned(&index, &key, &identity)? else {
            return Ok(None);
        };
        let Some(record) = indexed.record else {
            index.entries.remove(indexed.position);
            self.write_index(&index)?;
            return Ok(None);
        };
        Ok(Some(record))
    }

    /// Writes `turns` as the whole of `identity`'s `thread`, stamped `now`, creating the
    /// store on the first save. Empty turns delete the record instead of writing an
    /// empty one. A record already under the key that this build cannot read, or that
    /// names another identity, is refused rather than written over.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by the envoy run flow once it retains conversation memory"
        )
    )]
    pub(crate) fn save(
        &self,
        identity: &str,
        thread: &str,
        turns: Vec<EnvoyTurn>,
        now: SystemTime,
    ) -> Result<(), EnvoyMemoryError> {
        let (identity, key) = resolve(identity, thread)?;
        if turns.is_empty() {
            self.delete_where(|entry| entry.key == key)?;
            return Ok(());
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut index = self.read_index()?;
        let position = self
            .owned(&index, &key, &identity)?
            .map(|indexed| indexed.position);
        let record = EnvoySessionFile {
            version: ENVOY_SESSION_VERSION,
            identity,
            thread: thread.to_string(),
            last_used: rfc3339_utc(now),
            turns,
        };
        self.write_record(&key, &record)?;
        let entry = EnvoySessionEntry {
            key: key.clone(),
            identity: record.identity,
            thread: record.thread,
            last_used: record.last_used,
            turns: record.turns.len(),
            bytes: text_bytes(&record.turns),
        };
        match position {
            Some(position) => index.entries[position] = entry,
            None => index.entries.push(entry),
        }
        self.write_index(&index)
    }

    /// `true` when `identity`'s `thread` was there to remove.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by the envoy run flow once it retains conversation memory"
        )
    )]
    pub(crate) fn delete(&self, identity: &str, thread: &str) -> Result<bool, EnvoyMemoryError> {
        let (_, key) = resolve(identity, thread)?;
        Ok(self.delete_where(|entry| entry.key == key)? > 0)
    }

    /// Forgets every thread of `identity`; how many went.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by the envoy run flow once it retains conversation memory"
        )
    )]
    pub(crate) fn delete_identity(&self, identity: &str) -> Result<usize, EnvoyMemoryError> {
        let identity = canonical_hash(identity)
            .ok_or_else(|| EnvoyMemoryError::NotAnIdentity(identity.to_string()))?;
        self.delete_where(|entry| entry.identity == identity)
    }

    /// Removes record files the index does not name. `Ok(0)` without creating anything
    /// while the store does not exist. Returns how many conversations went.
    pub(crate) fn prune(&self, _now: SystemTime) -> Result<usize, EnvoyMemoryError> {
        if !self.dir.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let index = self.read_index()?;
        self.remove_orphans(&index)?;
        Ok(0)
    }

    /// `(conversations, distinct identities)`; `(0, 0)` while the store does not exist.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by the envoy run flow once it retains conversation memory"
        )
    )]
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

    /// Drops every entry `doomed` picks and its record; nothing is created, and the
    /// index is rewritten only when something went.
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

    /// `key`'s entry and record as far as `identity` may see them: `None` when the index
    /// does not name the key, the record `None` when the index names it but the disk
    /// lacks it. An entry or record under the key that names another identity is
    /// refused, never read and never written over.
    fn owned(
        &self,
        index: &EnvoySessionIndex,
        key: &str,
        identity: &str,
    ) -> Result<Option<Indexed>, EnvoyMemoryError> {
        let Some(position) = index.entries.iter().position(|entry| entry.key == key) else {
            return Ok(None);
        };
        let wrong_identity = |found: &str| EnvoyMemoryError::WrongIdentity {
            key: key.to_string(),
            asked: identity.to_string(),
            found: found.to_string(),
        };
        let entry = &index.entries[position];
        if entry.identity != identity {
            return Err(wrong_identity(&entry.identity));
        }
        let record = self.read_record(key)?;
        if let Some(record) = &record
            && record.identity != identity
        {
            return Err(wrong_identity(&record.identity));
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
    /// takes the lock for a store that does not exist yet.
    fn file_lock(&self) -> Result<File, EnvoyMemoryError> {
        create_dirs_owner_only(&self.dir).map_err(|source| EnvoyMemoryError::Io {
            path: self.dir.clone(),
            source,
        })?;
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

    /// The index, empty when the file is not there yet.
    fn read_index(&self) -> Result<EnvoySessionIndex, EnvoyMemoryError> {
        Ok(read_versioned(
            &self.index_path(),
            Format::Json,
            ENVOY_SESSION_INDEX_VERSION,
        )?
        .unwrap_or(EnvoySessionIndex {
            version: ENVOY_SESSION_INDEX_VERSION,
            entries: Vec::new(),
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

    /// `true` when a record file was there to remove.
    fn remove_record(&self, key: &str) -> Result<bool, EnvoyMemoryError> {
        let path = self.record_path(key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(EnvoyMemoryError::Io { path, source }),
        }
    }

    /// Removes every `<key>.yaml` in the directory that `index` does not name: a record
    /// the index does not know is not a conversation, whatever it holds.
    fn remove_orphans(&self, index: &EnvoySessionIndex) -> Result<(), EnvoyMemoryError> {
        let named: HashSet<&str> = index
            .entries
            .iter()
            .map(|entry| entry.key.as_str())
            .collect();
        let io_error = |source| EnvoyMemoryError::Io {
            path: self.dir.clone(),
            source,
        };
        for entry in fs::read_dir(&self.dir).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            let is_record = path.extension().is_some_and(|ext| ext == RECORD_EXTENSION);
            let orphan = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| !named.contains(stem));
            if is_record && orphan {
                fs::remove_file(&path).map_err(|source| EnvoyMemoryError::Io { path, source })?;
            }
        }
        Ok(())
    }
}

/// One key as the index and the disk hold it.
struct Indexed {
    position: usize,
    record: Option<EnvoySessionFile>,
}

/// The canonical identity and the key it and `thread` make, or the refusal for an
/// identity that is not an address hash.
fn resolve(identity: &str, thread: &str) -> Result<(String, String), EnvoyMemoryError> {
    match (canonical_hash(identity), session_key(identity, thread)) {
        (Some(identity), Some(key)) => Ok((identity, key)),
        _ => Err(EnvoyMemoryError::NotAnIdentity(identity.to_string())),
    }
}

fn text_bytes(turns: &[EnvoyTurn]) -> u64 {
    turns.iter().map(|turn| turn.text.len() as u64).sum()
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
    use std::time::Duration;

    const IDENTITY_A: &str = "0123456789abcdef0123456789abcdef";
    const IDENTITY_B: &str = "fedcba9876543210fedcba9876543210";

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
        store.record_path(&session_key(identity, thread).unwrap())
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
            ["version", "identity", "thread", "last_used", "turns"]
        );
        assert!(!yaml.contains("system"), "{yaml}");
        assert!(yaml.contains("role: user\n"), "{yaml}");
        assert!(yaml.contains("role: assistant\n"), "{yaml}");
    }

    #[test]
    fn two_identities_with_the_same_thread_get_different_keys() {
        let a = session_key(IDENTITY_A, "thread-one").unwrap();
        let b = session_key(IDENTITY_B, "thread-one").unwrap();
        assert_ne!(a, b);
        assert_ne!(a, session_key(IDENTITY_A, "thread-two").unwrap());
        assert_eq!(
            a,
            session_key(&IDENTITY_A.to_ascii_uppercase(), "thread-one").unwrap(),
            "the key digests the identity's bytes, not its spelling"
        );
        assert_eq!(session_key("not-a-hash", "thread-one"), None);
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
            store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap(),
            None
        );
        assert!(!store.delete(IDENTITY_A, "thread-one").unwrap());
        assert_eq!(store.delete_identity(IDENTITY_A).unwrap(), 0);
        store
            .save(IDENTITY_A, "thread-one", Vec::new(), t(1_000))
            .unwrap();

        assert!(!tmp.path.join("mesh").exists(), "{:?}", names_in(&tmp.path));
    }

    #[test]
    fn save_writes_under_envoy_sessions_instance_dir_through_the_index_lock() {
        let tmp = TempDir::new("envoy-sessions-layout");
        let store = store(&tmp, &enabled());

        store
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();

        let dir = tmp.path.join("mesh").join("envoy-sessions").join("inst");
        assert_eq!(store.dir(), dir);
        let key = session_key(IDENTITY_A, "thread-one").unwrap();
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
            .load(IDENTITY_A, "thread-one", t(1_000))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.identity, IDENTITY_A);
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
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
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
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        let key = session_key(IDENTITY_A, "thread-one").unwrap();
        let path = store.record_path(&key);
        let planted = fs::read_to_string(&path)
            .unwrap()
            .replace(IDENTITY_A, IDENTITY_B);
        fs::write(&path, planted).unwrap();

        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
        assert!(
            matches!(&err, EnvoyMemoryError::WrongIdentity { key: k, asked, found }
                if *k == key && asked == IDENTITY_A && found == IDENTITY_B),
            "{err:?}"
        );
        assert!(err.to_string().contains("refusing"), "{err}");
        assert_eq!(
            store.load(IDENTITY_B, "thread-one", t(1_000)).unwrap(),
            None,
            "B's key differs, so B sees nothing either"
        );

        let index_path = store.index_path();
        let swapped = fs::read_to_string(&index_path)
            .unwrap()
            .replace(IDENTITY_A, IDENTITY_B);
        fs::write(&index_path, swapped).unwrap();
        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::WrongIdentity { .. }),
            "an index entry under A's key naming B is refused too: {err:?}"
        );
    }

    #[test]
    fn delete_removes_one_thread() {
        let tmp = TempDir::new("envoy-sessions-delete");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, "thread-two", exchange(2), t(1_001))
            .unwrap();

        assert!(store.delete(IDENTITY_A, "thread-one").unwrap());
        assert!(!store.delete(IDENTITY_A, "thread-one").unwrap());

        assert_eq!(
            store.load(IDENTITY_A, "thread-one", t(1_001)).unwrap(),
            None
        );
        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert!(
            store
                .load(IDENTITY_A, "thread-two", t(1_001))
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
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, "thread-two", exchange(2), t(1_001))
            .unwrap();
        store
            .save(IDENTITY_B, "thread-one", exchange(3), t(1_002))
            .unwrap();

        assert_eq!(store.delete_identity(IDENTITY_A).unwrap(), 2);
        assert_eq!(store.delete_identity(IDENTITY_A).unwrap(), 0);

        assert!(!record_path(&store, IDENTITY_A, "thread-one").exists());
        assert!(!record_path(&store, IDENTITY_A, "thread-two").exists());
        assert_eq!(
            store
                .load(IDENTITY_B, "thread-one", t(1_002))
                .unwrap()
                .unwrap()
                .turns,
            exchange(3)
        );
        assert_eq!(store.stats().unwrap(), (1, 1));
    }

    #[test]
    fn stats_counts_conversations_and_distinct_identities() {
        let tmp = TempDir::new("envoy-sessions-stats");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        store
            .save(IDENTITY_A, "thread-two", exchange(2), t(1_001))
            .unwrap();
        store
            .save(IDENTITY_B, "thread-one", exchange(3), t(1_002))
            .unwrap();

        assert_eq!(store.stats().unwrap(), (3, 2));

        store
            .save(IDENTITY_A, "thread-one", exchange(4), t(1_003))
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
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        fs::remove_file(record_path(&store, IDENTITY_A, "thread-one")).unwrap();

        assert_eq!(
            store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap(),
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
                .save(identity, "thread", exchange(1), t(1_000))
                .unwrap_err();
            assert!(matches!(err, EnvoyMemoryError::NotAnIdentity(_)), "{err:?}");
            assert!(store.load(identity, "thread", t(1_000)).is_err());
            assert!(store.delete_identity(identity).is_err());
        }
        assert!(!tmp.path.join("mesh").exists());
    }

    fn planted_store(tag: &str) -> (TempDir, EnvoySessions) {
        let tmp = TempDir::new(tag);
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
            .unwrap();
        (tmp, store)
    }

    fn assert_refused_in_place(store: &EnvoySessions, path: &Path, planted: &str) {
        let err = store
            .load(IDENTITY_A, "thread-one", t(1_000))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("envoy memory"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(
            store
                .save(IDENTITY_A, "thread-one", exchange(2), t(1_001))
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

        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
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
            .load(IDENTITY_A, "thread-one", t(1_000))
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

        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
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

        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
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

        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
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

        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
        assert!(
            matches!(err, EnvoyMemoryError::NotARecord { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("added_later"), "{err}");
        assert_refused_in_place(&store, &path, &planted);

        let mut entry_field = original;
        entry_field["entries"][0]["added_later"] = serde_json::json!(1);
        fs::write(&path, entry_field.to_string()).unwrap();
        let err = store.load(IDENTITY_A, "thread-one", t(1_000)).unwrap_err();
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
            assert!(store.load(IDENTITY_A, "thread-one", t(1_000)).is_err());
            assert!(
                store
                    .save(IDENTITY_A, "thread-one", exchange(2), t(1_001))
                    .is_err()
            );
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), planted);
        assert_eq!(names_in(store.dir()), before, "no sibling appears");
    }

    #[test]
    fn prune_removes_orphan_record_files() {
        let tmp = TempDir::new("envoy-sessions-orphans");
        let store = store(&tmp, &enabled());
        store
            .save(IDENTITY_A, "thread-one", exchange(1), t(1_000))
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
}
