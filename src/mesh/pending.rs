//! Questions this node has asked peers and is still waiting on. A `PendingRecord` is the
//! on-disk half, so a question asked in one Coyote process can be answered in the next;
//! `Correlations` is the in-memory half that matches a reply to its question and wakes
//! whoever is waiting for it.

use crate::mesh::message::{PEER_ID_MAX_CHARS, PeerMessage};
use crate::mesh::r3::short;
use crate::mesh::{canonical_hash, mesh_cache_dir, parse_rfc3339, write_atomically};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingState {
    Open,
    Answered,
}

/// One line of `pending-<instance_id>.jsonl`. The shape is a stable on-disk record other
/// code reads back: fields are only ever added, never renamed or removed, and a reader
/// skips fields it does not know so an older Coyote can read a newer one's file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Deserialize)]
struct VersionProbe {
    version: u64,
}

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

    /// An exclusive lock on `<path>.lock`, held until the returned `File` drops. Every
    /// Coyote process of one identity shares the cache directory, and `write_atomically`
    /// renames over the file itself, so the lock lives on a sibling that is never replaced.
    fn file_lock(&self) -> Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
        }
        let path = self.path.with_added_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| {
                format!(
                    "Failed to open mesh pending store lock '{}'",
                    path.display()
                )
            })?;
        file.lock().with_context(|| {
            format!(
                "Failed to lock mesh pending store lock '{}'",
                path.display()
            )
        })?;
        Ok(file)
    }

    fn read_all(&self) -> Result<Vec<PendingRecord>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "Failed to read mesh pending store '{}'",
                        self.path.display()
                    )
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
                    "Mesh pending store '{}' line {} is not a pending question. It is cache: move the file aside to start fresh.",
                    self.path.display(),
                    index + 1
                )
            };
            // The version is read on its own first so a record from a newer Coyote is
            // named as such rather than failing on whatever field the newer layout added.
            let probe: VersionProbe = serde_json::from_str(line).with_context(not_a_record)?;
            if probe.version != PENDING_RECORD_VERSION {
                bail!(
                    "Mesh pending store '{}' line {} is a version {} record but this Coyote reads version {PENDING_RECORD_VERSION}. Upgrade Coyote if it was written by a newer Coyote; otherwise move the file aside (it is cache) to start fresh.",
                    self.path.display(),
                    index + 1,
                    probe.version
                );
            }
            let record: PendingRecord = serde_json::from_str(line).with_context(not_a_record)?;
            if parse_rfc3339(&record.sent_at).is_none() {
                bail!(
                    "Mesh pending store '{}' line {} has a `sent_at` that is not an RFC 3339 timestamp. It is cache: move the file aside to start fresh.",
                    self.path.display(),
                    index + 1
                );
            }
            records.push(record);
        }
        Ok(records)
    }

    fn write_all(&self, records: &[PendingRecord]) -> Result<()> {
        let mut text = String::new();
        for record in records {
            text.push_str(
                &serde_json::to_string(record)
                    .context("Failed to serialize a mesh pending question")?,
            );
            text.push('\n');
        }
        write_atomically(&self.path, text.as_bytes())
    }
}

fn is_expired(record: &PendingRecord, now: SystemTime) -> bool {
    // `read_all` already refused a `sent_at` that does not parse; a timestamp in the
    // future (clock stepped back) reads as just sent.
    parse_rfc3339(&record.sent_at)
        .is_some_and(|sent_at| now.duration_since(sent_at).unwrap_or_default() >= PENDING_TTL)
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
                "Mesh question {in_reply_to} was answered but the answer could not be recorded on disk: {err:#}"
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
            warn!("Mesh question {id} was collected but could not be removed from disk: {err:#}");
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
            warn!("Mesh question {id} was abandoned but could not be removed from disk: {err:#}");
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

        store.upsert(base, t(1_000)).unwrap();
        let mut newer =
            serde_json::to_value(record("future", t(2_000), PendingState::Open)).unwrap();
        newer["version"] = serde_json::json!(PENDING_RECORD_VERSION + 1);
        let existing = fs::read_to_string(store.path()).unwrap();
        fs::write(store.path(), format!("{newer}\n{existing}")).unwrap();
        let err = store.list(t(2_000)).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn a_record_with_a_field_this_coyote_does_not_know_is_read() {
        let tmp = TempDir::new("pending-unknown-field");
        let store = PendingStore::new(&tmp.path, "inst");
        let mut newer = serde_json::to_value(record("q1", t(1_000), PendingState::Open)).unwrap();
        newer["added_later"] = serde_json::json!({"nested": true});
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{newer}\n")).unwrap();

        let listed = store.list(t(1_000)).unwrap();
        assert_eq!(listed, vec![record("q1", t(1_000), PendingState::Open)]);
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
