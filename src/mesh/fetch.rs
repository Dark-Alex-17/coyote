//! `/list` and `/fetch`: the two paths a trusted peer reads files through. Both are
//! answered from the share set and the grant store alone; no model, tool or REPL line
//! sees a request, and the bytes of a served file go to the wire and nowhere else. The
//! dispatcher has settled who may ask before a handler here runs, so what remains is what
//! the share root serves to that peer. Every answer a peer could use to map the tree is
//! folded into one: a file that does not exist, one outside every allow and one a deny
//! refuses are all `not_shared`, built by the one function that spells it.
//!
//! The requester half, `list_shares` and `fetch_file`, is the mirror image: it treats the
//! reply as peer-controlled data, caps what it keeps, checks the bytes against the hash the
//! peer sent, and stages them under this instance's inbox.

use crate::config::mesh_config::MAX_FETCH_FILE_BYTES;
use crate::mesh::card::StatusCard;
use crate::mesh::events::{MeshEvent, MeshHooks};
use crate::mesh::grants::GrantStore;
use crate::mesh::inbox::StageError;
use crate::mesh::message::{PEER_LINK_TIMEOUT, PEER_REQUEST_TIMEOUT, PEER_WIRE_VERSION};
use crate::mesh::node::MeshRuntime;
use crate::mesh::r3::{
    AdmittedRequest, DispatchError, FETCH_PATH, Handler, LIST_PATH, MAX_R3_PAYLOAD_BYTES, R3Error,
    RefusalCode, Reply, RequestOptions, Settlement,
};
use crate::mesh::shares::{
    self, DEFAULT_LIST_WALK_BOUND, LIST_PAGE_SIZE, Listed, PeerRef, Served, ServedFile,
    ShareLocations, ShareSet, Via,
};
use crate::mesh::wire_path::{WIRE_PATH_MAX_BYTES, WirePath};
use crate::mesh::{hex_lower, redact_hashes, short};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use rmpv::Value;
use rns_transport::destination::DestinationDesc;
use rns_transport::resource::MAX_EFFICIENT_SIZE;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a fetch waits for its answer: a `MAX_FETCH_FILE_BYTES` Resource on a slow
/// interface takes minutes, where a status card takes a round trip.
pub(crate) const FILE_FETCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How many peers' last status card and last listing are kept, oldest out first.
pub(crate) const LAST_CARD_CACHE_PEERS: usize = 32;
pub(crate) const LAST_LIST_CACHE_PEERS: usize = 32;
/// Bytes left free under `MAX_R3_PAYLOAD_BYTES` by a listing page, for the frame, the
/// envelope and the page's own keys around the entries.
const LIST_PAGE_HEADROOM: usize = 2048;
/// The longest cursor a peer may send or return; `shares::list_cursor` makes 32 bytes.
const CURSOR_MAX_BYTES: usize = 64;
/// How much of a peer's `invalid_path` rule is kept; the rule ids are a dozen characters.
const RULE_MAX_CHARS: usize = 32;
/// Largest file an `ok` reply may carry while its response frame still fits ONE Resource
/// segment: `MAX_EFFICIENT_SIZE` less the frame and the reply's other keys. An itemized
/// workaround, not a protocol limit. reticulum-rs-transport 0.12.0 sends the first
/// segment's advertisement on the link's bound interface but dispatches every later one
/// through its path table (`transport/resource_wire.rs` `handle_resource_proof`, retried
/// from `transport/jobs.rs`), where a link id has no entry; a node that does not broadcast
/// unroutable packets therefore never gets segment two onto the wire and the requester
/// waits out its deadline. Removal condition: the upstream release that sends follow-up
/// advertisements the way it sends the first. Then `mesh.fetch.max_bytes` applies alone,
/// this constant and `an_ok_reply_at_the_ceiling_fits_one_resource_segment` go, and the
/// multi-segment reassembly test returns; the ceiling clauses on the `mesh.fetch.max_bytes`
/// row of README.md and in the config template and example go with it.
pub(crate) const SINGLE_SEGMENT_FETCH_CEILING: u64 =
    (MAX_EFFICIENT_SIZE - OK_REPLY_FRAMING_BYTES) as u64;
/// Room for the response frame (19 bytes), the map header and the `v`, `status`, `size`,
/// `sha256` and `bytes` keys with their headers around the file bytes: 99 bytes today,
/// rounded up.
const OK_REPLY_FRAMING_BYTES: usize = 128;
// The ceiling only ever lowers the configured limit; the day it does not, it is dead.
const _: () =
    assert!(SINGLE_SEGMENT_FETCH_CEILING < crate::config::mesh_config::MAX_FETCH_FILE_BYTES);

/// What the node serves files from: the local limits, the grant store and the case probe
/// of the share root. `max_bytes` is `mesh.fetch.max_bytes` and never a peer's number;
/// `serving_limit` is what a request is actually held to. `inbox_dir` is
/// `mesh.fetch.inbox_dir`, protected from serving along with the cache dir so a peer
/// cannot fetch what another peer sent. The grant store is swapped by `rebind_grants`
/// when the node re-keys, so a fork honours its own grants and not the original's.
pub(crate) struct FetchServing {
    config_dir: PathBuf,
    cache_dir: PathBuf,
    inbox_dir: Option<PathBuf>,
    max_bytes: u64,
    grants: ArcSwap<GrantStore>,
    hooks: MeshHooks,
    /// The last root probed and what the probe said; `None` inside means the probe
    /// failed and that root serves nothing until the root changes.
    probe: parking_lot::Mutex<Option<(PathBuf, Option<bool>)>>,
    /// The share-list refusal last warned about, so a list that stays broken is warned
    /// about once and not once per peer request.
    refused: parking_lot::Mutex<Option<String>>,
}

impl FetchServing {
    pub(crate) fn new(
        config_dir: PathBuf,
        cache_dir: PathBuf,
        inbox_dir: Option<PathBuf>,
        max_bytes: u64,
        grants: GrantStore,
        hooks: MeshHooks,
    ) -> Self {
        Self {
            config_dir,
            cache_dir,
            inbox_dir,
            max_bytes,
            grants: ArcSwap::from_pointee(grants),
            hooks,
            probe: parking_lot::Mutex::new(None),
            refused: parking_lot::Mutex::new(None),
        }
    }

    /// Opens the grant store of `instance_id` and serves from it, handing back the store it
    /// displaces so a caller that has to undo the switch can put it back without touching
    /// the disk; a store that cannot be opened leaves the current one in place.
    pub(crate) fn rebind_grants(
        &self,
        instance_id: &str,
        now: SystemTime,
    ) -> anyhow::Result<Arc<GrantStore>> {
        let grants = GrantStore::open(&self.cache_dir, instance_id, now)?;
        Ok(self.grants.swap(Arc::new(grants)))
    }

    /// Serves from `grants` again: the infallible half of `rebind_grants`.
    pub(crate) fn restore_grants(&self, grants: Arc<GrantStore>) {
        self.grants.store(grants);
    }

    #[cfg(test)]
    pub(crate) fn grants(&self) -> Arc<GrantStore> {
        self.grants.load_full()
    }

    /// The size a served file is held to: the configured limit, capped at what one
    /// Resource segment carries while `SINGLE_SEGMENT_FETCH_CEILING` stands.
    fn serving_limit(&self) -> u64 {
        self.max_bytes.min(SINGLE_SEGMENT_FETCH_CEILING)
    }

    /// Whether `root` folds case, learned once per root. The probe writes a file under
    /// the root, so it is never repeated per request; a probe that fails is remembered
    /// as failed, warned about once, and leaves that root serving nothing.
    pub(crate) fn case_insensitive_for(&self, root: &Path) -> Option<bool> {
        let mut probe = self.probe.lock();
        if let Some((probed, answer)) = probe.as_ref()
            && probed == root
        {
            return *answer;
        }
        let answer = match shares::probe_case_insensitive(root) {
            Ok(folds) => Some(folds),
            Err(err) => {
                warn!(
                    "Mesh file sharing is off for this workspace: the share root could not be probed: {}",
                    redact_hashes(&err.to_string())
                );
                None
            }
        };
        *probe = Some((root.to_path_buf(), answer));
        answer
    }

    /// Where the share lists for `root` live, with the cache and any configured inbox
    /// protected from serving.
    pub(crate) fn share_locations(&self, root: &Path) -> ShareLocations {
        let mut locations =
            ShareLocations::new(&self.config_dir, root).with_cache_dir(&self.cache_dir);
        if let Some(inbox_dir) = &self.inbox_dir {
            locations = locations.with_protected(inbox_dir);
        }
        locations
    }

    /// Loads the share set for `root`. A refused share list is the operator's to fix and
    /// is warned about when its refusal first appears or changes; the requests that keep
    /// finding it broken say so at `debug!`.
    pub(crate) fn shares_under(&self, root: &Path) -> ShareSet {
        let (shares, warning) = ShareSet::load_quietly(self.share_locations(root));
        let mut refused = self.refused.lock();
        match warning {
            Some(warning) if refused.as_deref() == Some(warning.as_str()) => {
                debug!("Mesh share list still refused; nothing is shared");
            }
            Some(warning) => {
                warn!("{warning}");
                *refused = Some(warning);
            }
            None => *refused = None,
        }
        shares
    }
}

/// Where the two providers read from: the session's share root and the node's serving
/// state. Both are `None` while the mesh is off or no snapshot has been published, and
/// then nothing is shared.
pub(crate) trait ShareSource: Send + Sync {
    fn share_root(&self) -> Option<PathBuf>;
    fn serving(&self) -> Option<Arc<FetchServing>>;
}

/// The read of a served file, behind a seam so a test can make it fail after
/// `is_served` has spent a grant use.
pub(crate) trait FileReader: Send + Sync {
    fn read_bounded(&self, file: std::fs::File, limit: u64) -> std::io::Result<Vec<u8>>;
}

struct ReadToEnd;

impl FileReader for ReadToEnd {
    fn read_bounded(&self, file: std::fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        file.take(limit).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// The requester of one admitted request, as the share set and the logs name it.
struct Requester {
    identity_hex: String,
    destination_hex: String,
}

impl Requester {
    fn of(request: &AdmittedRequest) -> Self {
        Self {
            identity_hex: request.identity.address_hash.to_hex_string(),
            destination_hex: request.destination_hash.to_hex_string(),
        }
    }

    fn peer(&self) -> PeerRef<'_> {
        PeerRef {
            identity: &self.identity_hex,
            destination: &self.destination_hex,
        }
    }

    /// The log's view of the requester: both hashes cut to `LOGGED_HASH_CHARS`.
    fn logged(&self) -> (String, String) {
        (
            short(&self.identity_hex).to_string(),
            short(&self.destination_hex).to_string(),
        )
    }
}

/// Serves `/list`. The source is held weakly because the slot owns the runtime that owns
/// the dispatcher that owns this handler.
pub(crate) struct ListHandler {
    source: Weak<dyn ShareSource>,
}

impl ListHandler {
    pub(crate) fn new(source: Weak<dyn ShareSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl Handler for ListHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        let requester = Requester::of(&request);
        let (id8, dest8) = requester.logged();
        let list = match decode_list(&request.body) {
            Ok(list) => list,
            Err(why) => {
                debug!(
                    "Mesh /list from {id8} (instance {dest8}) refused: {}",
                    redact_hashes(why)
                );
                return Reply::Code(RefusalCode::InvalidData);
            }
        };
        let Some(source) = self.source.upgrade() else {
            debug!(
                "Mesh /list from {id8} (instance {dest8}) dropped: the session slot behind the provider is gone"
            );
            return Reply::Silent;
        };
        let (Some(serving), Some(root)) = (source.serving(), source.share_root()) else {
            debug!("Mesh /list from {id8} (instance {dest8}) served 0 entries");
            return Reply::Value(list_page(Vec::new(), None));
        };
        let listed = tokio::task::spawn_blocking(move || {
            let Some(case_insensitive) = serving.case_insensitive_for(&root) else {
                return shares::Listing::default();
            };
            serving.shares_under(&root).list(
                &requester.peer(),
                list.prefix.as_deref(),
                list.cursor.as_deref(),
                case_insensitive,
                DEFAULT_LIST_WALK_BOUND,
            )
        })
        .await;
        let listing = match listed {
            Ok(listing) => listing,
            Err(err) => {
                warn!(
                    "Mesh /list from {id8} (instance {dest8}) was not answered: {}",
                    redact_hashes(&err.to_string())
                );
                return Reply::Silent;
            }
        };
        let (entries, next) = bound_page(listing);
        debug!(
            "Mesh /list from {id8} (instance {dest8}) served {} entries",
            entries.len()
        );
        Reply::Value(list_page(entries, next))
    }
}

/// Serves `/fetch`. Everything that touches the filesystem, from loading the share set to
/// reading the file, runs off the request loop.
pub(crate) struct FetchHandler {
    source: Weak<dyn ShareSource>,
    reader: Arc<dyn FileReader>,
}

impl FetchHandler {
    pub(crate) fn new(source: Weak<dyn ShareSource>) -> Self {
        Self::with_reader(source, Arc::new(ReadToEnd))
    }

