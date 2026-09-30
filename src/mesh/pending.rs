//! Questions this node has asked peers and is still waiting on. A `PendingRecord` is the
//! on-disk half, so a question asked in one Coyote process can be answered in the next;
//! `Correlations` is the in-memory half that matches a reply to its question and wakes
//! whoever is waiting for it. `InboundStore` is the mirror image: questions peers asked
//! this node that the envoy escalated to the person at the keyboard, kept apart so a
//! peer's reply can never be matched against one.

use crate::mesh::message::{PEER_ID_MAX_CHARS, PeerMessage};
use crate::mesh::r3::{redact_hashes, short};
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::{canonical_hash, mesh_cache_dir, parse_rfc3339, write_atomically};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

pub(crate) const PENDING_RECORD_VERSION: u64 = 1;
/// Past this many live records the oldest go, answered ones before open ones: an answer
/// nobody collected is worth less than a question still waiting on one.
pub(crate) const PENDING_MAX_ENTRIES: usize = 256;
/// A question unanswered for this long is forgotten; a reply after that reads as an
/// ordinary message.
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// The store keeps the question's first words only, enough to recognise it in a list.
pub(crate) const PENDING_QUESTION_MAX_CHARS: usize = 280;
/// How long a collect waits for a reply before reporting the question still open.
pub(crate) const DEFAULT_COLLECT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const INBOUND_RECORD_VERSION: u64 = 1;
/// Past this many escalated questions the oldest go; expiry reuses `PENDING_TTL`.
pub(crate) const INBOUND_MAX_ENTRIES: usize = 256;
/// What the envoy asked the human is kept whole, so a late `.mesh answer` still shows
/// the question as it was put.
pub(crate) const INBOUND_ENVOY_QUESTION_MAX_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingState {
    Open,
    Answered,
}

/// One line of `pending-<instance_id>.jsonl`. The shape is a stable on-disk record other
/// code reads back. It rejects fields it does not know, so any change to the layout, a
/// field added included, bumps `PENDING_RECORD_VERSION` and a reader refuses the whole
/// file on a version it does not write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingRecord {
    pub version: u64,
    /// The message id of the question, which the reply names in `in_reply_to`.
    pub id: String,
    /// Lower-hex, the instance the question went to.
    pub peer_destination: String,
    /// Lower-hex, the identity behind that instance when the question was sent.
    pub peer_identity: String,
    /// The question's first words, at most `PENDING_QUESTION_MAX_CHARS`.
    pub question: String,
    /// RFC 3339 UTC seconds.
    pub sent_at: String,
    /// RFC 3339 UTC seconds: when the asker stopped waiting for the first answer.
    pub timeout_at: String,
    pub state: PendingState,
    /// The answer, once it has come and until it is collected, so a reply nobody read
    /// before the process ended is still there for the next one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<PeerMessage>,
}

/// How a store's errors name itself and one of its lines.
#[derive(Clone, Copy)]
struct StoreNames {
    store: &'static str,
    record: &'static str,
}

const PENDING_NAMES: StoreNames = StoreNames {
    store: "pending store",
    record: "pending question",
};

/// Open questions, newest first, in `<cache_dir>/mesh/pending-<instance_id>.jsonl`. Keyed
/// by instance because a fork asks its own questions and must not collect the original's.
/// Every read parses the whole file and refuses it on the first bad line: a partial list
/// would drop a question silently, and the file is cache, so the remedy is to move it aside.
pub(crate) struct PendingStore {
    path: PathBuf,
    /// Orders the read-modify-write of every mutation within this process; `file_lock`
    /// does the same across processes.
    write_lock: Mutex<()>,
}

impl PendingStore {
    pub(crate) fn new(cache_dir: &Path, instance_id: &str) -> Self {
        Self {
            path: mesh_cache_dir(cache_dir).join(format!("pending-{instance_id}.jsonl")),
            write_lock: Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Newest first, with expired questions left out; the file is not touched.
    #[cfg(test)]
    pub(crate) fn list(&self, now: SystemTime) -> Result<Vec<PendingRecord>> {
        let mut records = self.read_all()?;
        records.retain(|record| !is_expired(record, now));
        Ok(records)
    }

    /// Adds `record` or replaces the record with its id in place. Refuses, with the file
    /// untouched, anything `read_all` would refuse on the way back.
    pub(crate) fn upsert(&self, record: PendingRecord, now: SystemTime) -> Result<()> {
        if record.version != PENDING_RECORD_VERSION {
            bail!(
                "A pending question is version {} but this Coyote writes version {PENDING_RECORD_VERSION}; refusing to store it.",
                record.version
            );
        }
        if record.id.is_empty() || record.id.chars().count() > PEER_ID_MAX_CHARS {
            bail!(
                "A pending question's id is empty or longer than {PEER_ID_MAX_CHARS} characters; refusing to store it."
            );
        }
        for (field, text) in [
            ("sent_at", &record.sent_at),
            ("timeout_at", &record.timeout_at),
        ] {
            if parse_rfc3339(text).is_none() {
                bail!(
                    "A pending question's `{field}` is not an RFC 3339 timestamp; refusing to store it."
                );
            }
        }
        for (field, hash) in [
            ("peer_destination", &record.peer_destination),
            ("peer_identity", &record.peer_identity),
        ] {
            if canonical_hash(hash).as_deref() != Some(hash.as_str()) {
                bail!(
                    "A pending question's {field} is not 32 lowercase hex characters; refusing to store it."
                );
            }
        }
        if record.question.chars().count() > PENDING_QUESTION_MAX_CHARS {
            bail!(
                "A pending question is longer than {PENDING_QUESTION_MAX_CHARS} characters; refusing to store it."
            );
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        match records.iter_mut().find(|existing| existing.id == record.id) {
            Some(existing) => *existing = record,
            None => records.insert(0, record),
        }
        evict(&mut records, now);
        self.write_all(&records)
    }

    /// `true` when a record with `id` was there to remove.
    pub(crate) fn remove(&self, id: &str) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let before = records.len();
        records.retain(|record| record.id != id);
        if records.len() == before {
            return Ok(false);
        }
        self.write_all(&records)?;
        Ok(true)
    }

    /// Drops expired questions and any over the cap; writes only if any went.
    #[cfg(test)]
    pub(crate) fn prune(&self, now: SystemTime) -> Result<usize> {
        // Nothing to prune means nothing to lock: taking the file lock would create the
        // cache directory for a store that does not exist yet.
        if !self.path.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let removed = evict(&mut records, now);
        if removed > 0 {
            self.write_all(&records)?;
        }
        Ok(removed)
    }

    /// The questions this store still has something to do with, newest first, after
    /// pruning: the open ones and the answered ones whose reply is on the line, waiting
    /// to be collected. An answered record without a reply was written before replies
    /// were persisted; nothing can be shown for it, so it is dropped from the file in the
    /// same pass. The file is left as it was when anything fails.
    pub(crate) fn load_pending(&self, now: SystemTime) -> Result<Vec<PendingRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let before = records.len();
        evict(&mut records, now);
        records.retain(|record| record.state == PendingState::Open || record.reply.is_some());
        if records.len() != before {
            self.write_all(&records)?;
        }
        Ok(records)
    }

    fn file_lock(&self) -> Result<File> {
        file_lock(&self.path, PENDING_NAMES)
    }

    fn read_all(&self) -> Result<Vec<PendingRecord>> {
        read_jsonl(
            &self.path,
            PENDING_NAMES,
            PENDING_RECORD_VERSION,
            |record: &PendingRecord| ("sent_at", &record.sent_at),
        )
    }

    fn write_all(&self, records: &[PendingRecord]) -> Result<()> {
        write_jsonl(&self.path, records, PENDING_NAMES)
    }
}

fn is_expired(record: &PendingRecord, now: SystemTime) -> bool {
    is_stale(&record.sent_at, now)
}

/// Expired first, then past `PENDING_MAX_ENTRIES` the oldest answered records, then the
/// oldest open ones; `records` is newest first. Returns how many went.
fn evict(records: &mut Vec<PendingRecord>, now: SystemTime) -> usize {
    let before = records.len();
    records.retain(|record| !is_expired(record, now));
    let mut over = records.len().saturating_sub(PENDING_MAX_ENTRIES);
    let mut index = records.len();
    while over > 0 && index > 0 {
        index -= 1;
        if records[index].state == PendingState::Answered {
            records.remove(index);
            over -= 1;
        }
    }
    records.truncate(records.len() - over);
    before - records.len()
}

/// One line of `inbound-<instance_id>.jsonl`: a question a peer asked that the envoy
/// could not answer on its own, waiting on the person at the keyboard. The same on-disk
/// discipline as `PendingRecord`: unknown fields are rejected and any layout change bumps
/// `INBOUND_RECORD_VERSION`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboundRecord {
    pub version: u64,
    /// The peer's message id, which the answer names in `in_reply_to`.
    pub id: String,
    /// Lower-hex, the instance that asked.
    pub peer_destination: String,
    /// Lower-hex, the identity behind that instance when it asked.
    pub peer_identity: String,
    /// The question's first words, at most `PENDING_QUESTION_MAX_CHARS`.
    pub question: String,
    /// What the envoy asked the human about the peer's question, empty when it asked
    /// nothing of its own.
    pub envoy_question: String,
    /// RFC 3339 UTC seconds.
    pub received_at: String,
}

