//! Questions this node has asked peers and is still waiting on. A `PendingRecord` is the
//! on-disk half, so a question asked in one Coyote process can be answered in the next;
//! `Correlations` is the in-memory half that matches a reply to its question and wakes
//! whoever is waiting for it. `InboundStore` is the mirror image: questions peers asked
//! this node that the envoy escalated to the person at the keyboard, kept apart so a
//! peer's reply can never be matched against one.

use crate::mesh::access::{ACCESS_MAX_PATHS, ACCESS_REASON_MAX_CHARS};
use crate::mesh::message::{Disposition, PEER_ID_MAX_CHARS, PeerMessage};
use crate::mesh::request::{redact_hashes, short};
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::wire_path::WirePath;
use crate::mesh::{canonical_hash, mesh_cache_dir, parse_rfc3339, write_atomically};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::convert::Infallible;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

pub(crate) const PENDING_RECORD_VERSION: u64 = 2;
/// Past this many live records the oldest go, answered ones before open ones: an answer
/// nobody collected is worth less than a question still waiting on one.
pub(crate) const PENDING_MAX_ENTRIES: usize = 256;
/// A question unanswered for this long is forgotten; a reply after that reads as an
/// ordinary message.
pub(crate) const PENDING_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// The store keeps the question's first words only, enough to recognise it in a list.
pub(crate) const PENDING_QUESTION_MAX_CHARS: usize = 280;
pub(crate) const DEFAULT_COLLECT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const INBOUND_RECORD_VERSION: u64 = 2;
/// Past this many escalated questions the oldest go; expiry reuses `PENDING_TTL`.
pub(crate) const INBOUND_MAX_ENTRIES: usize = 256;
/// What the envoy asked the human is kept whole, so a late `.mesh answer` still shows
/// the question as it was put.
pub(crate) const INBOUND_ENVOY_QUESTION_MAX_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingState {
    Open,
    /// The peer's human has been asked; still waiting, like `Open`, for the answer.
    Escalated,
    Answered,
}

impl PendingState {
    fn awaits_reply(self) -> bool {
        matches!(self, Self::Open | Self::Escalated)
    }

    /// Escalation is one-shot: a second `escalated` reply to a question already
    /// `Escalated` is a message, not an answer.
    fn accepts(self, disposition: Disposition) -> bool {
        match self {
            Self::Open => true,
            Self::Escalated => disposition != Disposition::Escalated,
            Self::Answered => false,
        }
    }
}

/// One line of `pending-<instance_id>.jsonl`. Unknown fields are rejected, so any change
/// to the layout, a field added included, bumps `PENDING_RECORD_VERSION` and a reader
/// refuses the whole file on a version it does not write. `reply` is the exception,
/// carried with `#[serde(default)]` and skipped when `None`: a line without one loads as
/// a question not yet answered.
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
    /// The conversation the question continues; its own id when it opened one.
    pub thread: String,
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

    /// Refuses, with the file untouched, anything `read_all` would refuse on the way back.
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
    /// pruning: the ones still awaiting a reply and the answered ones whose reply is on
    /// the line, waiting to be collected. An answered record without a reply has nothing
    /// to show, so it is dropped from the file in the same pass. The file is left as it
    /// was when anything fails.
    pub(crate) fn load_pending(&self, now: SystemTime) -> Result<Vec<PendingRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let before = records.len();
        evict(&mut records, now);
        records.retain(|record| record.state != PendingState::Answered || record.reply.is_some());
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
/// oldest still awaiting a reply; `records` is newest first.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InboundKind {
    #[default]
    Question,
    Access,
}

impl InboundKind {
    fn with_article(self) -> &'static str {
        match self {
            Self::Question => "a question",
            Self::Access => "an access request",
        }
    }
}

pub(crate) fn access_not_a_question(id: &str) -> String {
    format!(
        "`{id}` is an access request, not a question; decide it with `.mesh grant {id}` or `.mesh refuse {id}`"
    )
}

pub(crate) fn question_not_an_access_request(id: &str) -> String {
    format!("`{id}` is a question, not an access request; answer it with `.mesh answer {id}`")
}

/// One line of `inbound-<instance_id>.jsonl`. Same on-disk discipline as
/// `PendingRecord`: unknown fields are rejected and any layout change bumps
/// `INBOUND_RECORD_VERSION`. `kind`, `paths` and `reason` are the exception, carried
/// with `#[serde(default)]`: a version 2 line without them loads as a question with no
/// paths and no reason.
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
    pub thread: String,
    /// The question's first words, at most `PENDING_QUESTION_MAX_CHARS`.
    pub question: String,
    /// What the envoy asked the human about the peer's question, empty when it asked
    /// nothing of its own.
    pub envoy_question: String,
    /// RFC 3339 UTC seconds.
    pub received_at: String,
    #[serde(default)]
    pub kind: InboundKind,
    /// The paths an access request names; empty for a question.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub reason: String,
}

impl InboundRecord {
    /// Whether the record is `PENDING_TTL` or more old at `now`.
    pub(crate) fn is_expired(&self, now: SystemTime) -> bool {
        is_stale(&self.received_at, now)
    }
}

const INBOUND_NAMES: StoreNames = StoreNames {
    store: "inbound store",
    record: "inbound question",
};