    pub(crate) fn with_reader(source: Weak<dyn ShareSource>, reader: Arc<dyn FileReader>) -> Self {
        Self { source, reader }
    }
}

#[async_trait]
impl Handler for FetchHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        let requester = Requester::of(&request);
        let (id8, dest8) = requester.logged();
        let fetch = match decode_fetch(&request.body) {
            Ok(fetch) => fetch,
            Err(why) => {
                debug!(
                    "Mesh /fetch from {id8} (instance {dest8}) refused: {}",
                    redact_hashes(why)
                );
                return Reply::Code(RefusalCode::InvalidData);
            }
        };
        let Some(source) = self.source.upgrade() else {
            debug!(
                "Mesh /fetch from {id8} (instance {dest8}) dropped: the session slot behind the provider is gone"
            );
            return Reply::Silent;
        };
        let (Some(serving), Some(root)) = (source.serving(), source.share_root()) else {
            debug!("Mesh /fetch from {id8} (instance {dest8}) refused: not_shared");
            return Reply::Value(not_shared());
        };
        let reader = Arc::clone(&self.reader);
        let served = tokio::task::spawn_blocking(move || {
            serve_fetch(
                serving,
                &root,
                requester,
                fetch,
                reader.as_ref(),
                SystemTime::now(),
            )
        })
        .await;
        let outcome = match served {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(
                    "Mesh /fetch from {id8} (instance {dest8}) was not answered: {}",
                    redact_hashes(&err.to_string())
                );
                return Reply::Silent;
            }
        };
        match outcome {
            FetchOutcome::Refused { value, status } => {
                debug!("Mesh /fetch from {id8} (instance {dest8}) refused: {status}");
                Reply::Value(value)
            }
            FetchOutcome::NotModified(value) => {
                debug!("Mesh /fetch from {id8} (instance {dest8}) answered not_modified");
                Reply::Value(value)
            }
            FetchOutcome::Served { value, settlement } => {
                debug!(
                    "Mesh /fetch from {id8} (instance {dest8}) served {} bytes ({})",
                    settlement.size,
                    short(&hex_lower(&settlement.digest))
                );
                Reply::Settled { value, settlement }
            }
        }
    }
}

struct ListRequest {
    prefix: Option<String>,
    cursor: Option<String>,
}

struct FetchRequest {
    path: String,
    if_sha256: Option<[u8; 32]>,
}

/// What the blocking half of a fetch decided. `Refused` carries the status word for the
/// log; `Served` carries what happens once the reply is on the wire.
enum FetchOutcome {
    Refused {
        value: Value,
        status: String,
    },
    NotModified(Value),
    Served {
        value: Value,
        settlement: Box<FetchSettlement>,
    },
}

/// The `ok` reply's settlement: `sent` keeps the grant use spent and fires
/// `MeshEvent::FetchServed`; dropped unsent, the refund pays the use back and nothing fires.
struct FetchSettlement {
    size: u64,
    digest: [u8; 32],
    refund: Option<GrantRefund>,
    hooks: MeshHooks,
    event: MeshEvent,
}

impl Settlement for FetchSettlement {
    fn sent(self: Box<Self>) {
        if let Some(refund) = self.refund {
            refund.disarm();
        }
        self.hooks.fire(self.event);
    }
}

/// The use `is_served` spent on a grant, paid back when this is dropped unless the file
/// was delivered and `disarm` ran. Armed on creation, so every exit after `is_served`,
/// including a handler future dropped mid-flight, refunds without naming the case. The
/// refund goes to the store the use came out of, whatever store a re-key installed since.
struct GrantRefund {
    grants: Arc<GrantStore>,
    identity_hex: String,
    destination_hex: String,
    path: String,
    now: SystemTime,
    armed: bool,
}

impl GrantRefund {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for GrantRefund {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let peer = PeerRef {
            identity: &self.identity_hex,
            destination: &self.destination_hex,
        };
        if let Err(err) = self.grants.refund(&peer, &self.path, self.now) {
            debug!(
                "Mesh grant use could not be refunded: {}",
                redact_hashes(&err.to_string())
            );
        }
    }
}

/// A request whose `if_sha256` matches is answered `not_modified` and still counts as a
/// fetch: a grant use it spent stays spent.
fn serve_fetch(
    serving: Arc<FetchServing>,
    root: &Path,
    requester: Requester,
    fetch: FetchRequest,
    reader: &dyn FileReader,
    now: SystemTime,
) -> FetchOutcome {
    let refused = |value: Value, status: &str| FetchOutcome::Refused {
        value,
        status: status.to_string(),
    };
    let invalid_path = |rule: &'static str| {
        refused(
            status_reply("invalid_path", vec![("rule", Value::from(rule))]),
            &format!("invalid_path ({rule})"),
        )
    };
    // The grammar is checked before the probe so a bad name never touches the root.
    if let Err(invalid) = WirePath::parse(&fetch.path) {
        return invalid_path(invalid.rule);
    }
    let Some(case_insensitive) = serving.case_insensitive_for(root) else {
        return refused(not_shared(), "not_shared");
    };
    let limit = serving.serving_limit();
    let shares = serving.shares_under(root);
    let grants = serving.grants.load_full();
    let served = shares.is_served(
        &requester.peer(),
        &fetch.path,
        case_insensitive,
        limit,
        Some((&grants, now)),
    );
    let ServedFile {
        file, size, via, ..
    } = match served {
        Served::File(file) => file,
        Served::NotShared => return refused(not_shared(), "not_shared"),
        Served::InvalidPath { rule } => return invalid_path(rule),
        Served::TooLarge { size } => {
            return refused(
                too_large(limit),
                &format!("too_large ({size} bytes, limit {limit})"),
            );
        }
    };
    let refund = (via == Via::Grant).then(|| GrantRefund {
        grants: Arc::clone(&grants),
        identity_hex: requester.identity_hex.clone(),
        destination_hex: requester.destination_hex.clone(),
        path: fetch.path.clone(),
        now,
        armed: true,
    });
    // One byte past the limit tells a file that grew since `is_served` stat'ed it from one
    // that fits.
    let bytes = match reader.read_bounded(file, limit + 1) {
        Ok(bytes) if bytes.len() as u64 > limit => {
            return refused(
                too_large(limit),
                &format!("too_large (grew from {size} bytes past the limit {limit})"),
            );
        }
        Ok(bytes) => bytes,
        // The peer hears the same `not_shared` as for a missing file; the operator, whose
        // share list did serve this one, hears why.
        Err(err) => {
            return refused(
                not_shared(),
                &format!(
                    "not_shared (read failed: {})",
                    redact_hashes(&err.to_string())
                ),
            );
        }
    };
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    if fetch.if_sha256 == Some(digest) {
        if let Some(refund) = refund {
            refund.disarm();
        }
        return FetchOutcome::NotModified(status_reply(
            "not_modified",
            vec![("sha256", Value::Binary(digest.to_vec()))],
        ));
    }
    let size = bytes.len() as u64;
    FetchOutcome::Served {
        value: status_reply(
            "ok",
            vec![
                ("size", Value::from(size)),
                ("sha256", Value::Binary(digest.to_vec())),
                ("bytes", Value::Binary(bytes)),
            ],
        ),
        settlement: Box::new(FetchSettlement {
            size,
            digest,
            refund,
            hooks: serving.hooks.clone(),
            event: MeshEvent::FetchServed {
                identity: requester.identity_hex,
                destination: requester.destination_hex,
                size,
                hash_prefix: hex_lower(&digest)[..8].to_string(),
            },
        }),
    }
}

/// The one place `not_shared` is spelled, so a nonexistent file and an unshared one are
/// answered byte for byte alike.
fn not_shared() -> Value {
    status_reply("not_shared", Vec::new())
}

fn too_large(limit: u64) -> Value {
    status_reply("too_large", vec![("limit", Value::from(limit))])
}

fn status_reply(status: &str, extra: Vec<(&str, Value)>) -> Value {
    let mut entries = vec![
        (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
        (Value::from("status"), Value::from(status)),
    ];
    entries.extend(
        extra
            .into_iter()
            .map(|(key, value)| (Value::from(key), value)),
    );
    Value::Map(entries)
}

fn decode_list(body: &Value) -> Result<ListRequest, &'static str> {
    let entries = versioned_map(body)?;
    let prefix = match field(entries, "prefix") {
        None => None,
        Some(value) => Some(
            text_of(value)
                .map(str::to_string)
                .ok_or("prefix is not text")?,
        ),
    };
    let cursor = match field(entries, "cursor") {
        None => None,
        Some(value) => Some(
            text_of(value)
                .filter(|cursor| cursor.len() <= CURSOR_MAX_BYTES)
                .map(str::to_string)
                .ok_or("cursor is not text or is too long")?,
        ),
    };
    Ok(ListRequest { prefix, cursor })
}

fn decode_fetch(body: &Value) -> Result<FetchRequest, &'static str> {
    let entries = versioned_map(body)?;
    let path = field(entries, "path")
        .and_then(text_of)
        .map(str::to_string)
        .ok_or("path is missing or not text")?;
    let if_sha256 = match field(entries, "if_sha256") {
        None => None,
        Some(value) => Some(bin32_of(value).ok_or("if_sha256 is not a 32-byte hash")?),
    };
    Ok(FetchRequest { path, if_sha256 })
}

/// The entries of a request body that is a map carrying the supported `v`.
pub(crate) fn versioned_map(body: &Value) -> Result<&[(Value, Value)], &'static str> {
    let entries = body.as_map().ok_or("the body is not a map")?;
    if field(entries, "v").and_then(Value::as_u64) != Some(PEER_WIRE_VERSION) {
        return Err("v is missing or not the supported version");
    }
    Ok(entries)
}

/// The value under `key`, a nil counting as absent.
pub(crate) fn field<'a>(entries: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|(name, _)| name.as_str() == Some(key))
        .map(|(_, value)| value)
        .filter(|value| !value.is_nil())
}

fn text_of(value: &Value) -> Option<&str> {
    value.as_str()
}

fn bin32_of(value: &Value) -> Option<[u8; 32]> {
    bin_of(value).and_then(|bytes| bytes.try_into().ok())
}

/// Only a msgpack `bin`; `Value::as_slice` would take a string as well.
fn bin_of(value: &Value) -> Option<&[u8]> {
    match value {
        Value::Binary(bytes) => Some(bytes),
        _ => None,
    }
}

/// The page's entries as wire values, cut where the next one would take the encoded
/// response past `MAX_R3_PAYLOAD_BYTES` less the headroom. A cut page's cursor names the
/// last entry kept so the peer resumes after it; an uncut page keeps the share set's.
fn bound_page(listing: shares::Listing) -> (Vec<Value>, Option<String>) {
    let mut entries = Vec::new();
    let mut last_kept = None;
    let mut total = 0;
    for listed in listing.entries {
        let value = entry_value(&listed);
        let encoded = encoded_len(&value);
        if total + encoded + LIST_PAGE_HEADROOM > MAX_R3_PAYLOAD_BYTES {
            return (
                entries,
                last_kept
                    .as_deref()
                    .map(shares::list_cursor)
                    .or(listing.next),
            );
        }
        total += encoded;
        entries.push(value);
        last_kept = Some(listed.path);
    }
    (entries, listing.next)
}