const INBOUND_NAMES: StoreNames = StoreNames {
    store: "inbound store",
    record: "inbound question",
};

/// Escalated inbound questions, newest first, in
/// `<cache_dir>/mesh/inbound-<instance_id>.jsonl`. Kept apart from `PendingStore` so
/// `Correlations` never sees these ids: a peer reply naming one must not read as the
/// answer to a question of ours. Same file discipline as `PendingStore`.
pub(crate) struct InboundStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl InboundStore {
    pub(crate) fn new(cache_dir: &Path, instance_id: &str) -> Self {
        Self {
            path: mesh_cache_dir(cache_dir).join(format!("inbound-{instance_id}.jsonl")),
            write_lock: Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Adds `record` or replaces the record with its id in place, evicting the expired and
    /// the oldest over the cap in the same write. Refuses, with the file untouched,
    /// anything the reader would refuse on the way back, and a record whose id another
    /// peer's open question already carries: an answer is routed by id, so a second
    /// peer reusing one could otherwise have the human's answer sent to it.
    pub(crate) fn upsert(&self, record: InboundRecord, now: SystemTime) -> Result<()> {
        if record.version != INBOUND_RECORD_VERSION {
            bail!(
                "An inbound question is version {} but this Coyote writes version {INBOUND_RECORD_VERSION}; refusing to store it.",
                record.version
            );
        }
        if record.id.is_empty() || record.id.chars().count() > PEER_ID_MAX_CHARS {
            bail!(
                "An inbound question's id is empty or longer than {PEER_ID_MAX_CHARS} characters; refusing to store it."
            );
        }
        if parse_rfc3339(&record.received_at).is_none() {
            bail!(
                "An inbound question's `received_at` is not an RFC 3339 timestamp; refusing to store it."
            );
        }
        for (field, hash) in [
            ("peer_destination", &record.peer_destination),
            ("peer_identity", &record.peer_identity),
        ] {
            if canonical_hash(hash).as_deref() != Some(hash.as_str()) {
                bail!(
                    "An inbound question's {field} is not 32 lowercase hex characters; refusing to store it."
                );
            }
        }
        if record.question.chars().count() > PENDING_QUESTION_MAX_CHARS {
            bail!(
                "An inbound question is longer than {PENDING_QUESTION_MAX_CHARS} characters; refusing to store it."
            );
        }
        if record.envoy_question.chars().count() > INBOUND_ENVOY_QUESTION_MAX_CHARS {
            bail!(
                "An inbound question's envoy question is longer than {INBOUND_ENVOY_QUESTION_MAX_CHARS} characters; refusing to store it."
            );
        }
        let _guard = self.write_lock.lock();
        let _file_lock = file_lock(&self.path, INBOUND_NAMES)?;
        let mut records = self.read_all()?;
        records.retain(|record| !is_stale(&record.received_at, now));
        match records.iter_mut().find(|existing| existing.id == record.id) {
            Some(existing) if existing.peer_destination != record.peer_destination => bail!(
                "An open question with id {} belongs to another peer; refusing to store it.",
                record.id
            ),
            Some(existing) => *existing = record,
            None => records.insert(0, record),
        }
        records.truncate(INBOUND_MAX_ENTRIES);
        write_jsonl(&self.path, &records, INBOUND_NAMES)
    }

    /// The record with `id`, expired or not; the file is not touched.
    pub(crate) fn get(&self, id: &str) -> Result<Option<InboundRecord>> {
        Ok(self.read_all()?.into_iter().find(|record| record.id == id))
    }

    /// `true` when a record with `id` was there to remove.
    pub(crate) fn remove(&self, id: &str) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = file_lock(&self.path, INBOUND_NAMES)?;
        let mut records = self.read_all()?;
        let before = records.len();
        records.retain(|record| record.id != id);
        if records.len() == before {
            return Ok(false);
        }
        write_jsonl(&self.path, &records, INBOUND_NAMES)?;
        Ok(true)
    }

    /// Newest first, with expired questions left out; the file is not touched.
    pub(crate) fn list(&self, now: SystemTime) -> Result<Vec<InboundRecord>> {
        let mut records = self.read_all()?;
        records.retain(|record| !is_stale(&record.received_at, now));
        Ok(records)
    }

    /// Copies `other`'s unexpired questions into this store under one lock and one write,
    /// leaving ids already here as they are; `other` is not touched. Returns how many
    /// were copied.
    pub(crate) fn adopt_from(&self, other: &InboundStore, now: SystemTime) -> Result<usize> {
        let incoming = other.list(now)?;
        if incoming.is_empty() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = file_lock(&self.path, INBOUND_NAMES)?;
        let mut records = self.read_all()?;
        records.retain(|record| !is_stale(&record.received_at, now));
        let adopted: Vec<InboundRecord> = incoming
            .into_iter()
            .filter(|record| !records.iter().any(|existing| existing.id == record.id))
            .collect();
        let count = adopted.len();
        if count == 0 {
            return Ok(0);
        }
        records.extend(adopted);
        records.sort_by(|a, b| b.received_at.cmp(&a.received_at));
        records.truncate(INBOUND_MAX_ENTRIES);
        write_jsonl(&self.path, &records, INBOUND_NAMES)?;
        Ok(count)
    }

    fn read_all(&self) -> Result<Vec<InboundRecord>> {
        read_jsonl(
            &self.path,
            INBOUND_NAMES,
            INBOUND_RECORD_VERSION,
            |record: &InboundRecord| ("received_at", &record.received_at),
        )
    }
}

/// Whether `stamp`, an RFC 3339 time the reader has already accepted, is `PENDING_TTL`
/// or more before `now`. A stamp in the future (clock stepped back) reads as just made.
fn is_stale(stamp: &str, now: SystemTime) -> bool {
    parse_rfc3339(stamp).is_some_and(|at| now.duration_since(at).unwrap_or_default() >= PENDING_TTL)
}

/// An exclusive lock on `<path>.lock`, held until the returned `File` drops. Every
/// Coyote process of one identity shares the cache directory, and `write_atomically`
/// renames over the file itself, so the lock lives on a sibling that is never replaced.
fn file_lock(path: &Path, names: StoreNames) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
    }
    let lock_path = path.with_added_extension("lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "Failed to open mesh {} lock '{}'",
                names.store,
                lock_path.display()
            )
        })?;
    file.lock().with_context(|| {
        format!(
            "Failed to lock mesh {} lock '{}'",
            names.store,
            lock_path.display()
        )
    })?;
    Ok(file)
}