/// What one `adopt_from` merge did: how many rows came over, and this store's expired
/// rows whose ids they took, handed back so the caller can announce them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Adopted {
    pub(crate) count: usize,
    pub(crate) displaced: Vec<InboundRecord>,
}

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

    /// Refuses, with the file untouched, anything the reader would refuse on the way
    /// back, a record whose id another peer's open question already carries (an answer
    /// is routed by id, so the human's answer could otherwise go to the second peer), a
    /// record that would change the kind filed under its id, and any rewrite of an
    /// access request: the human grants the paths they read, so those are never swapped.
    /// Hands back the expired row whose id the filing took, so the caller can say it went.
    pub(crate) fn upsert(
        &self,
        record: InboundRecord,
        now: SystemTime,
    ) -> Result<Option<InboundRecord>> {
        match self.file_unless(record, now, |_| None::<Infallible>)? {
            Ok(displaced) => Ok(displaced),
            Err(never) => match never {},
        }
    }

    /// `upsert` behind a rule judged on the store's live contents: `refused_by` sees the
    /// unexpired records under the same lock the insert takes, so what it counts cannot
    /// change between its answer and the write. `Ok(Err(refusal))` is its answer when it
    /// refuses; nothing is written then. Expired rows stay in the file for
    /// `prune_expired`, so the sweep that calls it can say what went; the one exception
    /// is a store already holding `INBOUND_MAX_ENTRIES`, which lets its oldest rows go.
    pub(crate) fn file_unless<E>(
        &self,
        record: InboundRecord,
        now: SystemTime,
        refused_by: impl FnOnce(&[InboundRecord]) -> Option<E>,
    ) -> Result<Result<Option<InboundRecord>, E>> {
        let _guard = self.write_lock.lock();
        let _file_lock = file_lock(&self.path, INBOUND_NAMES)?;
        let mut records = self.read_all()?;
        let live: Vec<InboundRecord> = records
            .iter()
            .filter(|record| !record.is_expired(now))
            .cloned()
            .collect();
        if let Some(refusal) = refused_by(&live) {
            return Ok(Err(refusal));
        }
        self.insert_under_lock(&mut records, record, now).map(Ok)
    }

    /// The expired row whose id `record` took, if any, is handed back rather than
    /// dropped: it still has an expiry line owed to it.
    fn insert_under_lock(
        &self,
        records: &mut Vec<InboundRecord>,
        record: InboundRecord,
        now: SystemTime,
    ) -> Result<Option<InboundRecord>> {
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
        validate_kind_shape(&record)?;
        let displaced = match records.iter().position(|existing| existing.id == record.id) {
            // An expired row's id is free again; the fresh filing goes to the head so
            // the file stays newest first.
            Some(pos) if records[pos].is_expired(now) => {
                let expired = records.remove(pos);
                records.insert(0, record);
                Some(expired)
            }
            Some(pos) if records[pos].peer_destination != record.peer_destination => bail!(
                "An open question with id {} belongs to another peer; refusing to store it.",
                record.id
            ),
            Some(pos) if records[pos].kind != record.kind => bail!(
                "`{}` is already filed as {}; refusing to rewrite it as {}.",
                record.id,
                records[pos].kind.with_article(),
                record.kind.with_article()
            ),
            Some(_) if record.kind == InboundKind::Access => bail!(
                "An access request is decided, never rewritten; refusing to replace `{}`.",
                record.id
            ),
            Some(pos) => {
                records[pos] = record;
                None
            }
            None => {
                records.insert(0, record);
                None
            }
        };
        records.truncate(INBOUND_MAX_ENTRIES);
        write_jsonl(&self.path, records, INBOUND_NAMES)?;
        Ok(displaced)
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
        records.retain(|record| !record.is_expired(now));
        Ok(records)
    }

    /// Takes every expired record out of the file under the store's lock and hands
    /// them back, newest first, so the caller can say what went; the file is rewritten
    /// only when something did. A store with no file yet has nothing to expire.
    pub(crate) fn prune_expired(&self, now: SystemTime) -> Result<Vec<InboundRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let _guard = self.write_lock.lock();
        let _file_lock = file_lock(&self.path, INBOUND_NAMES)?;
        let (kept, expired): (Vec<InboundRecord>, Vec<InboundRecord>) = self
            .read_all()?
            .into_iter()
            .partition(|record| !record.is_expired(now));
        if !expired.is_empty() {
            write_jsonl(&self.path, &kept, INBOUND_NAMES)?;
        }
        Ok(expired)
    }

    /// Copies `other`'s unexpired questions into this store under one lock and one write;
    /// ids an unexpired row here already carries are left as they are, and `other` is
    /// not touched. An expired row here whose id an incoming row carries gives way to
    /// it, as under `upsert`, and is handed back in `displaced`; the other expired rows
    /// stay for `prune_expired`.
    pub(crate) fn adopt_from(&self, other: &InboundStore, now: SystemTime) -> Result<Adopted> {
        let incoming = other.list(now)?;
        if incoming.is_empty() {
            return Ok(Adopted::default());
        }
        let _guard = self.write_lock.lock();
        let _file_lock = file_lock(&self.path, INBOUND_NAMES)?;
        let records = self.read_all()?;
        let adopted: Vec<InboundRecord> = incoming
            .into_iter()
            .filter(|record| {
                !records
                    .iter()
                    .any(|existing| existing.id == record.id && !existing.is_expired(now))
            })
            .collect();
        let count = adopted.len();
        if count == 0 {
            return Ok(Adopted::default());
        }
        // Only an expired row can share an id with an adopted one: live twins were
        // filtered out above.
        let (displaced, mut records): (Vec<InboundRecord>, Vec<InboundRecord>) = records
            .into_iter()
            .partition(|existing| adopted.iter().any(|record| record.id == existing.id));
        records.extend(adopted);
        records.sort_by(|a, b| b.received_at.cmp(&a.received_at));
        records.truncate(INBOUND_MAX_ENTRIES);
        write_jsonl(&self.path, &records, INBOUND_NAMES)?;
        Ok(Adopted { count, displaced })
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

/// The kind-specific fields have to agree with `kind`: a question carries no paths or
/// reason, an access request carries no question text and the paths and reason the
/// wire handler accepted, so a stored record can be granted without being validated
/// again.
fn validate_kind_shape(record: &InboundRecord) -> Result<()> {
    match record.kind {
        InboundKind::Question => {
            if !record.paths.is_empty() || !record.reason.is_empty() {
                bail!(
                    "An inbound question carries paths or a reason, which only an access request may; refusing to store it."
                );
            }
        }
        InboundKind::Access => {
            if !record.question.is_empty() || !record.envoy_question.is_empty() {
                bail!(
                    "An inbound access request carries a question, which only a question may; refusing to store it."
                );
            }
            if record.paths.is_empty() || record.paths.len() > ACCESS_MAX_PATHS {
                bail!(
                    "An inbound access request names between 1 and {ACCESS_MAX_PATHS} paths, not {}; refusing to store it.",
                    record.paths.len()
                );
            }
            for (index, path) in record.paths.iter().enumerate() {
                WirePath::parse(path).with_context(|| {
                    format!(
                        "Requested path {} of {} is not a wire path",
                        index + 1,
                        record.paths.len()
                    )
                })?;
            }
            if record.reason.chars().count() > ACCESS_REASON_MAX_CHARS {
                bail!(
                    "An inbound access request's reason is longer than {ACCESS_REASON_MAX_CHARS} characters; refusing to store it."
                );
            }
        }
    }
    Ok(())
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
    /// Still unanswered when the wait ran out, and the peer's human has been asked.
    Escalated,
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
            .is_some_and(|entry| entry.record.state.awaits_reply())
    }

    /// Whether a reply naming `id` from `identity` with `disposition` is the answer this
    /// node is still waiting for: the same test `answer` applies, so admission and
    /// correlation agree.
    pub(crate) fn accepts_reply_from(
        &self,
        id: &str,
        identity: &str,
        disposition: Disposition,
    ) -> bool {
        self.state.lock().entries.get(id).is_some_and(|entry| {
            entry.record.state.accepts(disposition)
                && entry.record.peer_identity.eq_ignore_ascii_case(identity)
        })
    }

    /// `true` when `in_reply_to` named a question still awaiting a reply, asked of
    /// `reply`'s identity. An `Escalated` reply moves an `Open` question to `Escalated`
    /// and keeps waiting; any other closes it with `reply` as its answer, on disk too so
    /// it survives to the next process uncollected. A reply from any other identity, one
    /// to a question already answered, or a second `Escalated` reply to a question
    /// already escalated does not match: it is an ordinary message.
    pub(crate) fn answer(&self, in_reply_to: &str, reply: PeerMessage) -> bool {
        let mut state = self.state.lock();
        let CorrelationState { store, entries } = &mut *state;
        let Some(entry) = entries.get_mut(in_reply_to) else {
            return false;
        };
        if !entry.record.state.accepts(reply.disposition()) {
            debug!(
                "Mesh reply {} from {} names question {in_reply_to}, which is already {}; treating it as a message",
                reply.message_id,
                short(&reply.source_identity),
                if entry.record.state == PendingState::Escalated {
                    "escalated"
                } else {
                    "answered"
                }
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
        if reply.disposition() == Disposition::Escalated {
            entry.record.state = PendingState::Escalated;
        } else {
            entry.record.state = PendingState::Answered;
            entry.record.reply = Some(reply.clone());
            entry.reply = Some(reply);
        }
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
    /// collected with `take_answer`, so a wait that is cancelled or times out loses
    /// nothing. A timeout says whether the peer's human has been asked meanwhile.
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
                return match self.get(id) {
                    Some(correlation) if correlation.record.state == PendingState::Escalated => {
                        WaitOutcome::Escalated
                    }
                    _ => WaitOutcome::Pending,
                };
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

    /// Forgets a question still awaiting a reply, as after a send that failed, so nothing
    /// waits on a reply that was never asked for. `true` when such a record with `id` was
    /// there to remove; an answered one stays for `take_answer`.
    pub(crate) fn abandon(&self, id: &str) -> bool {
        let mut state = self.state.lock();
        let CorrelationState { store, entries } = &mut *state;
        if !entries
            .get(id)
            .is_some_and(|entry| entry.record.state.awaits_reply())
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

    /// The thread of question `id`, for the reply that answers it to inherit.
    pub(crate) fn thread_of(&self, id: &str) -> Option<String> {
        self.state
            .lock()
            .entries
            .get(id)
            .map(|entry| entry.record.thread.clone())
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
            thread: id.to_string(),
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
            thread: None,
            disposition: None,
            retry_after: None,
            parts: Vec::new(),
            dropped_parts: 0,
        }
    }

    fn reply_with(id: &str, disposition: Disposition, retry_after: Option<u32>) -> PeerMessage {
        PeerMessage {
            disposition: Some(disposition),
            retry_after,
            ..reply_to(id)
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
        assert!(
            err.contains(&format!("version {}", PENDING_RECORD_VERSION + 1)),
            "{err}"
        );
        assert!(
            err.contains(&format!("version {PENDING_RECORD_VERSION}")),
            "{err}"
        );
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
    fn a_version_1_pending_line_from_before_the_parts_bump_is_refused() {
        // A version-1 `pending-<id>.jsonl` line (no `thread`, no escalated state) is
        // spelled with a literal `"version":1` here, not the constant: there is no
        // migration, so the line is refused whole rather than loaded with a guessed
        // thread.
        let tmp = TempDir::new("pending-v1-before-bump");
        let store = PendingStore::new(&tmp.path, "inst");
        let line = concat!(
            r#"{"version":1,"id":"q-before-bump","#,
            r#""peer_destination":"abababababababababababababababab","#,
            r#""peer_identity":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","#,
            r#""question":"still pending?","#,
            r#""sent_at":"1970-01-01T00:16:40Z","timeout_at":"1970-01-01T00:17:10Z","#,
            r#""state":"open"}"#,
            "\n"
        );
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), line).unwrap();

        let err = format!("{:#}", store.list(t(1_100)).unwrap_err());

        assert!(err.contains("pending store"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("is version 1"), "{err}");
        assert!(
            err.contains(&format!("writes version {PENDING_RECORD_VERSION}")),
            "{err}"
        );
        assert!(err.contains("no migration exists"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(store.load_pending(t(1_100)).is_err());
        let path = store.path().to_path_buf();
        let correlations = Correlations::new();
        assert!(correlations.attach_store(store, t(1_100)).is_err());
        assert!(correlations.list().is_empty(), "no record is surfaced");
        assert!(
            fs::read_to_string(path).unwrap().contains("q-before-bump"),
            "a refused file is left where it is"
        );
    }

    #[test]
    fn a_version_1_inbound_line_from_before_the_parts_bump_is_refused() {
        // Same as the pending store: a version-1 inbound line (no `thread`) is refused,
        // not migrated. `kind`, `paths` and `reason` are optional within version 2, under
        // a `#[serde(default)]`.
        let tmp = TempDir::new("inbound-v1-before-bump");
        let store = InboundStore::new(&tmp.path, "inst");
        let line = concat!(
            r#"{"version":1,"id":"peer-q-before-bump","#,
            r#""peer_destination":"abababababababababababababababab","#,
            r#""peer_identity":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","#,
            r#""question":"what does the envoy think?","#,
            r#""envoy_question":"","#,
            r#""received_at":"1970-01-01T00:16:40Z"}"#,
            "\n"
        );
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), line).unwrap();

        let err = format!("{:#}", store.list(t(1_100)).unwrap_err());

        assert!(err.contains("inbound store"), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("is version 1"), "{err}");
        assert!(
            err.contains(&format!("writes version {INBOUND_RECORD_VERSION}")),
            "{err}"
        );
        assert!(err.contains("no migration exists"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(
            store.get("peer-q-before-bump").is_err(),
            "no record of a refused store is readable"
        );
        assert!(
            fs::read_to_string(store.path())
                .unwrap()
                .contains("peer-q-before-bump"),
            "a refused file is left where it is"
        );
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
            thread: id.to_string(),
            question: format!("question {id}"),
            envoy_question: format!("proposal {id}"),
            received_at: rfc3339_utc(received_at),
            kind: InboundKind::Question,
            paths: Vec::new(),
            reason: String::new(),
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
            inbound_ids(&store.list(expiry).unwrap()),
            vec!["newer", "new"],
            "the listing hides the expired"
        );
        assert!(
            store.get("old").unwrap().is_some(),
            "an upsert leaves the expired row in the file for the sweep"
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
        assert_eq!(target.adopt_from(&source, now).unwrap().count, 1);

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
        assert_eq!(target.adopt_from(&source, now).unwrap().count, 0);
    }

    #[test]
    fn a_fresh_filing_keeps_an_expired_row_for_the_sweep() {
        let tmp = TempDir::new("inbound-keep-expired");
        let store = InboundStore::new(&tmp.path, "inst");
        let now = t(1_000_000);
        let stale = inbound("stale", now - PENDING_TTL - Duration::from_secs(1));
        assert_eq!(
            store.upsert(stale.clone(), now - PENDING_TTL).unwrap(),
            None
        );

        assert_eq!(
            store.upsert(inbound("fresh", now), now).unwrap(),
            None,
            "a filing under a new id displaces nothing"
        );

        assert_eq!(
            store.get("stale").unwrap(),
            Some(stale.clone()),
            "the fresh filing leaves the expired row in the file"
        );
        assert_eq!(inbound_ids(&store.list(now).unwrap()), vec!["fresh"]);
        let seen = store
            .file_unless(inbound("next", now), now, |open| {
                Some(inbound_ids(open).join(","))
            })
            .unwrap();
        assert_eq!(
            seen,
            Err("fresh".to_string()),
            "a stale row refuses nothing"
        );

        assert_eq!(store.prune_expired(now).unwrap(), vec![stale]);
        assert!(store.get("stale").unwrap().is_none());
        assert_eq!(inbound_ids(&store.list(now).unwrap()), vec!["fresh"]);
    }

    /// The other writer, the merge a re-key runs: an expired row already in the target
    /// stays in the file through `adopt_from`, hidden from the listing, and is handed
    /// back by `prune_expired` alone, so the sweep can still say it went.
    #[test]
    fn usage_probe_adopt_from_keeps_the_targets_expired_row_for_the_sweep() {
        let tmp = TempDir::new("inbound-adopt-keeps-expired");
        let base = 1_000_000;
        let now = t(base);
        let source = InboundStore::new(&tmp.path, "inst-a");
        source.upsert(inbound("fresh", now), now).unwrap();
        let target = InboundStore::new(&tmp.path, "inst-b");
        let stale = inbound("stale", now - PENDING_TTL - Duration::from_secs(1));
        target.upsert(stale.clone(), now - PENDING_TTL).unwrap();

        assert_eq!(
            target.adopt_from(&source, now).unwrap(),
            Adopted {
                count: 1,
                displaced: Vec::new()
            }
        );

        assert_eq!(
            target.get("stale").unwrap(),
            Some(stale.clone()),
            "the merge leaves the expired row in the file"
        );
        assert_eq!(inbound_ids(&target.list(now).unwrap()), vec!["fresh"]);
        assert_eq!(
            target.prune_expired(now).unwrap(),
            vec![stale],
            "the sweep is what takes it out"
        );
        assert!(target.get("stale").unwrap().is_none());
        assert_eq!(inbound_ids(&target.list(now).unwrap()), vec!["fresh"]);
    }

    /// A peer that asked again under an id whose row in the target has expired: the
    /// merge files the fresh row and hands the expired twin back, as `upsert` does, so
    /// the caller announces it and the sweep has nothing left to say for that id.
    #[test]
    fn adopt_from_lets_a_fresh_row_replace_the_targets_expired_twin() {
        let tmp = TempDir::new("inbound-adopt-replaces-expired-twin");
        let now = t(1_000_000);
        let source = InboundStore::new(&tmp.path, "inst-a");
        let fresh = InboundRecord {
            question: "are you there now?".to_string(),
            ..inbound("shared", now)
        };
        source.upsert(fresh.clone(), now).unwrap();
        let target = InboundStore::new(&tmp.path, "inst-b");
        let stale = inbound("shared", now - PENDING_TTL - Duration::from_secs(1));
        target.upsert(stale.clone(), now - PENDING_TTL).unwrap();

        assert_eq!(
            target.adopt_from(&source, now).unwrap(),
            Adopted {
                count: 1,
                displaced: vec![stale]
            },
            "the expired twin comes back to be announced"
        );

        assert_eq!(
            target.get("shared").unwrap(),
            Some(fresh),
            "the fresh filing wins over the expired twin"
        );
        assert_eq!(inbound_ids(&target.list(now).unwrap()), vec!["shared"]);
        assert!(
            target.prune_expired(now).unwrap().is_empty(),
            "nothing is left for the sweep to announce"
        );
    }

    /// The merge dedups against the target's LIVE rows only: an expired twin gives way
    /// to the incoming row whoever filed either (the id is free again, as under
    /// `upsert`, so another peer's fresh row takes it), a live twin keeps the target's
    /// row whatever the source carries, a target row the source does not name stays,
    /// the merged file is ordered by `received_at` newest first, nothing is left for
    /// the sweep, and repeating the merge (as the re-key does) adopts nothing more and
    /// changes nothing.
    #[test]
    fn usage_probe_adopt_from_frees_an_expired_twins_id_for_any_peer_keeps_live_twins_and_repeats_as_a_no_op()
     {
        let tmp = TempDir::new("inbound-adopt-expired-twin-any-peer");
        let now = t(1_000_000);
        let hour = Duration::from_secs(3_600);
        let other_peer = |mut record: InboundRecord| {
            record.peer_destination = hex_lower(&[0x11; 16]);
            record.peer_identity = hex_lower(&[0x22; 16]);
            record
        };

        let source = InboundStore::new(&tmp.path, "inst-a");
        let fresh_shared = other_peer(InboundRecord {
            question: "still there, from someone else?".to_string(),
            ..inbound("shared", now - 2 * hour)
        });
        source.upsert(fresh_shared.clone(), now).unwrap();
        source
            .upsert(
                InboundRecord {
                    question: "the source's words".to_string(),
                    ..inbound("live-dup", now - 3 * hour)
                },
                now,
            )
            .unwrap();

        let target = InboundStore::new(&tmp.path, "inst-b");
        let targets_stale_shared = inbound("shared", now - PENDING_TTL - hour);
        target
            .upsert(targets_stale_shared.clone(), now - PENDING_TTL)
            .unwrap();
        let targets_live_dup = InboundRecord {
            question: "the target's words".to_string(),
            ..inbound("live-dup", now - hour)
        };
        target.upsert(targets_live_dup.clone(), now).unwrap();
        target.upsert(inbound("l0", now - hour / 2), now).unwrap();
        target
            .upsert(
                inbound("stale-alone", now - PENDING_TTL - 2 * hour),
                now - PENDING_TTL,
            )
            .unwrap();

        assert_eq!(
            target.adopt_from(&source, now).unwrap(),
            Adopted {
                count: 1,
                displaced: vec![targets_stale_shared]
            },
            "only the freed id is adopted, and its expired twin comes back"
        );
        assert_eq!(
            target.get("shared").unwrap(),
            Some(fresh_shared.clone()),
            "another peer's fresh row takes the expired twin's id"
        );
        assert_eq!(
            target.get("live-dup").unwrap(),
            Some(targets_live_dup.clone()),
            "a live twin keeps the target's row"
        );
        assert_eq!(
            inbound_ids(&target.list(now).unwrap()),
            vec!["l0", "live-dup", "shared"],
            "merged newest first by received_at"
        );
        assert!(
            target.get("stale-alone").unwrap().is_some(),
            "an expired row the source does not name stays for the sweep"
        );
        assert_eq!(
            inbound_ids(&source.list(now).unwrap()),
            vec!["live-dup", "shared"],
            "the source is not touched (file order, latest filing first)"
        );

        assert_eq!(
            target.adopt_from(&source, now).unwrap(),
            Adopted::default(),
            "the re-key's second carry adopts and displaces nothing"
        );
        assert_eq!(
            inbound_ids(&target.list(now).unwrap()),
            vec!["l0", "live-dup", "shared"]
        );
        assert_eq!(target.get("live-dup").unwrap(), Some(targets_live_dup));
        assert_eq!(
            inbound_ids(&target.prune_expired(now).unwrap()),
            vec!["stale-alone"],
            "the sweep announces the lone expired row and never the replaced twin"
        );
    }

    #[test]
    fn prune_expired_returns_the_removed_rows_and_keeps_the_rest() {
        let tmp = TempDir::new("inbound-prune-expired");
        let base = 1_000_000;
        let store = InboundStore::new(&tmp.path, "inst");
        assert!(
            store.prune_expired(t(base)).unwrap().is_empty(),
            "no file, nothing expired"
        );
        assert!(!store.path().exists(), "a prune creates no file");
        store
            .upsert(inbound("stale-q", t(base - 30)), t(base))
            .unwrap();
        store
            .upsert(
                InboundRecord {
                    kind: InboundKind::Access,
                    question: String::new(),
                    envoy_question: String::new(),
                    paths: vec!["src/x.rs".into()],
                    ..inbound("stale-a", t(base - 20))
                },
                t(base),
            )
            .unwrap();
        store
            .upsert(inbound("fresh", t(base - 5)), t(base))
            .unwrap();
        let now = t(base - 20) + PENDING_TTL;

        let expired = store.prune_expired(now).unwrap();

        assert_eq!(inbound_ids(&expired), vec!["stale-a", "stale-q"]);
        assert_eq!(expired[0].kind, InboundKind::Access);
        assert_eq!(expired[1].kind, InboundKind::Question);
        let bytes = fs::read(store.path()).unwrap();
        assert_eq!(inbound_ids(&store.list(now).unwrap()), vec!["fresh"]);
        assert!(store.get("stale-q").unwrap().is_none());
        assert!(store.get("stale-a").unwrap().is_none());

        assert!(store.prune_expired(now).unwrap().is_empty());
        assert_eq!(
            fs::read(store.path()).unwrap(),
            bytes,
            "a prune with nothing expired does not rewrite the file"
        );
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
        assert!(
            err.contains(&format!("version {}", INBOUND_RECORD_VERSION + 1)),
            "{err}"
        );
        assert!(
            err.contains(&format!("version {INBOUND_RECORD_VERSION}")),
            "{err}"
        );
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
    fn an_inbound_line_without_the_access_placeholders_loads_as_a_question() {
        let tmp = TempDir::new("inbound-access-defaults");
        let store = InboundStore::new(&tmp.path, "inst");
        let mut question = serde_json::to_value(inbound("q", t(1_000))).unwrap();
        let fields = question.as_object_mut().unwrap();
        for placeholder in ["kind", "paths", "reason"] {
            assert!(fields.remove(placeholder).is_some());
        }
        assert_eq!(
            question["version"],
            serde_json::json!(INBOUND_RECORD_VERSION)
        );
        let mut access = serde_json::to_value(inbound("a", t(1_001))).unwrap();
        access["kind"] = serde_json::json!("access");
        access["paths"] = serde_json::json!(["src/x.rs"]);
        access["reason"] = serde_json::json!("need it");
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{access}\n{question}\n")).unwrap();

        let listed = store.list(t(1_001)).unwrap();

        assert_eq!(inbound_ids(&listed), vec!["a", "q"]);
        assert_eq!(listed[1].kind, InboundKind::Question);
        assert!(listed[1].paths.is_empty());
        assert!(listed[1].reason.is_empty());
        assert_eq!(listed[0].kind, InboundKind::Access);
        assert_eq!(listed[0].paths, vec!["src/x.rs".to_string()]);
        assert_eq!(listed[0].reason, "need it");
    }

    #[test]
    fn inbound_upsert_holds_the_access_fields_to_the_record_kind() {
        let tmp = TempDir::new("inbound-kind-shape");
        let store = InboundStore::new(&tmp.path, "inst");
        let question = inbound("q", t(1_000));
        let access = InboundRecord {
            kind: InboundKind::Access,
            question: String::new(),
            envoy_question: String::new(),
            paths: vec!["src/x.rs".to_string()],
            reason: "need it".to_string(),
            ..inbound("a", t(1_000))
        };
        store.upsert(question.clone(), t(1_000)).unwrap();
        store.upsert(access.clone(), t(1_000)).unwrap();

        let cases = [
            (
                "a question with paths",
                InboundRecord {
                    paths: vec!["src/x.rs".to_string()],
                    ..question.clone()
                },
            ),
            (
                "a question with a reason",
                InboundRecord {
                    reason: "why".to_string(),
                    ..question.clone()
                },
            ),
            (
                "an access request without paths",
                InboundRecord {
                    paths: Vec::new(),
                    ..access.clone()
                },
            ),
            (
                "an access request past the path cap",
                InboundRecord {
                    paths: (0..=ACCESS_MAX_PATHS).map(|i| format!("p{i}.rs")).collect(),
                    ..access.clone()
                },
            ),
            (
                "an access request with a path that is not a wire path",
                InboundRecord {
                    paths: vec!["../x.rs".to_string()],
                    ..access.clone()
                },
            ),
            (
                "an access request with a reason past the cap",
                InboundRecord {
                    reason: "r".repeat(ACCESS_REASON_MAX_CHARS + 1),
                    ..access.clone()
                },
            ),
            (
                "an access request with a question",
                InboundRecord {
                    question: "may I?".to_string(),
                    ..access.clone()
                },
            ),
            (
                "an access request with an envoy question",
                InboundRecord {
                    envoy_question: "should we?".to_string(),
                    ..access.clone()
                },
            ),
        ];
        // Each under its own id, so the shape rule and not the no-rewrite rule refuses it.
        for (n, (what, broken)) in cases.into_iter().enumerate() {
            let broken = InboundRecord {
                id: format!("broken-{n}"),
                ..broken
            };
            assert!(store.upsert(broken, t(1_000)).is_err(), "{what} was stored");
        }
        assert_eq!(inbound_ids(&store.list(t(1_000)).unwrap()), vec!["a", "q"]);
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
    fn inbound_upsert_never_rewrites_an_access_request_or_changes_a_records_kind() {
        let tmp = TempDir::new("inbound-no-rewrite");
        let store = InboundStore::new(&tmp.path, "inst");
        let access = InboundRecord {
            kind: InboundKind::Access,
            question: String::new(),
            envoy_question: String::new(),
            paths: vec!["README.md".to_string()],
            reason: "the readme".to_string(),
            ..inbound("a", t(1_000))
        };
        let question = inbound("q", t(1_000));
        store.upsert(access.clone(), t(1_000)).unwrap();
        store.upsert(question.clone(), t(1_000)).unwrap();

        let swapped = InboundRecord {
            paths: vec!["src/secrets.rs".to_string()],
            received_at: rfc3339_utc(t(1_001)),
            ..access.clone()
        };
        let err = store.upsert(swapped, t(1_001)).unwrap_err().to_string();
        assert!(
            err.contains("An access request is decided, never rewritten; refusing to replace `a`"),
            "{err}"
        );
        assert_eq!(store.get("a").unwrap(), Some(access.clone()));

        let question_as_access = InboundRecord {
            id: "q".to_string(),
            thread: "q".to_string(),
            ..access.clone()
        };
        let err = store
            .upsert(question_as_access, t(1_001))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "`q` is already filed as a question; refusing to rewrite it as an access request"
            ),
            "{err}"
        );
        assert_eq!(store.get("q").unwrap(), Some(question.clone()));

        let access_as_question = InboundRecord {
            id: "a".to_string(),
            thread: "a".to_string(),
            ..question.clone()
        };
        let err = store
            .upsert(access_as_question, t(1_001))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "`a` is already filed as an access request; refusing to rewrite it as a question"
            ),
            "{err}"
        );
        assert_eq!(store.get("a").unwrap(), Some(access));

        let mut reworded = question.clone();
        reworded.envoy_question = "asked again".into();
        store.upsert(reworded.clone(), t(1_002)).unwrap();
        assert_eq!(store.get("q").unwrap(), Some(reworded));
        assert_eq!(store.list(t(1_002)).unwrap().len(), 2);
    }

    #[test]
    fn file_unless_judges_the_rule_on_the_live_records_and_writes_nothing_when_it_refuses() {
        let tmp = TempDir::new("inbound-file-unless");
        let store = InboundStore::new(&tmp.path, "inst");
        let now = t(1_000_000);
        store
            .upsert(
                inbound("stale", now - PENDING_TTL - Duration::from_secs(1)),
                now - PENDING_TTL,
            )
            .unwrap();
        store.upsert(inbound("fresh", now), now).unwrap();

        let mut seen = Vec::new();
        let refused = store
            .file_unless(inbound("next", now), now, |open| {
                seen = inbound_ids(open).into_iter().map(str::to_string).collect();
                Some("full")
            })
            .unwrap();
        assert_eq!(refused, Err("full"));
        assert_eq!(seen, vec!["fresh".to_string()]);
        assert_eq!(inbound_ids(&store.list(now).unwrap()), vec!["fresh"]);

        let filed = store
            .file_unless(inbound("next", now), now, |_| None::<&str>)
            .unwrap();
        assert_eq!(filed, Ok(None));
        assert_eq!(
            inbound_ids(&store.list(now).unwrap()),
            vec!["next", "fresh"]
        );
    }

    #[test]
    fn inbound_upsert_lets_a_new_peer_reuse_an_expired_id() {
        let tmp = TempDir::new("inbound-expired-reuse");
        let store = InboundStore::new(&tmp.path, "inst");
        let now = t(1_000_000);
        let stale = inbound("shared", now - PENDING_TTL - Duration::from_secs(1));
        store.upsert(stale.clone(), now - PENDING_TTL).unwrap();
        store
            .upsert(inbound("between", now - Duration::from_secs(10)), now)
            .unwrap();

        let mut other_peer = inbound("shared", now);
        other_peer.peer_destination = hex_lower(&[0xee; 16]);
        assert_eq!(
            store.upsert(other_peer.clone(), now).unwrap(),
            Some(stale),
            "the expired row the id was taken from comes back to be announced"
        );

        let listed = store.list(now).unwrap();
        assert_eq!(
            inbound_ids(&listed),
            vec!["shared", "between"],
            "the reused id is filed at the head, not where the expired row sat"
        );
        assert_eq!(listed[0], other_peer);
    }

    /// A filing hands back a row only when it took an EXPIRED twin's id: a rewrite of a
    /// live row, a fresh id, a refusal judged under the lock and a `file_unless` that
    /// goes through all answer `None` (nothing is owed a line), while a `file_unless`
    /// over an expired twin hands it back like `upsert` does. The cap is the one
    /// remover that hands nothing back: a full store lets its oldest rows, expired or
    /// not, go without a word, and the sweep finds nothing of them afterwards.
    #[test]
    fn a_filing_hands_back_only_the_expired_twin_it_displaced_and_the_cap_hands_back_nothing() {
        let tmp = TempDir::new("inbound-displaced-only-when-expired");
        let store = InboundStore::new(&tmp.path, "inst");
        let now = t(1_000_000);
        let live = inbound("live", now - Duration::from_secs(60));
        assert_eq!(store.upsert(live.clone(), now).unwrap(), None, "a fresh id");
        let mut rewrite = live.clone();
        rewrite.envoy_question = "second thoughts".into();
        assert_eq!(
            store.upsert(rewrite, now).unwrap(),
            None,
            "a live row rewritten in place owes no line"
        );
        let stale = inbound("stale", now - PENDING_TTL - Duration::from_secs(1));
        store.upsert(stale.clone(), now - PENDING_TTL).unwrap();
        assert_eq!(
            store
                .file_unless(inbound("other", now), now, |live_rows| {
                    assert_eq!(
                        inbound_ids(live_rows),
                        vec!["live"],
                        "the rule sees live rows only"
                    );
                    Some("over the limit")
                })
                .unwrap(),
            Err("over the limit"),
            "a refusal files nothing"
        );
        assert_eq!(
            store
                .file_unless(inbound("other", now), now, |_| None::<&str>)
                .unwrap(),
            Ok(None),
            "a filing under a new id through the rule owes no line either"
        );
        assert_eq!(
            store
                .file_unless(inbound("stale", now), now, |_| None::<&str>)
                .unwrap(),
            Ok(Some(stale.clone())),
            "the expired twin comes back through the rule as it does through upsert"
        );
        assert!(
            store.prune_expired(now).unwrap().is_empty(),
            "the displaced row is not left for the sweep to announce twice"
        );

        // The cap: a full store of old rows, half of them expired, takes one more at the
        // head and lets the oldest go; the filing hands back nothing and the sweep later
        // finds only the expired rows that stayed.
        let base = now - PENDING_TTL - Duration::from_secs(INBOUND_MAX_ENTRIES as u64);
        let full: Vec<InboundRecord> = (0..INBOUND_MAX_ENTRIES)
            .rev()
            .map(|n| inbound(&format!("q{n}"), base + Duration::from_secs(n as u64 * 2)))
            .collect();
        write_jsonl(store.path(), &full, INBOUND_NAMES).unwrap();
        let expired_before = store.list(now).unwrap().len();
        assert!(
            expired_before < INBOUND_MAX_ENTRIES,
            "some of the full store has expired"
        );
        assert_eq!(store.upsert(inbound("latest", now), now).unwrap(), None);
        assert!(
            store.get("q0").unwrap().is_none(),
            "the oldest row went silently"
        );
        assert_eq!(
            store
                .get("latest")
                .unwrap()
                .map(|record| record.id)
                .as_deref(),
            Some("latest")
        );
        let swept = store.prune_expired(now).unwrap();
        assert!(
            !swept.iter().any(|record| record.id == "q0"),
            "q0 is not the sweep's to announce"
        );
        assert_eq!(
            swept.len() + store.list(now).unwrap().len(),
            INBOUND_MAX_ENTRIES
        );
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
        // An answered record on disk with no reply persisted beside it: nothing to collect.
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

    #[tokio::test]
    async fn an_escalated_reply_keeps_the_question_pending_across_a_reopen() {
        let tmp = TempDir::new("pending-escalated");
        let now = SystemTime::now();
        let identity = hex_lower(&[0xcd; 16]);
        let first = Correlations::new();
        first
            .attach_store(PendingStore::new(&tmp.path, "inst"), now)
            .unwrap();
        first.open(record("q1", now, PendingState::Open)).unwrap();

        assert!(first.answer("q1", reply_with("q1", Disposition::Escalated, None)));

        let listed = first.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].record.state, PendingState::Escalated);
        assert!(listed[0].record.reply.is_none());
        assert!(listed[0].reply.is_none());
        assert_eq!(
            first.wait("q1", Duration::from_millis(10)).await,
            WaitOutcome::Escalated
        );
        assert!(first.is_open("q1"));
        assert!(first.accepts_reply_from("q1", &identity, Disposition::Answered));
        assert_eq!(first.take_answer("q1"), None, "nothing to collect yet");
        drop(first);

        let second = Correlations::new();
        assert_eq!(
            second
                .attach_store(PendingStore::new(&tmp.path, "inst"), now)
                .unwrap(),
            1
        );
        let listed = second.list();
        assert_eq!(listed[0].record.state, PendingState::Escalated);
        assert_eq!(
            second.wait("q1", Duration::from_millis(10)).await,
            WaitOutcome::Escalated
        );
        assert!(second.accepts_reply_from("q1", &identity, Disposition::Answered));

        let answer = reply_with("q1", Disposition::Answered, None);
        assert!(second.answer("q1", answer.clone()));
        assert_eq!(
            second.wait("q1", Duration::from_millis(10)).await,
            WaitOutcome::Replied(Box::new(answer.clone()))
        );
        assert!(!second.accepts_reply_from("q1", &identity, Disposition::Answered));
        assert_eq!(second.take_answer("q1"), Some(answer));
    }

    #[test]
    fn a_second_escalated_reply_to_an_escalated_question_is_not_an_answer_and_is_not_recorded() {
        let tmp = TempDir::new("pending-escalated-twice");
        let now = SystemTime::now();
        let identity = hex_lower(&[0xcd; 16]);
        let correlations = Correlations::new();
        correlations
            .attach_store(PendingStore::new(&tmp.path, "inst"), now)
            .unwrap();
        correlations
            .open(record("q1", now, PendingState::Open))
            .unwrap();
        assert!(correlations.answer("q1", reply_with("q1", Disposition::Escalated, None)));
        let store_file = mesh_cache_dir(&tmp.path).join("pending-inst.jsonl");
        assert!(store_file.is_file());
        fs::remove_file(&store_file).unwrap();

        assert!(!correlations.accepts_reply_from("q1", &identity, Disposition::Escalated));
        assert!(
            !correlations.answer("q1", reply_with("q1", Disposition::Escalated, None)),
            "escalation is one-shot"
        );
        assert!(
            !store_file.exists(),
            "a reply that answers nothing does not touch the store"
        );
        assert_eq!(
            correlations.get("q1").unwrap().record.state,
            PendingState::Escalated
        );

        for disposition in [
            Disposition::Answered,
            Disposition::Refused,
            Disposition::BudgetExhausted,
        ] {
            assert!(
                correlations.accepts_reply_from("q1", &identity, disposition),
                "{disposition:?} still closes an escalated question"
            );
        }
        assert!(correlations.answer("q1", reply_with("q1", Disposition::Answered, None)));
        assert_eq!(
            correlations.get("q1").unwrap().record.state,
            PendingState::Answered
        );
        assert!(store_file.is_file(), "the answer is recorded");
    }

    fn a_closing_disposition_ends_the_question(disposition: Disposition, retry_after: u32) {
        let correlations = Correlations::new();
        correlations
            .open(record("q1", t(1_000), PendingState::Open))
            .unwrap();

        assert!(correlations.answer("q1", reply_with("q1", disposition, Some(retry_after))));

        assert_eq!(
            correlations.get("q1").unwrap().record.state,
            PendingState::Answered
        );
        assert!(!correlations.is_open("q1"));
        assert!(
            !correlations.answer("q1", reply_with("q1", Disposition::Answered, None)),
            "the question is closed to a second reply"
        );
        let reply = correlations.take_answer("q1").unwrap();
        assert_eq!(reply.disposition(), disposition);
        assert_eq!(reply.retry_after, Some(retry_after));
        assert!(correlations.get("q1").is_none());
    }

    #[test]
    fn a_refused_reply_closes_the_question_with_its_disposition_and_retry_after() {
        a_closing_disposition_ends_the_question(Disposition::Refused, 90);
    }

    #[test]
    fn a_budget_exhausted_reply_closes_the_question_like_a_refusal() {
        a_closing_disposition_ends_the_question(Disposition::BudgetExhausted, 3600);
    }

    #[test]
    fn thread_is_persisted_on_pending_and_inbound_records() {
        let tmp = TempDir::new("pending-thread");
        let now = SystemTime::now();
        let correlations = Correlations::new();
        correlations
            .attach_store(PendingStore::new(&tmp.path, "inst"), now)
            .unwrap();
        correlations
            .open(PendingRecord {
                thread: "t-root".into(),
                ..record("q1", now, PendingState::Open)
            })
            .unwrap();
        assert_eq!(correlations.thread_of("q1"), Some("t-root".to_string()));
        assert_eq!(correlations.thread_of("nope"), None);
        InboundStore::new(&tmp.path, "inst")
            .upsert(
                InboundRecord {
                    thread: "t-in".into(),
                    ..inbound("peer-q", now)
                },
                now,
            )
            .unwrap();

        let reopened = Correlations::new();
        reopened
            .attach_store(PendingStore::new(&tmp.path, "inst"), now)
            .unwrap();
        assert_eq!(reopened.get("q1").unwrap().record.thread, "t-root");
        assert_eq!(reopened.thread_of("q1"), Some("t-root".to_string()));
        assert_eq!(
            InboundStore::new(&tmp.path, "inst")
                .get("peer-q")
                .unwrap()
                .unwrap()
                .thread,
            "t-in"
        );
    }

    #[test]
    fn evict_drops_an_answered_record_before_an_escalated_one() {
        let tmp = TempDir::new("pending-evict-escalated");
        let store = PendingStore::new(&tmp.path, "inst");
        let base = 1_000_000;
        let mut records: Vec<PendingRecord> = (1..=PENDING_MAX_ENTRIES)
            .rev()
            .map(|n| record(&format!("q{n}"), t(base + n as u64), PendingState::Open))
            .collect();
        records[PENDING_MAX_ENTRIES / 2] = PendingRecord {
            reply: Some(reply_to("answered")),
            ..record(
                "answered",
                t(base + (PENDING_MAX_ENTRIES / 2) as u64),
                PendingState::Answered,
            )
        };
        records.push(record("escalated", t(base), PendingState::Escalated));
        assert_eq!(records.len(), PENDING_MAX_ENTRIES + 1);
        store.write_all(&records).unwrap();

        assert_eq!(store.prune(t(base + 10_000)).unwrap(), 1);

        let listed = store.list(t(base + 10_000)).unwrap();
        assert_eq!(listed.len(), PENDING_MAX_ENTRIES);
        assert!(!ids(&listed).contains(&"answered"), "the answered one goes");
        assert_eq!(
            listed.last().map(|record| record.id.as_str()),
            Some("escalated"),
            "the oldest, still awaiting a reply, stays"
        );
    }

    /// A version-1 file (no `thread`) is refused whole with the `schema::version_refusal`
    /// wording naming both versions and "no migration", never read as a store with zero
    /// records, never migrated in place.
    #[test]
    fn a_version_one_pending_or_inbound_file_from_the_previous_build_is_refused_not_migrated() {
        assert_eq!(PENDING_RECORD_VERSION, 2);
        assert_eq!(INBOUND_RECORD_VERSION, 2);
        let tmp = TempDir::new("pending-v1-previous-build");
        let now = t(1_000);

        let mut v1_pending = serde_json::to_value(record("q1", now, PendingState::Open)).unwrap();
        v1_pending["version"] = serde_json::json!(1);
        v1_pending.as_object_mut().unwrap().remove("thread");
        let pending = PendingStore::new(&tmp.path, "inst");
        fs::create_dir_all(pending.path().parent().unwrap()).unwrap();
        fs::write(pending.path(), format!("{v1_pending}\n")).unwrap();

        let mut v1_inbound = serde_json::to_value(inbound("peer-q", now)).unwrap();
        v1_inbound["version"] = serde_json::json!(1);
        for placeholder in ["thread", "kind", "paths", "reason"] {
            v1_inbound.as_object_mut().unwrap().remove(placeholder);
        }
        let inbound_store = InboundStore::new(&tmp.path, "inst");
        fs::create_dir_all(inbound_store.path().parent().unwrap()).unwrap();
        fs::write(inbound_store.path(), format!("{v1_inbound}\n")).unwrap();
        let before = fs::read(inbound_store.path()).unwrap();

        let err = pending.list(now).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(
            err.contains("is version 1 but this Coyote writes version 2"),
            "{err}"
        );
        assert!(
            err.contains("no migration exists for versions before 2"),
            "{err}"
        );
        assert!(!err.contains("upgrade Coyote"), "{err}");

        let err = inbound_store.list(now).unwrap_err().to_string();
        assert!(err.contains("line 1"), "{err}");
        assert!(
            err.contains("is version 1 but this Coyote writes version 2"),
            "{err}"
        );
        assert!(
            err.contains("no migration exists for versions before 2"),
            "{err}"
        );
        assert!(inbound_store.get("peer-q").is_err(), "a lookup refuses too");
        assert_eq!(
            fs::read(inbound_store.path()).unwrap(),
            before,
            "refusal leaves the old file byte-for-byte as it was"
        );
        assert!(
            Correlations::new().attach_store(pending, now).is_err(),
            "attaching the refused store surfaces the refusal rather than an empty store"
        );
    }
}