fn entry_value(listed: &Listed) -> Value {
    Value::Map(vec![
        (Value::from("path"), Value::from(listed.path.as_str())),
        (Value::from("size"), Value::from(listed.size)),
        (Value::from("sha256"), Value::Binary(listed.sha256.to_vec())),
        (
            Value::from("mtime"),
            Value::F64(unix_secs_f64(listed.mtime)),
        ),
    ])
}

fn list_page(entries: Vec<Value>, next: Option<String>) -> Value {
    Value::Map(vec![
        (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
        (Value::from("entries"), Value::Array(entries)),
        (
            Value::from("next"),
            next.map_or(Value::Nil, |cursor| Value::from(cursor.as_str())),
        ),
    ])
}

fn encoded_len(value: &Value) -> usize {
    let mut bytes = Vec::new();
    // The only error source is the writer, and a `Vec` never fails to grow.
    let _ = rmpv::encode::write_value(&mut bytes, value);
    bytes.len()
}

fn unix_secs_f64(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

/// One file in a peer's listing, as `list_shares` read it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SharedEntry {
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub mtime: f64,
}

/// One page of a peer's listing; `next` is the cursor for the page after it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SharesPage {
    pub entries: Vec<SharedEntry>,
    pub next: Option<String>,
}

impl SharesPage {
    /// The page as the peer sent it, capped and with malformed entries skipped.
    fn from_value(value: &Value, dest8: &str) -> Result<Self, FetchError> {
        let entries = value.as_map().ok_or(FetchError::Malformed("map"))?;
        if field(entries, "v").and_then(Value::as_u64) != Some(PEER_WIRE_VERSION) {
            return Err(FetchError::Malformed("v"));
        }
        let listed = field(entries, "entries")
            .and_then(Value::as_array)
            .ok_or(FetchError::Malformed("entries"))?;
        let next = match field(entries, "next") {
            None => None,
            Some(cursor) => Some(
                text_of(cursor)
                    .filter(|cursor| cursor.len() <= CURSOR_MAX_BYTES)
                    .map(str::to_string)
                    .ok_or(FetchError::Malformed("next"))?,
            ),
        };
        let mut page = Vec::new();
        let mut skipped = 0;
        for entry in listed.iter().take(LIST_PAGE_SIZE) {
            match Self::entry(entry) {
                Some(entry) => page.push(entry),
                None => skipped += 1,
            }
        }
        if skipped > 0 {
            debug!("Mesh /list from {dest8} carried {skipped} entries this Coyote could not read");
        }
        Ok(Self {
            entries: page,
            next,
        })
    }

    fn entry(value: &Value) -> Option<SharedEntry> {
        let entries = value.as_map()?;
        let path = field(entries, "path")
            .and_then(text_of)
            .filter(|path| path.len() <= WIRE_PATH_MAX_BYTES && WirePath::parse(path).is_ok())?;
        let size = field(entries, "size").and_then(Value::as_u64)?;
        let sha256 = field(entries, "sha256").and_then(bin32_of)?;
        let mtime = field(entries, "mtime")
            .and_then(Value::as_f64)
            .filter(|mtime| mtime.is_finite())?;
        Some(SharedEntry {
            path: path.to_string(),
            size,
            sha256,
            mtime,
        })
    }
}

/// What a fetch came back with. Everything but `Staged` is the peer's typed answer;
/// a path this node's own grammar refuses is `InvalidPath` without a round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fetched {
    Staged {
        path: PathBuf,
        size: u64,
        sha256: [u8; 32],
    },
    NotModified {
        sha256: [u8; 32],
    },
    NotShared,
    InvalidPath {
        rule: String,
    },
    TooLarge {
        limit: u64,
    },
}

/// Why a listing or a fetch did not yield its answer. The file's bytes never appear in
/// one, and neither does a status word the peer made up.
#[derive(Debug)]
pub(crate) enum FetchError {
    Transport(R3Error),
    /// The peer let the request through but serves no files.
    NotServed,
    /// The reply is not the shape this Coyote reads; names the key that broke it.
    Malformed(&'static str),
    /// The peer sent more than `MAX_FETCH_FILE_BYTES`; the bytes were discarded.
    Oversize {
        len: usize,
    },
    /// The bytes do not hash to what the peer said they would.
    Corrupt,
    UnknownStatus,
    Stage(StageError),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(err) => write!(f, "{err}"),
            Self::NotServed => write!(f, "peer does not share files"),
            Self::Malformed(key) => write!(f, "the peer's reply could not be read: `{key}`"),
            Self::Oversize { len } => write!(
                f,
                "the peer sent {len} bytes, above the {MAX_FETCH_FILE_BYTES}-byte limit; discarded"
            ),
            Self::Corrupt => write!(f, "the peer's bytes do not match the hash it sent"),
            Self::UnknownStatus => write!(f, "peer sent an unknown status"),
            Self::Stage(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for FetchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(err) => Some(err),
            Self::Stage(err) => Some(err),
            _ => None,
        }
    }
}

/// What this node last heard from each peer, keyed by destination: the status card and
/// the paths it listed, so a REPL or tool can show them without another round trip.
/// Bounded per kind, oldest insert out.
#[derive(Default)]
pub(crate) struct PeerMemory {
    cards: Recent<StatusCard>,
    lists: Recent<Vec<String>>,
}

impl PeerMemory {
    pub(crate) fn remember_card(&self, destination_hex: &str, card: StatusCard) {
        self.cards
            .remember(destination_hex, card, LAST_CARD_CACHE_PEERS);
    }

    fn remember_list(&self, destination_hex: &str, paths: Vec<String>) {
        self.lists
            .remember(destination_hex, paths, LAST_LIST_CACHE_PEERS);
    }
}

struct Recent<T> {
    entries: parking_lot::Mutex<VecDeque<(String, T)>>,
}

impl<T> Default for Recent<T> {
    fn default() -> Self {
        Self {
            entries: parking_lot::Mutex::new(VecDeque::new()),
        }
    }
}

impl<T: Clone> Recent<T> {
    fn remember(&self, key: &str, value: T, capacity: usize) {
        let mut entries = self.entries.lock();
        entries.retain(|(known, _)| known != key);
        entries.push_back((key.to_string(), value));
        while entries.len() > capacity {
            entries.pop_front();
        }
    }

    #[cfg(test)]
    fn get(&self, key: &str) -> Option<T> {
        self.entries
            .lock()
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, value)| value.clone())
    }
}