/// Every record in the file, refusing the whole file on the first line that is not one
/// of `version`. `stamp` names the record's timestamp field and its text, so a record
/// whose time does not parse is refused here rather than read as never expiring.
fn read_jsonl<T: DeserializeOwned>(
    path: &Path,
    names: StoreNames,
    version: u64,
    stamp: impl Fn(&T) -> (&'static str, &str),
) -> Result<Vec<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to read mesh {} '{}'", names.store, path.display())
            });
        }
    };
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let not_a_record = || {
            format!(
                "Mesh {} '{}' line {} is not a {}. {}",
                names.store,
                path.display(),
                index + 1,
                names.record,
                Remedy::Cache.sentence()
            )
        };
        // The version is read on its own first so a record from a newer Coyote is
        // named as such rather than failing on whatever field the newer layout added.
        let probe: VersionProbe = serde_json::from_str(line).with_context(|| {
            unversioned_refusal(names.store, path, Some(index + 1), version, Remedy::Cache)
        })?;
        if probe.version != version {
            bail!(version_refusal(
                names.store,
                path,
                Some(index + 1),
                probe.version,
                version,
                Remedy::Cache
            ));
        }
        let record: T = serde_json::from_str(line).with_context(not_a_record)?;
        let (field, at) = stamp(&record);
        if parse_rfc3339(at).is_none() {
            bail!(
                "Mesh {} '{}' line {} has a `{field}` that is not an RFC 3339 timestamp. {}",
                names.store,
                path.display(),
                index + 1,
                Remedy::Cache.sentence()
            );
        }
        records.push(record);
    }
    Ok(records)
}

fn write_jsonl<T: Serialize>(path: &Path, records: &[T], names: StoreNames) -> Result<()> {
    let mut text = String::new();
    for record in records {
        text.push_str(
            &serde_json::to_string(record)
                .with_context(|| format!("Failed to serialize a mesh {}", names.record))?,
        );
        text.push('\n');
    }
    write_atomically(path, text.as_bytes())
}

/// One question and, once it has come, its answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Correlation {
    pub record: PendingRecord,
    pub reply: Option<PeerMessage>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WaitOutcome {
    Replied(Box<PeerMessage>),
    /// The question is known and still unanswered when the wait ran out.
    Pending,
    /// No question with that id is open or awaiting collection.
    Unknown,
}

#[derive(Default)]
struct CorrelationState {
    store: Option<PendingStore>,
    entries: HashMap<String, Correlation>,
}

#[cfg(test)]
type WaitHook = Box<dyn FnOnce(&Correlations) + Send>;

/// The questions in flight, matched to replies by `in_reply_to`. Writes go through to the
/// attached store on the caller's thread; the request path hands its delivery to a
/// blocking thread first, so the file write never sits on the server's request loop. The
/// file is a few lines and the write is the only way the question survives the process. A
/// store write that fails is logged and the in-memory state still moves on, so a full
/// disk never loses a reply that has already arrived.
#[derive(Default)]
pub(crate) struct Correlations {
    state: Mutex<CorrelationState>,
    /// Signalled on every change to an entry, so `wait` re-reads rather than polls.
    changed: Notify,
    /// Runs once inside `wait`, between its state read and its await: the one place a
    /// wakeup could be lost, which no scheduling from outside can reach on purpose.
    #[cfg(test)]
    between_read_and_await: Mutex<Option<WaitHook>>,
}

impl Correlations {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `PendingStore::load_pending` then `adopt`, in one call.
    #[cfg(test)]
    pub(crate) fn attach_store(&self, store: PendingStore, now: SystemTime) -> Result<usize> {
        let pending = store.load_pending(now)?;
        let loaded = pending.len();
        self.adopt(store, pending);
        Ok(loaded)
    }

    /// Makes `pending` the questions in flight, answered ones with their reply ready for
    /// `take_answer`, and `store` the file every later write goes to, in one step under
    /// the lock; whatever the previous store left in memory goes, since those questions
    /// belong to its instance. `pending` is what `store.load_pending` returned, or
    /// nothing when the file could not be read: the node then serves with no questions
    /// pending and a late reply lands as an ordinary message.
    pub(crate) fn adopt(&self, store: PendingStore, pending: Vec<PendingRecord>) {
        let mut state = self.state.lock();
        state.entries = pending
            .into_iter()
            .map(|record| {
                let reply = record.reply.clone();
                (record.id.clone(), Correlation { record, reply })
            })
            .collect();
        state.store = Some(store);
        drop(state);
        self.changed.notify_waiters();
    }

    /// Forgets the store and every question with it, for a node that has stopped: the
    /// records stay on disk for the next install of that instance to reopen.
    pub(crate) fn detach_store(&self) {
        let mut state = self.state.lock();
        state.store = None;
        state.entries.clear();
        drop(state);
        self.changed.notify_waiters();
    }

    /// Files a question. The record is written to the store first so a failure there
    /// leaves nothing waiting in memory for a reply the next process would not know.
    pub(crate) fn open(&self, record: PendingRecord) -> Result<()> {
        let mut state = self.state.lock();
        if let Some(store) = &state.store {
            store.upsert(record.clone(), SystemTime::now())?;
        }
        state.entries.insert(
            record.id.clone(),
            Correlation {
                record,
                reply: None,
            },
        );
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self, id: &str) -> bool {
        self.state
            .lock()
            .entries
            .get(id)
            .is_some_and(|entry| entry.record.state == PendingState::Open)
    }

    /// Whether a reply naming `id` from `identity` is the answer this node is still
    /// waiting for: the same test `answer` applies, so admission and correlation agree.
    pub(crate) fn accepts_reply_from(&self, id: &str, identity: &str) -> bool {
        self.state.lock().entries.get(id).is_some_and(|entry| {
            entry.record.state == PendingState::Open
                && entry.record.peer_identity.eq_ignore_ascii_case(identity)
        })
    }

    /// `true` when `in_reply_to` named an open question asked of `reply`'s identity, which
    /// `reply` now answers, on disk too so it survives to the next process uncollected. A
    /// reply from any other identity, or a second reply to the same question, does not
    /// match: it is an ordinary message.
    pub(crate) fn answer(&self, in_reply_to: &str, reply: PeerMessage) -> bool {
        let mut state = self.state.lock();
        let CorrelationState { store, entries } = &mut *state;
        let Some(entry) = entries.get_mut(in_reply_to) else {
            return false;
        };
        if entry.record.state != PendingState::Open {
            debug!(
                "Mesh reply {} from {} names question {in_reply_to}, which is already answered; treating it as a message",
                reply.message_id,
                short(&reply.source_identity)
            );
            return false;
        }
        if !entry
            .record
            .peer_identity
            .eq_ignore_ascii_case(&reply.source_identity)
        {
            debug!(
                "Mesh reply {} from {} names question {in_reply_to}, which was asked of {}; treating it as a message",
                reply.message_id,
                short(&reply.source_identity),
                short(&entry.record.peer_identity)
            );
            return false;
        }
        entry.record.state = PendingState::Answered;
        entry.record.reply = Some(reply.clone());
        entry.reply = Some(reply);
        if let Some(store) = store
            && let Err(err) = store.upsert(entry.record.clone(), SystemTime::now())
        {
            warn!(
                "Mesh question {in_reply_to} was answered but the answer could not be recorded on disk: {}",
                redact_hashes(&format!("{err:#}"))
            );
        }
        drop(state);
        self.changed.notify_waiters();
        true
    }