impl MeshRuntime {
    /// One page of the files `destination` shares with this node, answered live or not at
    /// all. The paths of a page that was read are remembered for `last_list`.
    pub(crate) async fn list_shares(
        &self,
        destination: &DestinationDesc,
        prefix: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<SharesPage, FetchError> {
        let body = Value::Map(vec![
            (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
            (
                Value::from("prefix"),
                prefix.map_or(Value::Nil, Value::from),
            ),
            (
                Value::from("cursor"),
                cursor.map_or(Value::Nil, Value::from),
            ),
        ]);
        let options = RequestOptions {
            request_timeout: PEER_REQUEST_TIMEOUT,
            link_timeout: PEER_LINK_TIMEOUT,
        };
        let outcome = self
            .request(destination, LIST_PATH, body, options)
            .await
            .map_err(FetchError::Transport)?;
        if DispatchError::from_value(&outcome.value).is_some() {
            return Err(FetchError::NotServed);
        }
        let destination_hex = destination.address_hash.to_hex_string();
        let page = SharesPage::from_value(&outcome.value, short(&destination_hex))?;
        self.memory().remember_list(
            &destination_hex,
            page.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect(),
        );
        Ok(page)
    }

    /// Fetches `path` from `destination` and stages the bytes under this instance's inbox,
    /// or returns the peer's typed answer. `if_sha256` is the hash this node already
    /// holds, so an unchanged file comes back as `NotModified` without its bytes.
    pub(crate) async fn fetch_file(
        &self,
        destination: &DestinationDesc,
        path: &str,
        if_sha256: Option<[u8; 32]>,
    ) -> Result<Fetched, FetchError> {
        let wire_path = match WirePath::parse(path) {
            Ok(wire_path) => wire_path,
            Err(invalid) => {
                return Ok(Fetched::InvalidPath {
                    rule: invalid.rule.to_string(),
                });
            }
        };
        let body = Value::Map(vec![
            (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
            (Value::from("path"), Value::from(path)),
            (
                Value::from("if_sha256"),
                if_sha256.map_or(Value::Nil, |hash| Value::Binary(hash.to_vec())),
            ),
        ]);
        let options = RequestOptions {
            request_timeout: FILE_FETCH_REQUEST_TIMEOUT,
            link_timeout: PEER_LINK_TIMEOUT,
        };
        let outcome = self
            .request(destination, FETCH_PATH, body, options)
            .await
            .map_err(FetchError::Transport)?;
        if DispatchError::from_value(&outcome.value).is_some() {
            return Err(FetchError::NotServed);
        }
        let entries = outcome.value.as_map().ok_or(FetchError::Malformed("map"))?;
        if field(entries, "v").and_then(Value::as_u64) != Some(PEER_WIRE_VERSION) {
            return Err(FetchError::Malformed("v"));
        }
        let status = field(entries, "status")
            .and_then(text_of)
            .ok_or(FetchError::Malformed("status"))?;
        match status {
            "ok" => {
                let bytes = field(entries, "bytes")
                    .and_then(bin_of)
                    .ok_or(FetchError::Malformed("bytes"))?;
                if bytes.len() as u64 > MAX_FETCH_FILE_BYTES {
                    return Err(FetchError::Oversize { len: bytes.len() });
                }
                let size = field(entries, "size")
                    .and_then(Value::as_u64)
                    .ok_or(FetchError::Malformed("size"))?;
                if size != bytes.len() as u64 {
                    return Err(FetchError::Malformed("size"));
                }
                let sha256 = field(entries, "sha256")
                    .and_then(bin32_of)
                    .ok_or(FetchError::Malformed("sha256"))?;
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                if digest != sha256 {
                    return Err(FetchError::Corrupt);
                }
                let staging = self.inbox_staging();
                let peer_destination = destination.address_hash.to_hex_string();
                let bytes = bytes.to_vec();
                let staged = tokio::task::spawn_blocking(move || {
                    staging.stage(&peer_destination, &wire_path, &digest, &bytes)
                })
                .await
                .map_err(|err| FetchError::Stage(StageError::Io(err.into())))?
                .map_err(FetchError::Stage)?;
                Ok(Fetched::Staged {
                    path: staged,
                    size,
                    sha256: digest,
                })
            }
            "not_modified" => {
                let sha256 = field(entries, "sha256")
                    .and_then(bin32_of)
                    .ok_or(FetchError::Malformed("sha256"))?;
                Ok(Fetched::NotModified { sha256 })
            }
            "not_shared" => Ok(Fetched::NotShared),
            "invalid_path" => {
                let rule = field(entries, "rule")
                    .and_then(text_of)
                    .ok_or(FetchError::Malformed("rule"))?;
                Ok(Fetched::InvalidPath {
                    rule: rule.chars().take(RULE_MAX_CHARS).collect(),
                })
            }
            "too_large" => {
                let limit = field(entries, "limit")
                    .and_then(Value::as_u64)
                    .ok_or(FetchError::Malformed("limit"))?;
                Ok(Fetched::TooLarge { limit })
            }
            _ => Err(FetchError::UnknownStatus),
        }
    }

    /// The status card `destination` last answered with, if it is among the last
    /// `LAST_CARD_CACHE_PEERS` peers asked.
    #[cfg(all(test, unix))]
    pub(crate) fn last_card(&self, destination_hex: &str) -> Option<StatusCard> {
        self.memory().cards.get(destination_hex)
    }

    /// The paths on the last listing page `destination` answered with, if it is among
    /// the last `LAST_LIST_CACHE_PEERS` peers listed.
    #[cfg(all(test, unix))]
    pub(crate) fn last_list(&self, destination_hex: &str) -> Option<Vec<String>> {
        self.memory().lists.get(destination_hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mesh_config::MAX_FETCH_FILE_BYTES;
    use crate::hooks::HookEvent;
    use crate::mesh::card::build_card;
    use crate::mesh::events::{RecordingHookSink, env_value, one_fire};
    use crate::mesh::mesh_config_dir;
    use crate::mesh::r3::{FETCH_PATH, LIST_PATH, PathHash, RequestId, ResponseFrame, SizeBranch};
    use crate::mesh::test_support::TempDir;
    use crate::mesh::wire_path::WIRE_PATH_MAX_BYTES;
    #[cfg(unix)]
    use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};
    use rand_core::OsRng;
    use rns_transport::destination::link::LinkId;
    use rns_transport::hash::AddressHash;
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use std::fs;

    const GRANT_ID: &str = "0123456789abcdef";

    struct TestSource {
        root: Option<PathBuf>,
        serving: Option<Arc<FetchServing>>,
    }

    impl ShareSource for TestSource {
        fn share_root(&self) -> Option<PathBuf> {
            self.root.clone()
        }

        fn serving(&self) -> Option<Arc<FetchServing>> {
            self.serving.clone()
        }
    }

    struct FailingReader;

    impl FileReader for FailingReader {
        fn read_bounded(&self, _file: fs::File, _limit: u64) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::other("the disk went away"))
        }
    }

    /// Hands back `limit` bytes whatever the file holds, as a file that grew past the
    /// limit between the stat and the read would.
    struct GrowingReader;

    impl FileReader for GrowingReader {
        fn read_bounded(&self, _file: fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
            Ok(vec![b'x'; limit as usize])
        }
    }

    /// A share root under a temp dir with the global share list beside it, served to one
    /// peer through handlers that see the root and the serving state.
    struct Fixture {
        _tmp: TempDir,
        root: PathBuf,
        serving: Arc<FetchServing>,
        source: Arc<TestSource>,
        identity: TransportIdentity,
        destination: String,
    }

    impl Fixture {
        fn new(tag: &str, max_bytes: u64, allow: &[&str]) -> Self {
            Self::with_inbox_dir(tag, max_bytes, allow, None)
        }

        /// `inbox_dir` is relative to the share root, standing in for a
        /// `mesh.fetch.inbox_dir` the operator pointed inside a shared workspace.
        fn with_inbox_dir(
            tag: &str,
            max_bytes: u64,
            allow: &[&str],
            inbox_dir: Option<&str>,
        ) -> Self {
            let tmp = TempDir::new(tag);
            let root = tmp.path.join("workspace");
            fs::create_dir_all(&root).unwrap();
            write_allow(&tmp, allow);
            let serving = serving_for(&tmp, max_bytes, inbox_dir.map(|dir| root.join(dir)));
            let source = Arc::new(TestSource {
                root: Some(root.clone()),
                serving: Some(Arc::clone(&serving)),
            });
            let (identity, destination) = anyone();
            Self {
                _tmp: tmp,
                root,
                serving,
                source,
                identity,
                destination,
            }
        }

        fn file(&self, relative: &str, bytes: &[u8]) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn weak_source(&self) -> Weak<dyn ShareSource> {
            Arc::downgrade(&self.source) as Weak<dyn ShareSource>
        }

        fn admitted(&self, path: &str, body: Value) -> AdmittedRequest {
            admitted(path, body, &self.identity, &self.destination)
        }

        async fn fetch(&self, path: &str, if_sha256: Option<[u8; 32]>) -> Reply {
            self.fetch_with(FetchHandler::new(self.weak_source()), path, if_sha256)
                .await
        }

        async fn fetch_with(
            &self,
            handler: FetchHandler,
            path: &str,
            if_sha256: Option<[u8; 32]>,
        ) -> Reply {
            handler
                .handle(self.admitted(FETCH_PATH, fetch_body(path, if_sha256)))
                .await
        }

        async fn list(&self, prefix: Option<&str>, cursor: Option<&str>) -> Value {
            let reply = ListHandler::new(self.weak_source())
                .handle(self.admitted(LIST_PATH, list_body(prefix, cursor)))
                .await;
            value_of(reply)
        }

        fn grant(&self, paths: &[&str]) {
            let paths: Vec<String> = paths.iter().map(|path| (*path).to_string()).collect();
            self.serving
                .grants()
                .grant(GRANT_ID, &self.destination, &paths, None, SystemTime::now())
                .unwrap();
        }
    }

    fn serving_for(tmp: &TempDir, max_bytes: u64, inbox_dir: Option<PathBuf>) -> Arc<FetchServing> {
        let cache_dir = tmp.path.join("cache");
        Arc::new(FetchServing::new(
            tmp.path.join("config"),
            cache_dir.clone(),
            inbox_dir,
            max_bytes,
            GrantStore::new(&cache_dir, "inst"),
            MeshHooks::default(),
        ))
    }

    /// Writes the global share list allowing `patterns`; none leaves the list absent.
    fn write_allow(tmp: &TempDir, patterns: &[&str]) {
        if patterns.is_empty() {
            return;
        }
        let path = mesh_config_dir(&tmp.path.join("config")).join("shares.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = String::from("version: 1\nallow:\n");
        for pattern in patterns {
            text.push_str(&format!("- pattern: '{pattern}'\n"));
        }
        fs::write(path, text).unwrap();
    }

    fn anyone() -> (TransportIdentity, String) {
        (
            TransportIdentity::new_from_rand(OsRng),
            hex_lower(&[0x2b; 16]),
        )
    }

    fn admitted(
        path: &str,
        body: Value,
        identity: &TransportIdentity,
        destination: &str,
    ) -> AdmittedRequest {
        AdmittedRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: *identity.as_identity(),
            destination_hash: AddressHash::new_from_hex_string(destination).unwrap(),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(path),
            requested_at: 1_700_000_000.0,
            body,
            branch: SizeBranch::Packet,
        }
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    fn fetch_body(path: &str, if_sha256: Option<[u8; 32]>) -> Value {
        map(vec![
            ("v", Value::from(PEER_WIRE_VERSION)),
            ("path", Value::from(path)),
            (
                "if_sha256",
                if_sha256.map_or(Value::Nil, |hash| Value::Binary(hash.to_vec())),
            ),
        ])
    }

    fn list_body(prefix: Option<&str>, cursor: Option<&str>) -> Value {
        map(vec![
            ("v", Value::from(PEER_WIRE_VERSION)),
            ("prefix", prefix.map_or(Value::Nil, Value::from)),
            ("cursor", cursor.map_or(Value::Nil, Value::from)),
        ])
    }

    fn value_of(reply: Reply) -> Value {
        match reply {
            Reply::Value(value) | Reply::Settled { value, .. } => value,
            Reply::Code(code) => panic!("refused with {code:?}"),
            Reply::Silent => panic!("answered with silence"),
        }
    }

    /// The value of a settled reply once the server has reported it sent.
    fn sent(reply: Reply) -> Value {
        let Reply::Settled { value, settlement } = reply else {
            panic!("an ok reply settles on send");
        };
        settlement.sent();
        value
    }

    fn field_of<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        field(value.as_map().unwrap(), key)
    }

    fn status_of(reply: &Reply) -> &str {
        let value = match reply {
            Reply::Value(value) | Reply::Settled { value, .. } => value,
            Reply::Code(code) => panic!("refused with {code:?}"),
            Reply::Silent => panic!("answered with silence"),
        };
        field_of(value, "status").and_then(text_of).unwrap()
    }

    fn entry_paths(page: &Value) -> Vec<String> {
        field_of(page, "entries")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|entry| {
                field_of(entry, "path")
                    .and_then(text_of)
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    fn next_of(page: &Value) -> Option<String> {
        field_of(page, "next").and_then(text_of).map(str::to_string)
    }

    fn encoded(reply: Reply) -> Vec<u8> {
        ResponseFrame {
            request_id: RequestId::from([1u8; 16]),
            data: value_of(reply),
        }
        .encode()
    }

    fn sha256_of(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    #[tokio::test]
    async fn only_the_effective_share_set_is_listed() {
        let fx = Fixture::new("fetch-list-effective", MAX_FETCH_FILE_BYTES, &["docs/a.md"]);
        fx.file("docs/a.md", b"a");
        for n in 1..=9 {
            fx.file(&format!("docs/b{n}.md"), b"b");
        }
        fx.file("src/x.rs", b"x");

        let page = fx.list(None, None).await;

        assert_eq!(entry_paths(&page), ["docs/a.md"]);
        assert_eq!(next_of(&page), None);
    }

    /// Four hundred entries fit the share set's page but not the wire: the page is cut
    /// where the next entry would overflow the frame, and the cursor resumes after it.
    #[tokio::test]
    async fn a_list_page_is_cut_by_encoded_bytes_before_the_entry_count() {
        let fx = Fixture::new("fetch-list-cut", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        let deep = ["a", "b", "c", "d"].map(|c| c.repeat(200)).join("/");
        let all: Vec<String> = (0..400).map(|n| format!("docs/{deep}/{n:03}.md")).collect();
        for path in &all {
            assert!(path.len() <= WIRE_PATH_MAX_BYTES);
            fx.file(path, b"x");
        }

        let first = fx.list(None, None).await;

        let frame = ResponseFrame {
            request_id: RequestId::from([1u8; 16]),
            data: first.clone(),
        }
        .encode();
        assert!(frame.len() <= MAX_R3_PAYLOAD_BYTES, "{} bytes", frame.len());
        let kept = entry_paths(&first);
        assert!(
            !kept.is_empty() && kept.len() < all.len(),
            "{} entries",
            kept.len()
        );
        assert_eq!(kept, all[..kept.len()]);
        let cursor = next_of(&first).unwrap();
        assert_eq!(cursor, shares::list_cursor(kept.last().unwrap()));

        let second = fx.list(None, Some(&cursor)).await;

        assert_eq!(entry_paths(&second), all[kept.len()..]);
        assert_eq!(next_of(&second), None);
    }

    #[tokio::test]
    async fn a_malformed_list_or_fetch_body_is_refused_with_invalid_data() {
        let fx = Fixture::new("fetch-malformed", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        fx.file("docs/a.md", b"a");
        let v = || ("v", Value::from(PEER_WIRE_VERSION));
        let fetches = [
            map(vec![
                ("v", Value::from(2u64)),
                ("path", Value::from("docs/a.md")),
            ]),
            Value::Nil,
            map(vec![v(), ("path", Value::from(5u64))]),
            map(vec![
                v(),
                ("path", Value::from("docs/a.md")),
                ("if_sha256", Value::Binary(vec![0; 31])),
            ]),
        ];
        for body in fetches {
            let reply = FetchHandler::new(fx.weak_source())
                .handle(fx.admitted(FETCH_PATH, body.clone()))
                .await;
            assert!(
                matches!(reply, Reply::Code(RefusalCode::InvalidData)),
                "{body:?}"
            );
        }
        let lists = [
            map(vec![("v", Value::from(2u64))]),
            Value::Nil,
            map(vec![v(), ("cursor", Value::from("c".repeat(65)))]),
            map(vec![v(), ("prefix", Value::from(3u64))]),
        ];
        for body in lists {
            let reply = ListHandler::new(fx.weak_source())
                .handle(fx.admitted(LIST_PATH, body.clone()))
                .await;
            assert!(
                matches!(reply, Reply::Code(RefusalCode::InvalidData)),
                "{body:?}"
            );
        }

        let lenient_fetch = map(vec![
            v(),
            ("path", Value::from("docs/a.md")),
            ("colour", Value::from("blue")),
        ]);
        let reply = FetchHandler::new(fx.weak_source())
            .handle(fx.admitted(FETCH_PATH, lenient_fetch))
            .await;
        assert_eq!(status_of(&reply), "ok");
        let lenient_list = map(vec![v(), ("colour", Value::from("blue"))]);
        let reply = ListHandler::new(fx.weak_source())
            .handle(fx.admitted(LIST_PATH, lenient_list))
            .await;
        assert_eq!(entry_paths(&value_of(reply)), ["docs/a.md"]);
    }

    #[tokio::test]
    async fn not_shared_is_byte_identical_for_a_nonexistent_and_an_unshared_path() {
        let fx = Fixture::new(
            "fetch-not-shared-alike",
            MAX_FETCH_FILE_BYTES,
            &["docs/**", ".env"],
        );
        fx.file("src/x.rs", b"x");
        fx.file(".env", b"SECRET=1");

        let mut encodings = Vec::new();
        for path in ["docs/missing.md", "src/x.rs", ".env"] {
            let reply = fx.fetch(path, None).await;
            assert_eq!(status_of(&reply), "not_shared", "{path}");
            encodings.push(encoded(reply));
        }

        assert_eq!(encodings[0], encodings[1]);
        assert_eq!(encodings[0], encodings[2]);
    }

    #[tokio::test]
    async fn invalid_path_is_answered_before_the_filesystem_is_touched() {
        let fx = Fixture::new("fetch-invalid-first", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        fs::remove_dir(&fx.root).unwrap();

        for (path, rule) in [("../x", "segment"), ("a\\b", "backslash")] {
            let reply = fx.fetch(path, None).await;
            assert_eq!(status_of(&reply), "invalid_path", "{path}");
            assert_eq!(
                field_of(&value_of(reply), "rule").and_then(text_of),
                Some(rule)
            );
        }

        assert!(!fx.root.exists());
    }

    #[tokio::test]
    async fn too_large_carries_the_local_limit_and_not_modified_carries_no_body() {
        let fx = Fixture::new("fetch-limit-and-not-modified", 16, &["docs/**"]);
        fx.file("docs/big.md", &[b'x'; 17]);
        let bytes = b"0123456789";
        fx.file("docs/small.md", bytes);

        let reply = fx.fetch("docs/big.md", None).await;
        assert_eq!(status_of(&reply), "too_large");
        assert_eq!(
            field_of(&value_of(reply), "limit").and_then(Value::as_u64),
            Some(16)
        );

        let reply = fx.fetch("docs/small.md", Some(sha256_of(bytes))).await;
        assert_eq!(status_of(&reply), "not_modified");
        let value = value_of(reply);
        assert_eq!(
            field_of(&value, "sha256"),
            Some(&Value::Binary(sha256_of(bytes).to_vec()))
        );
        assert_eq!(field_of(&value, "bytes"), None);
        assert_eq!(field_of(&value, "size"), None);

        let reply = fx.fetch("docs/small.md", Some([0; 32])).await;
        assert_eq!(status_of(&reply), "ok");
        let value = value_of(reply);
        assert_eq!(field_of(&value, "size").and_then(Value::as_u64), Some(10));
        assert_eq!(
            field_of(&value, "bytes"),
            Some(&Value::Binary(bytes.to_vec()))
        );
    }

    /// With the configured limit above the single-segment ceiling, the ceiling is what a
    /// request is held to and what `too_large` reports; a file exactly at it is served and
    /// its whole response frame fits one Resource segment.
    #[tokio::test]
    async fn a_file_above_the_single_segment_ceiling_is_too_large_with_that_limit() {
        let ceiling = usize::try_from(SINGLE_SEGMENT_FETCH_CEILING).unwrap();
        let fx = Fixture::new("fetch-segment-ceiling", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        fx.file("docs/over.bin", &vec![0x5a; ceiling + 1]);
        let at_ceiling = vec![0xa5; ceiling];
        fx.file("docs/at.bin", &at_ceiling);

        let reply = fx.fetch("docs/over.bin", None).await;
        assert_eq!(status_of(&reply), "too_large");
        assert_eq!(
            field_of(&value_of(reply), "limit").and_then(Value::as_u64),
            Some(SINGLE_SEGMENT_FETCH_CEILING)
        );

        let reply = fx.fetch("docs/at.bin", None).await;
        assert_eq!(status_of(&reply), "ok");
        let frame = ResponseFrame {
            request_id: RequestId::from([1u8; 16]),
            data: value_of(reply),
        }
        .encode();
        assert!(
            frame.len() <= MAX_EFFICIENT_SIZE,
            "a {}-byte frame does not fit one segment of {MAX_EFFICIENT_SIZE}",
            frame.len()
        );
    }

    /// The framing allowance behind the ceiling is not optimistic: the largest `ok` reply
    /// the ceiling admits encodes within one segment with the allowance to spare.
    #[test]
    fn an_ok_reply_at_the_ceiling_fits_one_resource_segment() {
        let bytes = vec![0xffu8; usize::try_from(SINGLE_SEGMENT_FETCH_CEILING).unwrap()];
        let digest = sha256_of(&bytes);
        let frame = ResponseFrame {
            request_id: RequestId::from([0xeeu8; 16]),
            data: status_reply(
                "ok",
                vec![
                    ("size", Value::from(bytes.len() as u64)),
                    ("sha256", Value::Binary(digest.to_vec())),
                    ("bytes", Value::Binary(bytes)),
                ],
            ),
        }
        .encode();
        assert!(frame.len() <= MAX_EFFICIENT_SIZE);
        assert!(MAX_EFFICIENT_SIZE - frame.len() < OK_REPLY_FRAMING_BYTES);
    }

    /// `fetch_file` waits `FILE_FETCH_REQUEST_TIMEOUT` for its reply where `list_shares`
    /// waits a status round trip: a file is a Resource that takes minutes on a slow
    /// interface. Pinned here until the spec's constants table carries the row.
    #[cfg(unix)]
    #[test]
    fn a_file_fetch_waits_two_minutes_where_a_listing_waits_a_round_trip() {
        assert_eq!(FILE_FETCH_REQUEST_TIMEOUT, Duration::from_secs(120));
        assert_eq!(PEER_REQUEST_TIMEOUT, Duration::from_secs(15));
        assert!(FILE_FETCH_REQUEST_TIMEOUT > PEER_REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn a_file_that_grew_past_the_limit_after_the_stat_is_too_large() {
        let fx = Fixture::new("fetch-grew", 16, &["docs/**"]);
        fx.file("docs/a.md", b"small");

        let handler = FetchHandler::with_reader(fx.weak_source(), Arc::new(GrowingReader));
        let reply = fx.fetch_with(handler, "docs/a.md", None).await;

        assert_eq!(status_of(&reply), "too_large");
        assert_eq!(
            field_of(&value_of(reply), "limit").and_then(Value::as_u64),
            Some(16)
        );
    }

    #[tokio::test]
    async fn a_one_off_grant_is_consumed_by_the_fetch_and_the_second_fetch_is_not_shared() {
        let fx = Fixture::new("fetch-grant-once", MAX_FETCH_FILE_BYTES, &[]);
        fx.file("src/secret.rs", b"s");
        fx.grant(&["src/secret.rs"]);

        let first = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&first), "ok");
        sent(first);

        let second = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&second), "not_shared");
    }

    #[tokio::test]
    async fn a_grant_of_three_paths_serves_each_once_and_a_fourth_fetch_of_any_is_not_shared() {
        let fx = Fixture::new("fetch-grant-three", MAX_FETCH_FILE_BYTES, &[]);
        let paths = ["src/a.rs", "src/b.rs", "src/c.rs"];
        for path in paths {
            fx.file(path, path.as_bytes());
        }
        fx.grant(&paths);

        for path in paths {
            let reply = fx.fetch(path, None).await;
            assert_eq!(status_of(&reply), "ok", "{path}");
            sent(reply);
        }
        for path in paths {
            assert_eq!(
                status_of(&fx.fetch(path, None).await),
                "not_shared",
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn a_grant_use_is_refunded_when_the_read_fails_after_is_served() {
        let fx = Fixture::new("fetch-grant-refund-read", MAX_FETCH_FILE_BYTES, &[]);
        fx.file("src/secret.rs", b"s");
        fx.grant(&["src/secret.rs"]);

        let failing = FetchHandler::with_reader(fx.weak_source(), Arc::new(FailingReader));
        let reply = fx.fetch_with(failing, "src/secret.rs", None).await;
        assert_eq!(status_of(&reply), "not_shared");

        let reply = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&reply), "ok");
        sent(reply);
        let reply = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&reply), "not_shared");
    }

    #[tokio::test]
    async fn a_grant_use_is_refunded_when_the_settlement_is_dropped_unsent_and_kept_when_sent() {
        let fx = Fixture::new("fetch-grant-refund-send", MAX_FETCH_FILE_BYTES, &[]);
        fx.file("src/secret.rs", b"s");
        fx.grant(&["src/secret.rs"]);

        let first = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&first), "ok");
        drop(first);

        let second = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&second), "ok");
        sent(second);

        assert_eq!(
            status_of(&fx.fetch("src/secret.rs", None).await),
            "not_shared",
            "a sent reply keeps the use spent"
        );
    }

    /// A `FileReader` that holds the fetch open until the test lets it go, so the handler
    /// future can be dropped after `is_served` has spent the grant use.
    struct HeldReader {
        reached: tokio::sync::mpsc::UnboundedSender<()>,
        release: parking_lot::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl FileReader for HeldReader {
        fn read_bounded(&self, file: fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
            let _ = self.reached.send(());
            if let Some(release) = self.release.lock().take() {
                let _ = release.blocking_recv();
            }
            ReadToEnd.read_bounded(file, limit)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grant_use_is_refunded_when_the_handler_future_is_dropped_before_it_replies() {
        use std::time::Duration;
        let fx = Fixture::new("fetch-grant-refund-dropped", MAX_FETCH_FILE_BYTES, &[]);
        fx.file("src/secret.rs", b"s");
        fx.grant(&["src/secret.rs"]);
        let (reached, mut reached_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let held = FetchHandler::with_reader(
            fx.weak_source(),
            Arc::new(HeldReader {
                reached,
                release: parking_lot::Mutex::new(Some(release_rx)),
            }),
        );

        let mut handling = held.handle(fx.admitted(FETCH_PATH, fetch_body("src/secret.rs", None)));
        tokio::select! {
            _ = &mut handling => panic!("the read is held until the test releases it"),
            reached = reached_rx.recv() => assert!(reached.is_some()),
        }
        drop(handling);
        release.send(()).unwrap();
        let refunded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if status_of(&fx.fetch("src/secret.rs", None).await) == "ok" {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(refunded.is_ok(), "the dropped fetch refunded its grant use");
    }

    #[tokio::test]
    async fn a_served_fetch_fires_mesh_fetch_served_with_peer_size_and_hash_prefix_and_no_path() {
        let fx = Fixture::new("fetch-hook", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        let bytes = b"# docs\n";
        fx.file("docs/a.md", bytes);
        let sink = RecordingHookSink::attach(&fx.serving.hooks);

        let unsent = fx.fetch("docs/a.md", None).await;
        assert_eq!(status_of(&unsent), "ok");
        drop(unsent);
        assert!(sink.drain().is_empty(), "an unsent reply fires nothing");

        sent(fx.fetch("docs/a.md", None).await);

        let envs = one_fire(&sink, HookEvent::MeshFetchServed);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(
                fx.identity
                    .as_identity()
                    .address_hash
                    .to_hex_string()
                    .as_str()
            )
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(fx.destination.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_SIZE"),
            Some(bytes.len().to_string().as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_HASH_PREFIX"),
            Some(&hex_lower(&sha256_of(bytes))[..8])
        );
        assert!(
            envs.iter().all(|(_, value)| !value.contains("docs/")),
            "{envs:?}"
        );

        let reply = fx.fetch("docs/a.md", Some(sha256_of(bytes))).await;
        assert_eq!(status_of(&reply), "not_modified");
        assert!(sink.drain().is_empty());
    }

    /// The probe cannot write under a read-only root, so the root serves nothing and
    /// the operator is told once. Root writes anywhere, so under root there is nothing
    /// to show.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_root_that_cannot_be_probed_serves_nothing() {
        use std::os::unix::fs::PermissionsExt;
        install_log_collector();
        let fx = Fixture::new("fetch-unprobeable-root", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        fx.file("docs/a.md", b"a");
        let set_mode = |mode| {
            fs::set_permissions(&fx.root, fs::Permissions::from_mode(mode)).unwrap();
        };
        set_mode(0o500);
        if fs::write(fx.root.join("written-despite-the-mode"), b"").is_ok() {
            set_mode(0o700);
            return;
        }
        let sharing_off_warns = || {
            warn_snapshot()
                .iter()
                .filter(|line| line.starts_with("Mesh file sharing is off for this workspace"))
                .count()
        };
        let before = sharing_off_warns();

        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert!(entry_paths(&fx.list(None, None).await).is_empty());
        assert_eq!(sharing_off_warns(), before + 1);

        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert_eq!(
            sharing_off_warns(),
            before + 1,
            "the failed probe is remembered"
        );
        set_mode(0o700);
    }

    /// A share list the node refuses is the operator's problem, said once; the peers whose
    /// requests keep finding it broken do not get to repeat the warning. Fixing the file
    /// and breaking it again warns again.
    // Serialized with its sibling below: both count the marker-less
    // "Mesh share list still refused" debug line in the binary-wide log buffer.
    #[cfg(unix)]
    #[serial_test::serial(mesh_share_list_logs)]
    #[tokio::test]
    async fn a_refused_share_list_is_warned_about_once_per_root_not_per_request() {
        install_log_collector();
        let fx = Fixture::new(
            "fetch-refused-list-once",
            MAX_FETCH_FILE_BYTES,
            &["docs/**"],
        );
        fx.file("docs/a.md", b"a");
        let shares = mesh_config_dir(&fx._tmp.path.join("config")).join("shares.yaml");
        // The buffer is shared by every test in the binary; the fixture's directory name is
        // the marker that keeps a sibling's identical refusal out of the count.
        let marker = fx._tmp.path.to_string_lossy().into_owned();
        let refusal_warns = || {
            warn_snapshot()
                .iter()
                .filter(|line| {
                    line.contains(&marker) && line.ends_with("Nothing is shared until then.")
                })
                .count()
        };
        let before = refusal_warns();

        fs::write(&shares, "version: 1\nallow: not-a-list\n").unwrap();
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert!(entry_paths(&fx.list(None, None).await).is_empty());
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert_eq!(refusal_warns(), before + 1, "three requests, one warning");

        fs::write(&shares, "version: 1\nallow:\n- pattern: 'docs/**'\n").unwrap();
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "ok");
        assert_eq!(refusal_warns(), before + 1);

        fs::write(&shares, "version: 1\nallow: still-not-a-list\n").unwrap();
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert_eq!(
            refusal_warns(),
            before + 2,
            "a list broken again is warned about again"
        );
    }

    /// A `mesh.fetch.inbox_dir` pointed inside a shared workspace: what peers staged
    /// there is neither served nor listed under `allow **`, while the rest of the tree is.
    #[tokio::test]
    async fn a_file_under_a_configured_inbox_dir_is_never_served_or_listed() {
        let fx = Fixture::with_inbox_dir(
            "fetch-protected-inbox",
            MAX_FETCH_FILE_BYTES,
            &["**"],
            Some("inbox"),
        );
        fx.file("inbox/inst/0123abcd/docs/a.md", b"theirs");
        fx.file("README.md", b"ours");

        assert_eq!(
            status_of(&fx.fetch("inbox/inst/0123abcd/docs/a.md", None).await),
            "not_shared"
        );
        assert_eq!(status_of(&fx.fetch("README.md", None).await), "ok");
        assert_eq!(entry_paths(&fx.list(None, None).await), ["README.md"]);
    }

    /// A `not_modified` answer is a fetch: the grant use it spent stays spent, so the
    /// peer that already holds the bytes cannot keep the grant alive by re-asking.
    #[tokio::test]
    async fn usage_probe_a_not_modified_answer_under_a_grant_keeps_the_use_spent() {
        let fx = Fixture::new("fetch-probe-grant-not-modified", MAX_FETCH_FILE_BYTES, &[]);
        let bytes = b"granted once";
        fx.file("src/secret.rs", bytes);
        fx.grant(&["src/secret.rs"]);

        let reply = fx.fetch("src/secret.rs", Some(sha256_of(bytes))).await;
        assert_eq!(status_of(&reply), "not_modified");
        assert!(
            matches!(reply, Reply::Value(_)),
            "not_modified carries nothing to settle"
        );

        assert_eq!(
            status_of(&fx.fetch("src/secret.rs", None).await),
            "not_shared",
            "the use spent on the not_modified answer is not paid back"
        );
    }

    /// A reader that hands back more bytes than the file had when `is_served` stat'ed
    /// it, standing in for a file that grew between the stat and the read.
    struct GrownReader(usize);

    impl FileReader for GrownReader {
        fn read_bounded(&self, _file: fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
            Ok(vec![b'g'; self.0.min(usize::try_from(limit).unwrap())])
        }
    }

    /// A file that grew past the limit between the stat and the read is refused
    /// `too_large` with the applied limit, and the grant use `is_served` spent comes back,
    /// exactly once: the next fetch is served and the one after that is not.
    #[tokio::test]
    async fn usage_probe_a_grant_use_is_refunded_when_the_file_grew_past_the_limit_before_the_read()
    {
        let limit = 4;
        let fx = Fixture::new("fetch-probe-grant-grew", limit, &[]);
        fx.file("src/secret.rs", b"abc");
        fx.grant(&["src/secret.rs"]);

        let grown = FetchHandler::with_reader(
            fx.weak_source(),
            Arc::new(GrownReader(usize::try_from(limit).unwrap() + 1)),
        );
        let reply = fx.fetch_with(grown, "src/secret.rs", None).await;
        assert_eq!(status_of(&reply), "too_large");
        assert_eq!(
            field_of(&value_of(reply), "limit").and_then(Value::as_u64),
            Some(limit)
        );

        let reply = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&reply), "ok", "the use was paid back");
        sent(reply);
        assert_eq!(
            status_of(&fx.fetch("src/secret.rs", None).await),
            "not_shared",
            "paid back once, not twice"
        );
    }

    /// The warn-once rule keys on the refusal text while it stands, not on the file: a
    /// list fixed and then broken again with the very same mistake is a new refusal and
    /// is warned about again, and the requests that keep finding it broken in between
    /// say so at `debug!`, never at `warn!`.
    // Serialized with `a_refused_share_list_is_warned_about_once_per_root_not_per_request`:
    // the "still refused" debug line carries no marker, so only one of them may count it.
    #[cfg(unix)]
    #[serial_test::serial(mesh_share_list_logs)]
    #[tokio::test]
    async fn usage_probe_a_list_broken_again_with_the_same_text_warns_again_and_repeats_only_at_debug()
     {
        install_log_collector();
        let fx = Fixture::new(
            "fetch-probe-refused-list-same-text",
            MAX_FETCH_FILE_BYTES,
            &["docs/**"],
        );
        fx.file("docs/a.md", b"a");
        let shares = mesh_config_dir(&fx._tmp.path.join("config")).join("shares.yaml");
        // The warning names the share file, so the fixture's directory keeps a sibling's
        // identical refusal out of the count.
        let marker = fx._tmp.path.to_string_lossy().into_owned();
        let refusal_warns = || {
            warn_snapshot()
                .iter()
                .filter(|line| {
                    line.contains(&marker) && line.ends_with("Nothing is shared until then.")
                })
                .count()
        };
        let still_refused = || {
            debug_snapshot()
                .iter()
                .filter(|line| line.contains("Mesh share list still refused"))
                .count()
        };
        let (warns_before, debugs_before) = (refusal_warns(), still_refused());
        let broken = "version: 1\nallow: not-a-list\n";

        fs::write(&shares, broken).unwrap();
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert_eq!(refusal_warns(), warns_before + 1);
        assert_eq!(
            still_refused(),
            debugs_before,
            "the first refusal is the warning"
        );

        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert!(entry_paths(&fx.list(None, None).await).is_empty());
        assert_eq!(refusal_warns(), warns_before + 1);
        assert_eq!(
            still_refused(),
            debugs_before + 2,
            "each request that finds the same refusal says so at debug"
        );

        fs::write(&shares, "version: 1\nallow:\n- pattern: 'docs/**'\n").unwrap();
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "ok");

        fs::write(&shares, broken).unwrap();
        assert_eq!(status_of(&fx.fetch("docs/a.md", None).await), "not_shared");
        assert_eq!(
            refusal_warns(),
            warns_before + 2,
            "the same mistake made again after a fix is warned about again"
        );
        assert_eq!(still_refused(), debugs_before + 2);
    }

    /// `uses_left` of the one grant `store` holds for the fixture's path.
    fn uses_left(store: &GrantStore) -> u32 {
        let records = store.list().unwrap();
        let [record] = records.as_slice() else {
            panic!("one grant record, got {}", records.len());
        };
        let [granted] = record.paths.as_slice() else {
            panic!("one granted path, got {}", record.paths.len());
        };
        granted.uses_left
    }

    /// Installs a fork grant store on the fixture's serving state that already lent and
    /// spent one use of `path` to the fixture's peer, so a refund landing in the wrong
    /// store would show as a use the fork never lent coming back.
    fn rekey_to_a_fork_whose_use_is_spent(
        fx: &Fixture,
        path: &str,
        now: SystemTime,
    ) -> Arc<GrantStore> {
        fx.serving.rebind_grants("fork", now).unwrap();
        let fork = fx.serving.grants();
        fork.grant(GRANT_ID, &fx.destination, &[path.to_string()], None, now)
            .unwrap();
        let identity_hex = fx.identity.as_identity().address_hash.to_hex_string();
        let peer = crate::mesh::shares::PeerRef {
            identity: &identity_hex,
            destination: &fx.destination,
        };
        assert!(fork.consume(&peer, path, now).unwrap());
        assert_eq!(uses_left(&fork), 0);
        fork
    }

    /// A use spent before the node re-keyed is paid back to the store it came out of: a
    /// handler future dropped after `is_served` restores the original's grant, and the
    /// fork's grant for the same peer and path, which lent nothing to this fetch, stays
    /// spent. The next fetch is judged by the fork's store and refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_fetch_dropped_after_a_rekey_refunds_the_store_the_use_came_from() {
        use std::time::Duration;
        let fx = Fixture::new(
            "fetch-probe-refund-origin-dropped",
            MAX_FETCH_FILE_BYTES,
            &[],
        );
        fx.file("src/secret.rs", b"s");
        fx.grant(&["src/secret.rs"]);
        let original = fx.serving.grants();
        assert_eq!(uses_left(&original), 1);

        let (reached, mut reached_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let held = FetchHandler::with_reader(
            fx.weak_source(),
            Arc::new(HeldReader {
                reached,
                release: parking_lot::Mutex::new(Some(release_rx)),
            }),
        );
        let mut handling = held.handle(fx.admitted(FETCH_PATH, fetch_body("src/secret.rs", None)));
        tokio::select! {
            _ = &mut handling => panic!("the read is held until the test releases it"),
            reached = reached_rx.recv() => assert!(reached.is_some()),
        }
        assert_eq!(
            uses_left(&original),
            0,
            "is_served spent the original's use"
        );

        let now = SystemTime::now();
        let fork = rekey_to_a_fork_whose_use_is_spent(&fx, "src/secret.rs", now);
        assert!(!Arc::ptr_eq(&fork, &original));

        drop(handling);
        release.send(()).unwrap();
        let refunded = tokio::time::timeout(Duration::from_secs(5), async {
            while uses_left(&original) != 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            refunded.is_ok(),
            "the dropped fetch paid the use back to the original's store"
        );
        assert_eq!(
            uses_left(&fork),
            0,
            "the fork lent nothing to that fetch and is paid nothing"
        );
        assert_eq!(
            status_of(&fx.fetch("src/secret.rs", None).await),
            "not_shared",
            "the node now serves on the fork's grants, whose use is spent"
        );
    }

    /// The same, for the other failure before dispatch: an `ok` reply built on the
    /// original's grant and dropped unsent after the re-key refunds the original, not the
    /// fork.
    #[tokio::test]
    async fn usage_probe_an_ok_reply_dropped_unsent_after_a_rekey_refunds_the_store_the_use_came_from()
     {
        let fx = Fixture::new(
            "fetch-probe-refund-origin-unsent",
            MAX_FETCH_FILE_BYTES,
            &[],
        );
        fx.file("src/secret.rs", b"s");
        fx.grant(&["src/secret.rs"]);
        let original = fx.serving.grants();

        let reply = fx.fetch("src/secret.rs", None).await;
        assert_eq!(status_of(&reply), "ok");
        assert_eq!(uses_left(&original), 0);

        let now = SystemTime::now();
        let fork = rekey_to_a_fork_whose_use_is_spent(&fx, "src/secret.rs", now);

        drop(reply);
        assert_eq!(
            uses_left(&original),
            1,
            "the unsent reply paid the use back to the original's store"
        );
        assert_eq!(uses_left(&fork), 0, "the fork is paid nothing");

        // Re-keyed back to the original instance (a store over the same file), the
        // restored use serves one more fetch; a reply that goes out keeps it spent.
        fx.serving.rebind_grants("inst", now).unwrap();
        let reply = fx.fetch("src/secret.rs", None).await;
        assert_eq!(
            status_of(&reply),
            "ok",
            "served on the original's restored use"
        );
        sent(reply);
        assert_eq!(uses_left(&original), 0);
        assert_eq!(uses_left(&fork), 0);
    }

    #[tokio::test]
    async fn an_absent_snapshot_serves_nothing() {
        let tmp = TempDir::new("fetch-absent-snapshot");
        let (identity, destination) = anyone();
        let sources = [
            TestSource {
                root: None,
                serving: Some(serving_for(&tmp, MAX_FETCH_FILE_BYTES, None)),
            },
            TestSource {
                root: Some(tmp.path.clone()),
                serving: None,
            },
        ];
        for source in sources {
            let source = Arc::new(source);
            let weak = Arc::downgrade(&source) as Weak<dyn ShareSource>;
            let reply = FetchHandler::new(weak.clone())
                .handle(admitted(
                    FETCH_PATH,
                    fetch_body("docs/a.md", None),
                    &identity,
                    &destination,
                ))
                .await;
            assert_eq!(status_of(&reply), "not_shared");
            let reply = ListHandler::new(weak)
                .handle(admitted(
                    LIST_PATH,
                    list_body(None, None),
                    &identity,
                    &destination,
                ))
                .await;
            assert!(entry_paths(&value_of(reply)).is_empty());
        }

        let gone = Arc::downgrade(&Arc::new(TestSource {
            root: None,
            serving: None,
        })) as Weak<dyn ShareSource>;
        let reply = FetchHandler::new(gone.clone())
            .handle(admitted(
                FETCH_PATH,
                fetch_body("docs/a.md", None),
                &identity,
                &destination,
            ))
            .await;
        assert!(matches!(reply, Reply::Silent));
        let reply = ListHandler::new(gone)
            .handle(admitted(
                LIST_PATH,
                list_body(None, None),
                &identity,
                &destination,
            ))
            .await;
        assert!(matches!(reply, Reply::Silent));
    }

    #[test]
    fn last_card_and_last_list_are_bounded_and_keyed_by_destination() {
        let memory = PeerMemory::default();
        let card = build_card(None, None, None, None, None, &[], SystemTime::now());
        let key = |n: usize| format!("{n:032x}");
        let remember = |n: usize, paths: &[&str]| {
            memory.remember_card(&key(n), card.clone());
            memory.remember_list(
                &key(n),
                paths.iter().map(|path| (*path).to_string()).collect(),
            );
        };
        for n in 0..=LAST_CARD_CACHE_PEERS {
            remember(n, &["first"]);
        }

        assert_eq!(memory.cards.get(&key(0)), None);
        assert_eq!(memory.lists.get(&key(0)), None);
        assert!(memory.cards.get(&key(LAST_CARD_CACHE_PEERS)).is_some());
        assert_eq!(
            memory.lists.get(&key(LAST_LIST_CACHE_PEERS)),
            Some(vec!["first".to_string()])
        );
        assert_eq!(memory.cards.get("unknown"), None);
        assert_eq!(memory.lists.get("unknown"), None);

        remember(2, &["refreshed"]);
        assert_eq!(memory.cards.entries.lock().len(), LAST_CARD_CACHE_PEERS);
        assert_eq!(memory.lists.entries.lock().len(), LAST_LIST_CACHE_PEERS);
        remember(LAST_CARD_CACHE_PEERS + 1, &["later"]);
        remember(LAST_CARD_CACHE_PEERS + 2, &["later"]);

        assert_eq!(
            memory.cards.get(&key(3)),
            None,
            "the oldest untouched key goes"
        );
        assert!(memory.cards.get(&key(2)).is_some());
        assert_eq!(memory.lists.get(&key(3)), None);
        assert_eq!(
            memory.lists.get(&key(2)),
            Some(vec!["refreshed".to_string()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_peers_listing_page_is_capped_and_its_unreadable_entries_are_skipped() {
        let entry = |path: &str| {
            map(vec![
                ("path", Value::from(path)),
                ("size", Value::from(1u64)),
                ("sha256", Value::Binary(vec![7; 32])),
                ("mtime", Value::F64(1.0)),
            ])
        };
        let page = |version: u64, entries: Vec<Value>, next: Value| {
            map(vec![
                ("v", Value::from(version)),
                ("entries", Value::Array(entries)),
                ("next", next),
            ])
        };

        let good = SharesPage::from_value(
            &page(1, vec![entry("docs/a.md")], Value::from("c".repeat(32))),
            "dest8",
        )
        .unwrap();
        assert_eq!(
            good,
            SharesPage {
                entries: vec![SharedEntry {
                    path: "docs/a.md".into(),
                    size: 1,
                    sha256: [7; 32],
                    mtime: 1.0,
                }],
                next: Some("c".repeat(32)),
            }
        );

        let wrong_version = SharesPage::from_value(&page(2, Vec::new(), Value::Nil), "dest8");
        assert!(matches!(wrong_version, Err(FetchError::Malformed("v"))));
        let long_cursor =
            SharesPage::from_value(&page(1, Vec::new(), Value::from("c".repeat(65))), "dest8");
        assert!(matches!(long_cursor, Err(FetchError::Malformed("next"))));
        let no_entries = SharesPage::from_value(&map(vec![("v", Value::from(1u64))]), "dest8");
        assert!(matches!(no_entries, Err(FetchError::Malformed("entries"))));

        let many: Vec<Value> = (0..=LIST_PAGE_SIZE)
            .map(|n| entry(&format!("docs/{n}.md")))
            .collect();
        let capped = SharesPage::from_value(&page(1, many, Value::Nil), "dest8").unwrap();
        assert_eq!(capped.entries.len(), LIST_PAGE_SIZE);

        let mixed = page(
            1,
            vec![
                entry("../x"),
                map(vec![("path", Value::from("docs/no-size.md"))]),
                entry("docs/b.md"),
            ],
            Value::Nil,
        );
        let read = SharesPage::from_value(&mixed, "dest8").unwrap();
        let paths: Vec<&str> = read
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        assert_eq!(paths, ["docs/b.md"]);
    }

    /// The `/list` worked example: `prefix` narrows the page to one subtree, entries are
    /// sorted bytewise by path and carry `path`/`size`/`sha256`(bin32)/`mtime`(f64), the
    /// empty page is exactly `{v, entries: [], next: nil}`, and a directory is neither
    /// listed nor fetchable (its `not_shared` is byte-identical to a missing file's).
    #[tokio::test]
    async fn usage_probe_list_prefix_filters_sorts_and_shapes_entries_per_the_worked_example() {
        let fx = Fixture::new(
            "fetch-probe-list-shape",
            MAX_FETCH_FILE_BYTES,
            &["docs/**", "notes/**"],
        );
        let z = b"zz".to_vec();
        let a = vec![b'a'; 1204];
        let m = b"m".to_vec();
        fx.file("docs/z.md", &z);
        fx.file("notes/n.md", b"n");
        fx.file("docs/sub/m.md", &m);
        fx.file("docs/a.md", &a);
        fx.file("src/x.rs", b"x");
        let before = unix_secs_f64(SystemTime::now());

        let page = fx.list(Some("docs/"), None).await;

        assert_eq!(
            field_of(&page, "v").and_then(Value::as_u64),
            Some(PEER_WIRE_VERSION)
        );
        // `field` reads a nil as absent, so the raw map is what proves `next` is on the wire.
        let raw = |value: &Value, key: &str| -> Option<Value> {
            value
                .as_map()
                .unwrap()
                .iter()
                .find(|(name, _)| name.as_str() == Some(key))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            raw(&page, "next"),
            Some(Value::Nil),
            "next is present and nil"
        );
        assert_eq!(
            entry_paths(&page),
            ["docs/a.md", "docs/sub/m.md", "docs/z.md"],
            "bytewise order, no directory entry, nothing outside the prefix"
        );
        let entries = field_of(&page, "entries")
            .and_then(Value::as_array)
            .unwrap();
        for (entry, bytes) in entries.iter().zip([&a, &m, &z]) {
            assert_eq!(entry.as_map().unwrap().len(), 4, "{entry:?}");
            assert_eq!(
                field_of(entry, "size").and_then(Value::as_u64),
                Some(bytes.len() as u64)
            );
            assert_eq!(
                field_of(entry, "sha256"),
                Some(&Value::Binary(sha256_of(bytes).to_vec()))
            );
            let mtime = field_of(entry, "mtime").and_then(Value::as_f64).unwrap();
            assert!(
                mtime.is_finite() && (mtime - before).abs() < 60.0,
                "mtime {mtime} is not seconds since the epoch near {before}"
            );
        }

        assert_eq!(
            entry_paths(&fx.list(Some("notes/"), None).await),
            ["notes/n.md"]
        );
        assert_eq!(
            entry_paths(&fx.list(None, None).await),
            ["docs/a.md", "docs/sub/m.md", "docs/z.md", "notes/n.md"]
        );

        let empty = fx.list(Some("nothing/"), None).await;
        let keys: Vec<&str> = empty
            .as_map()
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str().unwrap())
            .collect();
        assert_eq!(keys, ["v", "entries", "next"], "{empty:?}");
        assert_eq!(raw(&empty, "entries"), Some(Value::Array(Vec::new())));
        assert_eq!(raw(&empty, "next"), Some(Value::Nil));

        let directory = fx.fetch("docs", None).await;
        assert_eq!(status_of(&directory), "not_shared");
        let nested_directory = fx.fetch("docs/sub", None).await;
        assert_eq!(status_of(&nested_directory), "not_shared");
        let missing = encoded(fx.fetch("docs/missing.md", None).await);
        assert_eq!(encoded(directory), missing);
        assert_eq!(encoded(nested_directory), missing);
    }

    /// A page holds at most 1 000 entries even when the wire would take more: the first
    /// page of 1 001 short paths is exactly 1 000 with a cursor, and the cursor resumes
    /// at the one left over with no gap, overlap, or further page.
    #[tokio::test]
    async fn usage_probe_a_list_page_holds_at_most_one_thousand_entries() {
        let fx = Fixture::new("fetch-probe-list-cap", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        let all: Vec<String> = (0..=1_000).map(|n| format!("docs/f{n:04}.md")).collect();
        for path in &all {
            fx.file(path, b"x");
        }

        let first = fx.list(None, None).await;

        let frame = ResponseFrame {
            request_id: RequestId::from([1u8; 16]),
            data: first.clone(),
        }
        .encode();
        assert!(
            frame.len() <= MAX_R3_PAYLOAD_BYTES,
            "{} bytes: the cut must come from the count, not the wire",
            frame.len()
        );
        let kept = entry_paths(&first);
        assert_eq!(kept.len(), 1_000);
        assert_eq!(kept, all[..1_000]);
        let cursor = next_of(&first).expect("a cut page carries a cursor");
        assert!(cursor.len() <= 64, "{cursor}");

        let second = fx.list(None, Some(&cursor)).await;

        assert_eq!(entry_paths(&second), all[1_000..]);
        assert_eq!(next_of(&second), None);
    }

    /// Every refusal leaves one `debug!` line naming the peer and the rule that fired, and
    /// none of those lines carries the requested path.
    #[cfg(unix)]
    #[tokio::test]
    async fn usage_probe_every_refusal_is_logged_at_debug_with_its_rule_and_without_the_path() {
        install_log_collector();
        let fx = Fixture::new("fetch-probe-refusal-log", 16, &["docs/**"]);
        fx.file("docs/big-probe-q9.md", &[b'x'; 17]);
        fx.file("src/hidden-probe-q9.rs", b"s");
        let identity = TransportIdentity::new_from_rand(OsRng);
        let destination = hex_lower(&[0x7e; 16]);
        let identity_hex = identity.as_identity().address_hash.to_hex_string();
        let mine =
            |line: &String| line.contains(&identity_hex[..8]) || line.contains(&destination[..8]);
        let handler = FetchHandler::new(fx.weak_source());
        let refusals = [
            ("../probe-q9", "invalid_path", "segment"),
            ("src/hidden-probe-q9.rs", "not_shared", "not_shared"),
            ("docs/missing-probe-q9.md", "not_shared", "not_shared"),
            ("docs/big-probe-q9.md", "too_large", "too_large"),
        ];

        for (path, status, rule_word) in refusals {
            let before = debug_snapshot().iter().filter(|line| mine(line)).count();
            let reply = handler
                .handle(admitted(
                    FETCH_PATH,
                    fetch_body(path, None),
                    &identity,
                    &destination,
                ))
                .await;
            assert_eq!(status_of(&reply), status, "{path}");
            let lines: Vec<String> = debug_snapshot().into_iter().filter(mine).collect();
            assert!(
                lines.len() > before,
                "no debug line for the {status} refusal of {path}: {lines:?}"
            );
            assert!(
                lines
                    .iter()
                    .skip(before)
                    .any(|line| line.contains(rule_word)),
                "no debug line names the rule {rule_word:?} for {path}: {lines:?}"
            );
        }

        // The decode refusals of both paths and the no-snapshot `not_shared` log too, each
        // naming what was wrong and never the body's text.
        let decode_refusals: [(&str, Value, &str); 3] = [
            (
                FETCH_PATH,
                map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION)),
                    ("path", Value::from(7u8)),
                ]),
                "path is missing or not text",
            ),
            (
                LIST_PATH,
                map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION)),
                    ("prefix", Value::from("probe-q9/")),
                    ("cursor", Value::from(3u8)),
                ]),
                "cursor is not text or is too long",
            ),
            (LIST_PATH, Value::from("probe-q9"), "the body is not a map"),
        ];
        for (path, body, why) in decode_refusals {
            let before = debug_snapshot().iter().filter(|line| mine(line)).count();
            let reply = if path == FETCH_PATH {
                handler
                    .handle(admitted(path, body, &identity, &destination))
                    .await
            } else {
                ListHandler::new(fx.weak_source())
                    .handle(admitted(path, body, &identity, &destination))
                    .await
            };
            assert!(matches!(reply, Reply::Code(RefusalCode::InvalidData)));
            let lines: Vec<String> = debug_snapshot().into_iter().filter(mine).collect();
            assert!(
                lines.iter().skip(before).any(|line| line.contains(why)),
                "no debug line says {why:?} for {path}: {lines:?}"
            );
        }
        let no_snapshot = Arc::new(TestSource {
            root: None,
            serving: Some(Arc::clone(&fx.serving)),
        });
        let before = debug_snapshot().iter().filter(|line| mine(line)).count();
        let reply = FetchHandler::new(Arc::downgrade(&no_snapshot) as Weak<dyn ShareSource>)
            .handle(admitted(
                FETCH_PATH,
                fetch_body("docs/probe-q9.md", None),
                &identity,
                &destination,
            ))
            .await;
        assert_eq!(status_of(&reply), "not_shared");
        let lines: Vec<String> = debug_snapshot().into_iter().filter(mine).collect();
        assert!(
            lines
                .iter()
                .skip(before)
                .any(|line| line.contains("refused: not_shared"))
        );

        let leaked: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(|line| line.contains("probe-q9"))
            .collect();
        assert!(
            leaked.is_empty(),
            "a refusal line carried the path: {leaked:?}"
        );
    }

    /// A granted file the operating system refuses to open is refused `not_shared` byte for
    /// byte like a missing one, the refusal is logged with its rule and without the path,
    /// and the grant's use is not spent by a fetch that served nothing: once the file is
    /// readable again the same grant serves it exactly once.
    #[cfg(unix)]
    #[tokio::test]
    async fn usage_probe_an_unreadable_granted_file_is_not_shared_byte_for_byte_and_keeps_its_use()
    {
        use std::os::unix::fs::PermissionsExt;
        install_log_collector();
        let fx = Fixture::new("fetch-probe-unreadable-grant", MAX_FETCH_FILE_BYTES, &[]);
        let relative = "docs/locked-probe-u7.md";
        fx.file(relative, b"locked");
        let on_disk = fx.root.join(relative);
        fs::set_permissions(&on_disk, fs::Permissions::from_mode(0o000)).unwrap();
        fx.grant(&[relative]);
        let store = fx.serving.grants();
        assert_eq!(uses_left(&store), 1);
        let identity_hex = fx.identity.as_identity().address_hash.to_hex_string();
        let destination = fx.destination.clone();
        let mine =
            |line: &String| line.contains(&identity_hex[..8]) || line.contains(&destination[..8]);

        let before = debug_snapshot().iter().filter(|line| mine(line)).count();
        let refused = fx.fetch(relative, None).await;
        assert_eq!(status_of(&refused), "not_shared");
        let lines: Vec<String> = debug_snapshot().into_iter().filter(mine).collect();
        assert!(
            lines
                .iter()
                .skip(before)
                .any(|line| line.contains("refused: not_shared")),
            "no debug line names the rule for the unreadable file: {lines:?}"
        );
        assert_eq!(
            encoded(refused),
            encoded(fx.fetch("docs/missing-probe-u7.md", None).await),
            "an unreadable file and a missing one answer byte for byte alike"
        );
        assert_eq!(
            uses_left(&store),
            1,
            "a fetch that served nothing spends nothing"
        );

        fs::set_permissions(&on_disk, fs::Permissions::from_mode(0o644)).unwrap();
        let served = fx.fetch(relative, None).await;
        assert_eq!(status_of(&served), "ok");
        let value = sent(served);
        assert_eq!(
            field_of(&value, "bytes").and_then(|bytes| bytes.as_slice()),
            Some(&b"locked"[..])
        );
        assert_eq!(uses_left(&store), 0);
        assert_eq!(status_of(&fx.fetch(relative, None).await), "not_shared");

        let leaked: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(|line| line.contains("probe-u7"))
            .collect();
        assert!(
            leaked.is_empty(),
            "a refusal line carried the path: {leaked:?}"
        );
    }

    /// A read that fails after the share list served the file answers the peer with the
    /// shared `not_shared` bytes and tells the operator why at `debug!`: the line names the
    /// rule, carries the read error with any full hash in it cut short, and never the
    /// path. The share rule is untouched, so the next fetch with a working reader serves.
    #[cfg(unix)]
    #[tokio::test]
    async fn usage_probe_a_read_that_fails_after_the_share_list_served_logs_why_without_the_path() {
        struct HashyReader;
        impl FileReader for HashyReader {
            fn read_bounded(&self, _file: fs::File, _limit: u64) -> std::io::Result<Vec<u8>> {
                Err(std::io::Error::other(
                    "block 0123456789abcdef0123456789abcdef vanished",
                ))
            }
        }
        install_log_collector();
        let fx = Fixture::new(
            "fetch-probe-read-failed-log",
            MAX_FETCH_FILE_BYTES,
            &["docs/**"],
        );
        let relative = "docs/served-probe-u8.md";
        fx.file(relative, b"served");
        let identity_hex = fx.identity.as_identity().address_hash.to_hex_string();
        let destination = fx.destination.clone();
        let mine =
            |line: &String| line.contains(&identity_hex[..8]) || line.contains(&destination[..8]);

        let before = debug_snapshot().iter().filter(|line| mine(line)).count();
        let failing = FetchHandler::with_reader(fx.weak_source(), Arc::new(HashyReader));
        let refused = fx.fetch_with(failing, relative, None).await;
        assert_eq!(status_of(&refused), "not_shared");
        let lines: Vec<String> = debug_snapshot().into_iter().filter(mine).collect();
        let why = "not_shared (read failed: block 01234567 vanished)";
        assert!(
            lines.iter().skip(before).any(|line| line.contains(why)),
            "no debug line says {why:?}: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .skip(before)
                .any(|line| line.contains("0123456789abcdef0123456789abcdef")),
            "the read error's full hash reached the log: {lines:?}"
        );
        assert_eq!(
            encoded(refused),
            encoded(fx.fetch("docs/missing-probe-u8.md", None).await),
            "a failed read and a missing file answer byte for byte alike"
        );

        let served = fx.fetch(relative, None).await;
        assert_eq!(status_of(&served), "ok", "the share rule still serves");
        assert_eq!(
            field_of(&sent(served), "bytes").and_then(|bytes| bytes.as_slice()),
            Some(&b"served"[..])
        );

        let leaked: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(|line| line.contains("probe-u8"))
            .collect();
        assert!(
            leaked.is_empty(),
            "a refusal line carried the path: {leaked:?}"
        );
    }

    /// Usage probe: cursor misuse at the handler. A cursor that
    /// is well-formed but unknown (right alphabet and length, names no entry) starts the
    /// listing over at the first page; so does a cursor that named an entry which has
    /// since vanished from the share root — a stale page token never yields an error, a
    /// gap, or a leak of other files.
    #[tokio::test]
    async fn usage_probe_an_unknown_or_stale_cursor_starts_the_listing_over() {
        let fx = Fixture::new("fetch-probe-cursor", MAX_FETCH_FILE_BYTES, &["docs/**"]);
        let names = ["docs/a.md", "docs/b.md", "docs/c.md"];
        for name in names {
            fx.file(name, name.as_bytes());
        }
        // Also an unshared sibling that no cursor may ever surface.
        fx.file("src/x.rs", b"x");

        let first = fx.list(None, None).await;
        assert_eq!(entry_paths(&first), names);
        assert_eq!(next_of(&first), None);

        // A well-formed cursor that matches nothing: 32 lowercase hex, like a real one.
        let unknown = "0".repeat(32);
        let restarted = fx.list(None, Some(&unknown)).await;
        assert_eq!(
            entry_paths(&restarted),
            names,
            "an unknown cursor starts over at the first page"
        );
        assert_eq!(next_of(&restarted), None);

        // A real cursor resumes after the entry it names...
        let after_a = fx.list(None, Some(&shares::list_cursor("docs/a.md"))).await;
        assert_eq!(entry_paths(&after_a), ["docs/b.md", "docs/c.md"]);

        // ...and once that entry is gone the same cursor is stale and starts over.
        fs::remove_file(fx.root.join("docs").join("a.md")).unwrap();
        let stale = fx.list(None, Some(&shares::list_cursor("docs/a.md"))).await;
        assert_eq!(
            entry_paths(&stale),
            ["docs/b.md", "docs/c.md"],
            "a stale cursor starts over, which is now the whole remaining set"
        );
        assert_eq!(next_of(&stale), None);

        // A cursor built from an unshared file's path reveals nothing about it: it is
        // simply unknown and starts over.
        let unshared = fx.list(None, Some(&shares::list_cursor("src/x.rs"))).await;
        assert_eq!(entry_paths(&unshared), ["docs/b.md", "docs/c.md"]);

        // Spec worked example: a prefix combined with an unknown cursor still filters.
        let prefixed = fx.list(Some("docs/c"), Some(&unknown)).await;
        assert_eq!(entry_paths(&prefixed), ["docs/c.md"]);
        assert!(
            !entry_paths(&prefixed).iter().any(|p| p.starts_with("src/")),
            "{:?}",
            entry_paths(&prefixed)
        );
    }
}