    /// Waits up to `timeout` for the reply to `id`. Nothing is removed: the reply is
    /// collected with `take_answer`, so a wait that is cancelled or times out loses nothing.
    pub(crate) async fn wait(&self, id: &str, timeout: Duration) -> WaitOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Enabled before the state is read, so an answer landing between the read and
            // the await is a wakeup rather than a full timeout whichever way `Notify` is
            // signalled.
            let mut changed = std::pin::pin!(self.changed.notified());
            changed.as_mut().enable();
            match self.get(id) {
                None => return WaitOutcome::Unknown,
                Some(Correlation {
                    reply: Some(reply), ..
                }) => return WaitOutcome::Replied(Box::new(reply)),
                Some(_) =>
                {
                    #[cfg(test)]
                    if let Some(hook) = self.between_read_and_await.lock().take() {
                        hook(self);
                    }
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return WaitOutcome::Pending;
            }
        }
    }

    /// Removes an answered question and returns its reply; `None` leaves an unanswered
    /// question in place.
    pub(crate) fn take_answer(&self, id: &str) -> Option<PeerMessage> {
        let mut state = self.state.lock();
        let CorrelationState { store, entries } = &mut *state;
        let reply = entries.get(id)?.reply.as_ref()?.clone();
        entries.remove(id);
        if let Some(store) = store
            && let Err(err) = store.remove(id)
        {
            warn!(
                "Mesh question {id} was collected but could not be removed from disk: {}",
                redact_hashes(&format!("{err:#}"))
            );
        }
        drop(state);
        self.changed.notify_waiters();
        Some(reply)
    }

    /// Forgets an open question whose send failed, so nothing waits on a reply that was
    /// never asked for. `true` when an open record with `id` was there to remove; an
    /// answered one stays for `take_answer`.
    pub(crate) fn abandon(&self, id: &str) -> bool {
        let mut state = self.state.lock();
        let CorrelationState { store, entries } = &mut *state;
        if !entries
            .get(id)
            .is_some_and(|entry| entry.record.state == PendingState::Open)
        {
            return false;
        }
        entries.remove(id);
        if let Some(store) = store
            && let Err(err) = store.remove(id)
        {
            warn!(
                "Mesh question {id} was abandoned but could not be removed from disk: {}",
                redact_hashes(&format!("{err:#}"))
            );
        }
        drop(state);
        self.changed.notify_waiters();
        true
    }

    pub(crate) fn get(&self, id: &str) -> Option<Correlation> {
        self.state.lock().entries.get(id).cloned()
    }

    /// Every question in flight, newest first by `sent_at`.
    pub(crate) fn list(&self) -> Vec<Correlation> {
        let mut entries: Vec<Correlation> = self.state.lock().entries.values().cloned().collect();
        entries.sort_by(|a, b| b.record.sent_at.cmp(&a.record.sent_at));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::message::{PeerKind, PeerVia};
    use crate::mesh::test_support::TempDir;
    use crate::mesh::{hex_lower, rfc3339_utc};

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn record(id: &str, sent_at: SystemTime, state: PendingState) -> PendingRecord {
        PendingRecord {
            version: PENDING_RECORD_VERSION,
            id: id.to_string(),
            peer_destination: hex_lower(&[0xab; 16]),
            peer_identity: hex_lower(&[0xcd; 16]),
            question: format!("question {id}"),
            sent_at: rfc3339_utc(sent_at),
            timeout_at: rfc3339_utc(sent_at + DEFAULT_COLLECT_TIMEOUT),
            state,
            reply: None,
        }
    }

    fn reply_to(id: &str) -> PeerMessage {
        PeerMessage {
            source_identity: hex_lower(&[0xcd; 16]),
            source_destination: hex_lower(&[0xab; 16]),
            destination: hex_lower(&[0x01; 16]),
            title: None,
            content: format!("answer to {id}"),
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: format!("reply-{id}"),
            in_reply_to: Some(id.to_string()),
            kind: PeerKind::Reply,
            via: PeerVia::Direct,
        }
    }

    fn ids(records: &[PendingRecord]) -> Vec<&str> {
        records.iter().map(|record| record.id.as_str()).collect()
    }

    #[test]
    fn pending_store_survives_reopen_and_prunes_by_ttl_and_cap() {
        let tmp = TempDir::new("pending-reopen");
        let base = 1_000_000;
        {
            let store = PendingStore::new(&tmp.path, "inst-a");
            assert!(store.list(t(base)).unwrap().is_empty());
            assert!(!tmp.path.join("mesh").exists(), "listing creates nothing");
            store
                .upsert(record("old", t(base - 10), PendingState::Open), t(base))
                .unwrap();
            store
                .upsert(record("new", t(base), PendingState::Open), t(base))
                .unwrap();
            let mut answered = record("old", t(base - 10), PendingState::Open);
            answered.state = PendingState::Answered;
            store.upsert(answered, t(base)).unwrap();
        }

        let store = PendingStore::new(&tmp.path, "inst-a");
        assert_eq!(
            store.path(),
            tmp.path.join("mesh").join("pending-inst-a.jsonl")
        );
        let listed = store.list(t(base)).unwrap();
        assert_eq!(
            ids(&listed),
            vec!["new", "old"],
            "an upsert keeps its place"
        );
        assert_eq!(listed[1].state, PendingState::Answered);
        assert!(
            PendingStore::new(&tmp.path, "inst-b")
                .list(t(base))
                .unwrap()
                .is_empty(),
            "another instance has its own file"
        );

        let expiry = t(base - 10) + PENDING_TTL;
        assert_eq!(ids(&store.list(expiry).unwrap()), vec!["new"]);
        assert_eq!(store.prune(expiry).unwrap(), 1);
        assert_eq!(store.prune(expiry).unwrap(), 0);
        assert_eq!(ids(&store.list(expiry).unwrap()), vec!["new"]);
        assert!(store.remove("new").unwrap());
        assert!(!store.remove("new").unwrap());
        assert!(store.list(t(base)).unwrap().is_empty());

        let mut records: Vec<PendingRecord> = (0..PENDING_MAX_ENTRIES + 2)
            .rev()
            .map(|n| {
                let state = if n % 2 == 0 {
                    PendingState::Answered
                } else {
                    PendingState::Open
                };
                record(&format!("q{n}"), t(base + n as u64), state)
            })
            .collect();
        records.push(record(
            "stale",
            t(base - 3_600 * 24 * 8),
            PendingState::Open,
        ));
        store.write_all(&records).unwrap();

        assert_eq!(store.prune(t(base + 10_000)).unwrap(), 3);
        let listed = store.list(t(base + 10_000)).unwrap();
        assert_eq!(listed.len(), PENDING_MAX_ENTRIES);
        assert!(!ids(&listed).contains(&"stale"), "expired goes first");
        assert!(!ids(&listed).contains(&"q0"), "then the oldest answered");
        assert!(!ids(&listed).contains(&"q2"));
        assert!(ids(&listed).contains(&"q1"), "the oldest open one survives");
        assert_eq!(listed[0].id, format!("q{}", PENDING_MAX_ENTRIES + 1));
    }

    #[test]
    fn upsert_refuses_records_the_reader_would_refuse() {
        let tmp = TempDir::new("pending-refuse");
        let store = PendingStore::new(&tmp.path, "inst");
        let base = record("fine", t(1_000), PendingState::Open);
        for (what, broken, needle) in [
            (
                "version",
                PendingRecord {
                    version: PENDING_RECORD_VERSION + 1,
                    ..base.clone()
                },
                "version",
            ),
            (
                "empty id",
                PendingRecord {
                    id: String::new(),
                    ..base.clone()
                },
                "id",
            ),
            (
                "bad sent_at",
                PendingRecord {
                    sent_at: "yesterday".into(),
                    ..base.clone()
                },
                "RFC 3339",
            ),
            (
                "bad destination",
                PendingRecord {
                    peer_destination: "AB".repeat(16),
                    ..base.clone()
                },
                "peer_destination",
            ),
            (
                "long question",
                PendingRecord {
                    question: "q".repeat(PENDING_QUESTION_MAX_CHARS + 1),
                    ..base.clone()
                },
                "characters",
            ),
        ] {
            let err = store.upsert(broken, t(1_000)).unwrap_err().to_string();
            assert!(err.contains("refusing"), "{what}: {err}");
            assert!(err.contains(needle), "{what}: {err}");
        }
        assert!(!store.path().exists());
    }

    #[test]
    fn a_newer_pending_line_refuses_the_whole_store_and_surfaces_no_record() {
        let tmp = TempDir::new("pending-newer-whole-store");
        let store = PendingStore::new(&tmp.path, "inst");
        store
            .upsert(record("fine", t(1_000), PendingState::Open), t(1_000))
            .unwrap();
        let mut newer =
            serde_json::to_value(record("future", t(2_000), PendingState::Open)).unwrap();
        newer["version"] = serde_json::json!(PENDING_RECORD_VERSION + 1);
        newer["future_field"] = serde_json::json!(1);
        let existing = fs::read_to_string(store.path()).unwrap();
        fs::write(store.path(), format!("{existing}{newer}\n")).unwrap();

        let err = store.list(t(2_000)).unwrap_err().to_string();

        assert!(err.contains(&store.path().display().to_string()), "{err}");
        assert!(err.contains("pending store"), "{err}");
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("version 2"), "{err}");
        assert!(err.contains("version 1"), "{err}");
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(store.load_pending(t(2_000)).is_err());
        assert!(
            store
                .upsert(record("more", t(3_000), PendingState::Open), t(3_000))
                .is_err()
        );
        assert!(
            fs::read_to_string(store.path())
                .unwrap()
                .contains("future_field"),
            "a refused file is left where it is"
        );
    }

    #[test]
    fn a_pre_baseline_pending_line_refuses_as_having_no_migration() {
        let tmp = TempDir::new("pending-older");
        let store = PendingStore::new(&tmp.path, "inst");
        let mut older = serde_json::to_value(record("q1", t(1_000), PendingState::Open)).unwrap();
        older["version"] = serde_json::json!(0);
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{older}\n")).unwrap();

        let err = store.list(t(1_000)).unwrap_err().to_string();

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("version 0"), "{err}");
        assert!(err.contains("no migration"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(!err.contains("upgrade Coyote"), "{err}");
    }

    #[test]
    fn a_pending_line_without_a_version_refuses_the_whole_store() {
        let tmp = TempDir::new("pending-unversioned");
        let store = PendingStore::new(&tmp.path, "inst");
        let mut bare = serde_json::to_value(record("q1", t(1_000), PendingState::Open)).unwrap();
        bare.as_object_mut().unwrap().remove("version");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{bare}\n")).unwrap();

        let err = format!("{:#}", store.list(t(1_000)).unwrap_err());

        assert!(err.contains(&store.path().display().to_string()), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn a_record_with_a_field_this_coyote_does_not_know_is_refused() {
        let tmp = TempDir::new("pending-unknown-field");
        let store = PendingStore::new(&tmp.path, "inst");
        let mut newer = serde_json::to_value(record("q1", t(1_000), PendingState::Open)).unwrap();
        newer["added_later"] = serde_json::json!({"nested": true});
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{newer}\n")).unwrap();

        let err = format!("{:#}", store.list(t(1_000)).unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("not a pending question"), "{err}");
        assert!(err.contains("added_later"), "{err}");
    }

    #[test]
    fn a_reply_with_a_field_this_coyote_does_not_know_refuses_the_store() {
        let tmp = TempDir::new("pending-unknown-reply-field");
        let store = PendingStore::new(&tmp.path, "inst");
        let answered = PendingRecord {
            reply: Some(reply_to("q1")),
            ..record("q1", t(1_000), PendingState::Answered)
        };
        let mut newer = serde_json::to_value(answered).unwrap();
        newer["reply"]["later"] = serde_json::json!(1);
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{newer}\n")).unwrap();

        let err = format!("{:#}", store.list(t(1_000)).unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("later"), "{err}");
    }

    fn inbound(id: &str, received_at: SystemTime) -> InboundRecord {
        InboundRecord {
            version: INBOUND_RECORD_VERSION,
            id: id.to_string(),
            peer_destination: hex_lower(&[0xab; 16]),
            peer_identity: hex_lower(&[0xcd; 16]),
            question: format!("question {id}"),
            envoy_question: format!("proposal {id}"),
            received_at: rfc3339_utc(received_at),
        }
    }

    fn inbound_ids(records: &[InboundRecord]) -> Vec<&str> {
        records.iter().map(|record| record.id.as_str()).collect()
    }

    #[test]
    fn inbound_store_survives_reopen_and_prunes_by_ttl_and_cap() {
        let tmp = TempDir::new("inbound-reopen");
        let base = 1_000_000;
        {
            let store = InboundStore::new(&tmp.path, "inst-a");
            assert!(store.list(t(base)).unwrap().is_empty());
            assert!(store.get("old").unwrap().is_none());
            assert!(!tmp.path.join("mesh").exists(), "reading creates nothing");
            store.upsert(inbound("old", t(base - 10)), t(base)).unwrap();
            store.upsert(inbound("new", t(base)), t(base)).unwrap();
            let mut proposed = inbound("old", t(base - 10));
            proposed.envoy_question = "second thoughts".into();
            store.upsert(proposed, t(base)).unwrap();
        }

        let store = InboundStore::new(&tmp.path, "inst-a");
        assert_eq!(
            store.path(),
            tmp.path.join("mesh").join("inbound-inst-a.jsonl")
        );
        let listed = store.list(t(base)).unwrap();
        assert_eq!(
            inbound_ids(&listed),
            vec!["new", "old"],
            "an upsert keeps its place"
        );
        assert_eq!(listed[1].envoy_question, "second thoughts");
        assert_eq!(store.get("old").unwrap(), Some(listed[1].clone()));
        assert!(
            InboundStore::new(&tmp.path, "inst-b")
                .list(t(base))
                .unwrap()
                .is_empty(),
            "another instance has its own file"
        );
        assert!(
            PendingStore::new(&tmp.path, "inst-a")
                .list(t(base))
                .unwrap()
                .is_empty(),
            "the pending store never sees a peer's question"
        );

        let expiry = t(base - 10) + PENDING_TTL;
        assert_eq!(inbound_ids(&store.list(expiry).unwrap()), vec!["new"]);
        store.upsert(inbound("newer", expiry), expiry).unwrap();
        assert_eq!(
            inbound_ids(&store.list(t(base)).unwrap()),
            vec!["newer", "new"],
            "an upsert drops the expired from the file"
        );
        assert!(store.remove("new").unwrap());
        assert!(!store.remove("new").unwrap());
        assert!(store.get("new").unwrap().is_none());

        let records: Vec<InboundRecord> = (0..INBOUND_MAX_ENTRIES)
            .rev()
            .map(|n| inbound(&format!("q{n}"), t(base + n as u64)))
            .collect();
        write_jsonl(store.path(), &records, INBOUND_NAMES).unwrap();
        store
            .upsert(inbound("latest", t(base + 10_000)), t(base + 10_000))
            .unwrap();
        let listed = store.list(t(base + 10_000)).unwrap();
        assert_eq!(listed.len(), INBOUND_MAX_ENTRIES);
        assert_eq!(listed[0].id, "latest");
        assert!(!inbound_ids(&listed).contains(&"q0"), "the oldest goes");
        assert!(inbound_ids(&listed).contains(&"q1"));
    }

    #[test]
    fn inbound_adopt_from_merges_once_skipping_ids_already_held_and_the_expired() {
        let tmp = TempDir::new("inbound-adopt");
        let base = 1_000_000;
        let source = InboundStore::new(&tmp.path, "inst-a");
        source
            .upsert(inbound("shared", t(base - 20)), t(base))
            .unwrap();
        source
            .upsert(inbound("fresh", t(base - 5)), t(base))
            .unwrap();
        source
            .upsert(inbound("stale", t(base - 30)), t(base))
            .unwrap();
        let target = InboundStore::new(&tmp.path, "inst-b");
        let mut held = inbound("shared", t(base - 20));
        held.envoy_question = "the fork's own proposal".into();
        target.upsert(held, t(base)).unwrap();
        target
            .upsert(inbound("own", t(base - 10)), t(base))
            .unwrap();

        let now = t(base - 30) + PENDING_TTL;
        assert_eq!(target.adopt_from(&source, now).unwrap(), 1);

        let listed = target.list(now).unwrap();
        assert_eq!(inbound_ids(&listed), vec!["fresh", "own", "shared"]);
        assert_eq!(
            listed[2].envoy_question, "the fork's own proposal",
            "a colliding id keeps the target's record"
        );
        assert_eq!(
            inbound_ids(&source.list(now).unwrap()),
            vec!["fresh", "shared"],
            "the source is not touched"
        );
        assert_eq!(target.adopt_from(&source, now).unwrap(), 0);
    }

    #[test]
    fn inbound_upsert_refuses_records_the_reader_would_refuse() {
        let tmp = TempDir::new("inbound-refuse");
        let store = InboundStore::new(&tmp.path, "inst");
        let base = inbound("fine", t(1_000));
        for (what, broken, needle) in [
            (
                "version",
                InboundRecord {
                    version: INBOUND_RECORD_VERSION + 1,
                    ..base.clone()
                },
                "version",
            ),
            (
                "empty id",
                InboundRecord {
                    id: String::new(),
                    ..base.clone()
                },
                "id",
            ),
            (
                "bad received_at",
                InboundRecord {
                    received_at: "yesterday".into(),
                    ..base.clone()
                },
                "RFC 3339",
            ),
            (
                "bad identity",
                InboundRecord {
                    peer_identity: "CD".repeat(16),
                    ..base.clone()
                },
                "peer_identity",
            ),
            (
                "long question",
                InboundRecord {
                    question: "q".repeat(PENDING_QUESTION_MAX_CHARS + 1),
                    ..base.clone()
                },
                "characters",
            ),
            (
                "long envoy question",
                InboundRecord {
                    envoy_question: "p".repeat(INBOUND_ENVOY_QUESTION_MAX_CHARS + 1),
                    ..base.clone()
                },
                "envoy question",
            ),
        ] {
            let err = store.upsert(broken, t(1_000)).unwrap_err().to_string();
            assert!(err.contains("refusing"), "{what}: {err}");
            assert!(err.contains(needle), "{what}: {err}");
        }
        assert!(!store.path().exists());
    }

    #[test]
    fn a_newer_inbound_line_refuses_the_whole_store_and_surfaces_no_record() {
        let tmp = TempDir::new("inbound-newer-whole-store");
        let store = InboundStore::new(&tmp.path, "inst");
        store.upsert(inbound("fine", t(1_000)), t(1_000)).unwrap();
        let mut newer = serde_json::to_value(inbound("future", t(2_000))).unwrap();
        newer["version"] = serde_json::json!(INBOUND_RECORD_VERSION + 1);
        newer["future_field"] = serde_json::json!(1);
        let existing = fs::read_to_string(store.path()).unwrap();
        let written = format!("{newer}\n{existing}");
        fs::write(store.path(), &written).unwrap();

        let err = store.list(t(2_000)).unwrap_err().to_string();

        assert!(err.contains(&store.path().display().to_string()), "{err}");
        assert!(err.contains("inbound store"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("version 2"), "{err}");
        assert!(err.contains("version 1"), "{err}");
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(
            store.get("fine").is_err(),
            "no record of a refused store is readable"
        );
        assert_eq!(
            fs::read_to_string(store.path()).unwrap(),
            written,
            "a refused file is left where it is"
        );
    }

    #[test]
    fn a_pre_baseline_inbound_line_refuses_as_having_no_migration() {
        let tmp = TempDir::new("inbound-older");
        let store = InboundStore::new(&tmp.path, "inst");
        let mut older = serde_json::to_value(inbound("q1", t(1_000))).unwrap();
        older["version"] = serde_json::json!(0);
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{older}\n")).unwrap();

        let err = store.list(t(1_000)).unwrap_err().to_string();

        assert!(err.contains("inbound store"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("version 0"), "{err}");
        assert!(err.contains("no migration"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn an_inbound_line_without_a_version_refuses_the_whole_store() {
        let tmp = TempDir::new("inbound-unversioned");
        let store = InboundStore::new(&tmp.path, "inst");
        let mut bare = serde_json::to_value(inbound("q1", t(1_000))).unwrap();
        bare.as_object_mut().unwrap().remove("version");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{bare}\n")).unwrap();

        let err = format!("{:#}", store.list(t(1_000)).unwrap_err());

        assert!(err.contains(&store.path().display().to_string()), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn an_inbound_record_with_an_unknown_field_is_refused() {
        let tmp = TempDir::new("inbound-unknown-field");
        let store = InboundStore::new(&tmp.path, "inst");
        let mut current = serde_json::to_value(inbound("q1", t(1_000))).unwrap();
        current["added_later"] = serde_json::json!(true);
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{current}\n")).unwrap();

        let err = format!("{:#}", store.list(t(1_000)).unwrap_err());

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("inbound question"), "{err}");
        assert!(err.contains("added_later"), "{err}");
    }

    #[test]
    fn inbound_upsert_refuses_an_id_another_peer_holds_open() {
        let tmp = TempDir::new("inbound-collide");
        let store = InboundStore::new(&tmp.path, "inst");
        let first = inbound("shared", t(1_000));
        store.upsert(first.clone(), t(1_000)).unwrap();

        let mut other_peer = inbound("shared", t(1_001));
        other_peer.peer_destination = hex_lower(&[0xee; 16]);
        let err = store.upsert(other_peer, t(1_001)).unwrap_err().to_string();
        assert!(err.contains("belongs to another peer"), "{err}");
        assert_eq!(store.get("shared").unwrap(), Some(first.clone()));

        let mut same_peer = inbound("shared", t(1_002));
        same_peer.envoy_question = "asked again".into();
        store.upsert(same_peer.clone(), t(1_002)).unwrap();
        assert_eq!(store.get("shared").unwrap(), Some(same_peer));
        assert_eq!(store.list(t(1_002)).unwrap().len(), 1);
    }

    #[test]
    fn inbound_upsert_lets_a_new_peer_reuse_an_expired_id() {
        let tmp = TempDir::new("inbound-expired-reuse");
        let store = InboundStore::new(&tmp.path, "inst");
        let now = t(1_000_000);
        let stale = inbound("shared", now - PENDING_TTL - Duration::from_secs(1));
        store.upsert(stale, now - PENDING_TTL).unwrap();

        let mut other_peer = inbound("shared", now);
        other_peer.peer_destination = hex_lower(&[0xee; 16]);
        store.upsert(other_peer.clone(), now).unwrap();
        assert_eq!(store.get("shared").unwrap(), Some(other_peer));
        assert_eq!(store.list(now).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn correlations_answer_wakes_a_waiter_and_wait_never_removes_the_record() {
        let correlations = std::sync::Arc::new(Correlations::new());
        correlations
            .open(record("q1", t(1_000), PendingState::Open))
            .unwrap();
        assert!(correlations.is_open("q1"));
        assert_eq!(
            correlations.wait("q1", Duration::from_millis(50)).await,
            WaitOutcome::Pending
        );
        assert_eq!(
            correlations.wait("nope", Duration::from_millis(50)).await,
            WaitOutcome::Unknown
        );

        let waiter = {
            let correlations = correlations.clone();
            tokio::spawn(async move { correlations.wait("q1", Duration::from_secs(10)).await })
        };
        tokio::task::yield_now().await;
        assert!(correlations.answer("q1", reply_to("q1")));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .unwrap()
                .unwrap(),
            WaitOutcome::Replied(Box::new(reply_to("q1")))
        );

        assert!(!correlations.is_open("q1"), "answered is no longer open");
        assert_eq!(
            correlations.wait("q1", Duration::from_millis(10)).await,
            WaitOutcome::Replied(Box::new(reply_to("q1"))),
            "a wait collects nothing"
        );
        let listed = correlations.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].record.state, PendingState::Answered);
        assert_eq!(correlations.take_answer("q1"), Some(reply_to("q1")));
        assert_eq!(correlations.take_answer("q1"), None);
        assert!(correlations.get("q1").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_landing_between_the_state_read_and_the_first_poll_still_wakes_the_waiter() {
        let correlations = Correlations::new();
        correlations
            .open(record("q1", t(1_000), PendingState::Open))
            .unwrap();
        *correlations.between_read_and_await.lock() =
            Some(Box::new(|correlations: &Correlations| {
                assert!(correlations.answer("q1", reply_to("q1")));
            }));
        let started = tokio::time::Instant::now();

        let outcome = correlations.wait("q1", Duration::from_secs(600)).await;

        assert!(
            correlations.between_read_and_await.lock().is_none(),
            "the answer must have landed inside the window"
        );
        assert_eq!(outcome, WaitOutcome::Replied(Box::new(reply_to("q1"))));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the waiter slept {:?} on a reply that was already there",
            started.elapsed()
        );
    }

    #[test]
    fn an_unmatched_reply_is_not_an_answer() {
        let correlations = Correlations::new();
        correlations
            .open(record("q1", t(1_000), PendingState::Open))
            .unwrap();

        assert!(!correlations.answer("q2", reply_to("q2")));
        assert!(correlations.answer("q1", reply_to("q1")));
        assert!(
            !correlations.answer("q1", reply_to("q1")),
            "the first answer stands"
        );
        assert_eq!(
            correlations.take_answer("unknown"),
            None,
            "collecting an unknown id removes nothing"
        );
        assert_eq!(correlations.list().len(), 1);
    }

    #[test]
    fn a_reply_from_another_identity_does_not_answer_the_question() {
        let correlations = Correlations::new();
        correlations
            .open(record("q1", t(1_000), PendingState::Open))
            .unwrap();
        let mut impostor = reply_to("q1");
        impostor.source_identity = hex_lower(&[0xee; 16]);

        assert!(!correlations.answer("q1", impostor));
        assert!(correlations.is_open("q1"), "the question is still waiting");

        let mut upper = reply_to("q1");
        upper.source_identity = upper.source_identity.to_ascii_uppercase();
        assert!(
            correlations.answer("q1", upper),
            "the identity compare is case-insensitive"
        );
    }

    #[test]
    fn a_store_carries_open_and_uncollected_questions_to_the_next_correlations() {
        let tmp = TempDir::new("pending-carry");
        // `open` and `answer` stamp their store writes with the wall clock, so the
        // records must be sent now or the TTL evicts them on the way in.
        let now = SystemTime::now();
        let first = Correlations::new();
        assert_eq!(
            first
                .attach_store(PendingStore::new(&tmp.path, "inst"), now)
                .unwrap(),
            0
        );
        first.open(record("open", now, PendingState::Open)).unwrap();
        first
            .open(record(
                "answered",
                now + Duration::from_secs(1),
                PendingState::Open,
            ))
            .unwrap();
        assert!(first.answer("answered", reply_to("answered")));
        first
            .open(record(
                "collected",
                now + Duration::from_secs(2),
                PendingState::Open,
            ))
            .unwrap();
        assert!(first.answer("collected", reply_to("collected")));
        assert!(first.take_answer("collected").is_some());
        drop(first);
        // An answered record from before replies were persisted: nothing to collect.
        let store = PendingStore::new(&tmp.path, "inst");
        store
            .upsert(
                record(
                    "legacy",
                    now + Duration::from_secs(3),
                    PendingState::Answered,
                ),
                now,
            )
            .unwrap();

        let second = Correlations::new();
        assert_eq!(second.attach_store(store, now).unwrap(), 2);
        assert!(second.is_open("open"));
        assert!(second.get("legacy").is_none());
        assert_eq!(
            second.take_answer("answered"),
            Some(reply_to("answered")),
            "an uncollected answer is there for the next process"
        );
        assert_eq!(
            ids(&PendingStore::new(&tmp.path, "inst").list(now).unwrap()),
            vec!["open"]
        );
        assert!(second.answer("open", reply_to("open")));
        let on_disk = PendingStore::new(&tmp.path, "inst").list(now).unwrap();
        assert_eq!(on_disk[0].state, PendingState::Answered);
        assert_eq!(on_disk[0].reply, Some(reply_to("open")));
    }

    #[test]
    fn a_store_that_cannot_be_read_is_refused_and_what_was_attached_stays() {
        let tmp = TempDir::new("pending-unreadable");
        let now = SystemTime::now();
        let correlations = Correlations::new();
        correlations
            .attach_store(PendingStore::new(&tmp.path, "inst-a"), now)
            .unwrap();
        correlations
            .open(record("from-a", now, PendingState::Open))
            .unwrap();
        let store_b = PendingStore::new(&tmp.path, "inst-b");
        fs::write(store_b.path(), "not json\n").unwrap();
        let before = fs::read_to_string(store_b.path()).unwrap();

        let err = store_b.load_pending(now).unwrap_err().to_string();
        assert!(err.contains("move the file aside"), "{err}");
        assert_eq!(
            fs::read_to_string(store_b.path()).unwrap(),
            before,
            "a refused file is left as it was"
        );
        let err = correlations
            .attach_store(PendingStore::new(&tmp.path, "inst-b"), now)
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(
            correlations.is_open("from-a"),
            "A's questions stay in memory"
        );
        correlations
            .open(record("still-a", now, PendingState::Open))
            .unwrap();
        assert_eq!(
            ids(&PendingStore::new(&tmp.path, "inst-a").list(now).unwrap()),
            vec!["still-a", "from-a"],
            "writes still go to A"
        );

        correlations.adopt(PendingStore::new(&tmp.path, "inst-c"), vec![]);
        assert!(correlations.list().is_empty());
        correlations
            .open(record("from-c", now, PendingState::Open))
            .unwrap();
        assert_eq!(
            ids(&PendingStore::new(&tmp.path, "inst-c").list(now).unwrap()),
            vec!["from-c"]
        );
    }

    #[test]
    fn attaching_another_store_rebuilds_from_it_and_detaching_forgets_everything() {
        let tmp = TempDir::new("pending-rebind");
        let now = SystemTime::now();
        let correlations = Correlations::new();
        correlations
            .attach_store(PendingStore::new(&tmp.path, "inst-a"), now)
            .unwrap();
        correlations
            .open(record("from-a", now, PendingState::Open))
            .unwrap();
        let store_b = PendingStore::new(&tmp.path, "inst-b");
        store_b
            .upsert(record("from-b", now, PendingState::Open), now)
            .unwrap();

        assert_eq!(correlations.attach_store(store_b, now).unwrap(), 1);
        assert!(
            correlations.get("from-a").is_none(),
            "A's questions are A's"
        );
        assert!(correlations.is_open("from-b"));
        assert_eq!(
            ids(&PendingStore::new(&tmp.path, "inst-a").list(now).unwrap()),
            vec!["from-a"],
            "A's file is left for A's next install"
        );

        correlations.detach_store();
        assert!(correlations.list().is_empty());
        correlations
            .open(record("unstored", now, PendingState::Open))
            .unwrap();
        assert!(
            PendingStore::new(&tmp.path, "inst-b")
                .list(now)
                .unwrap()
                .iter()
                .all(|record| record.id != "unstored"),
            "nothing is written once detached"
        );
    }

    #[test]
    fn abandon_forgets_an_open_question_on_disk_too_and_leaves_an_answered_one() {
        let tmp = TempDir::new("pending-abandon");
        let now = SystemTime::now();
        let correlations = Correlations::new();
        correlations
            .attach_store(PendingStore::new(&tmp.path, "inst"), now)
            .unwrap();
        correlations
            .open(record("failed", now, PendingState::Open))
            .unwrap();
        correlations
            .open(record("answered", now, PendingState::Open))
            .unwrap();
        assert!(correlations.answer("answered", reply_to("answered")));

        assert!(correlations.abandon("failed"));
        assert!(!correlations.abandon("failed"), "already gone");
        assert!(
            !correlations.abandon("answered"),
            "an answer waits for take_answer"
        );
        assert!(!correlations.abandon("unknown"));
        assert!(correlations.get("failed").is_none());
        assert_eq!(
            ids(&PendingStore::new(&tmp.path, "inst").list(now).unwrap()),
            vec!["answered"]
        );
    }
}
