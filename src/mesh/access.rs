//! Access requests: a trusted peer asking to read paths this node does not share. The
//! request is answered at once when the share rules already serve every path, refused
//! when the same peer is already waiting on the same set or on too many, and otherwise
//! held in the inbound store until the person at the keyboard grants or refuses it.
//!
//! Outbound, a request tries the peer over a link first and falls back to an LXMF
//! propagation node only when the peer cannot be reached; the fetch path routes such a
//! stored request into the same admission, and a grant it earns at once is sent back as
//! the decision reply the human's grant would send.
//!
//! Only the person at the keyboard sees what was asked for: the paths and the reason go
//! to the inbound record and the human line and nowhere else, never to the envoy, a
//! model note or a log line.

use crate::mesh::canonical_hash;
use crate::mesh::events::{AccessDecision, MeshEvent};
use crate::mesh::fetch::{FetchServing, ShareSource, field, versioned_map};
use crate::mesh::grants::DEFAULT_GRANT_TTL;
use crate::mesh::idle::{IdleNotify, Origin};
use crate::mesh::message::PEER_REQUEST_TIMEOUT;
use crate::mesh::message::{
    Disposition, OutboundPeer, PEER_WIRE_VERSION, PartLimits, PeerKind, PeerVia, RawPart,
    SendError, SendOutcome, is_wire_id,
};
use crate::mesh::node::{MeshRuntime, MeshSlot};
use crate::mesh::notify::Source;
use crate::mesh::parse_rfc3339;
use crate::mesh::pending::{
    DEFAULT_COLLECT_TIMEOUT, INBOUND_RECORD_VERSION, InboundKind, InboundRecord, InboundStore,
    PENDING_QUESTION_MAX_CHARS, PENDING_RECORD_VERSION, PendingRecord, PendingState,
    question_not_an_access_request,
};
use crate::mesh::propagation::{OutboundMessage, PropagationError, PropagationOptions};
use crate::mesh::propagation_fetch::{InboundMessage, InboundSink};
use crate::mesh::protocol::describe_version;
use crate::mesh::r3::{
    ACCESS_PATH, AdmittedRequest, DEFAULT_LINK_TIMEOUT, DispatchError, Handler, NAME_HASH_LEN,
    OriginName, R3Error, RefusalCode, Reply, RequestOptions,
};
use crate::mesh::shares::PeerRef;
use crate::mesh::shares::{Mutation, ShareSet, WriteScope};
use crate::mesh::trust::{
    Decision, IdentityStanding, KeyChangeOutcome, OriginVerdict, Rule, TrustStore, same_hash,
};
use crate::mesh::wire_path::WirePath;
use crate::mesh::{
    canonicalize, display_text, hex_lower, human_size, redact_hashes, rfc3339_utc, short,
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
use rmpv::Value;
use rns_transport::destination::DestinationDesc;
use rns_transport::hash::AddressHash;
use serde_json::json;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Matches `GRANT_MAX_PATHS`: a request for more is a share list by another name.
pub(crate) const ACCESS_MAX_PATHS: usize = 16;
pub(crate) const ACCESS_REASON_MAX_CHARS: usize = 500;
/// How much of a requested path and of the reason the human line shows; the rest is an
/// ellipsis. The caps keep every line of the text under the notification line cap with
/// the longest paths and reason the wire admits, so the verbs are never cut off.
pub(crate) const ACCESS_PATH_DISPLAY_MAX_CHARS: usize = 100;
pub(crate) const ACCESS_REASON_DISPLAY_MAX_CHARS: usize = 200;
/// Open requests one identity may have waiting on the human at once; the next is
/// refused rather than filed, so a peer cannot fill the inbound store by asking.
pub(crate) const ACCESS_MAX_PENDING_PER_IDENTITY: usize = 5;

/// A request body that passed every rule: a wire id, one to `ACCESS_MAX_PATHS` wire
/// paths with exact repeats dropped, and a reason cleaned for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidAccess {
    pub id: String,
    pub paths: Vec<String>,
    pub reason: String,
}

/// One admitted request as the surface sees it, whichever route it arrived by.
pub(crate) struct InboundAccess {
    pub identity_hash: String,
    pub destination_hash: String,
    pub request: ValidAccess,
    pub via: PeerVia,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AccessOutcome {
    Pending,
    /// `expires` is unix seconds, the grant's end as the peer should plan around it.
    Granted {
        expires: f64,
    },
    Refused(AccessRefusal),
}

impl AccessOutcome {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted { .. } => "granted",
            Self::Refused(_) => "refused",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessRefusal {
    Duplicate,
    TooManyPending,
}

impl AccessRefusal {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::TooManyPending => "too_many_pending",
        }
    }
}

/// Where an admitted access request is decided. Admission reads and writes the inbound
/// store, so it is called off the request loop.
pub(crate) trait AccessSurface: Send + Sync {
    fn admit_access(&self, request: InboundAccess) -> AccessOutcome;

    /// Sends the peer the decision reply for a request admission granted at once, on the
    /// route that has no link to answer over. The reply is what the human's grant would
    /// send; the peer collects it under the request's id either way.
    fn settle_granted_at_once(
        &self,
        identity_hash: &str,
        destination_hash: &str,
        request: &ValidAccess,
        expires: f64,
    );
}

/// Serves `/access`. The surface is held weakly because the slot owns the runtime that
/// owns the dispatcher that owns this handler.
pub(crate) struct AccessHandler {
    surface: Weak<dyn AccessSurface>,
}

impl AccessHandler {
    pub(crate) fn new(surface: Weak<dyn AccessSurface>) -> Self {
        Self { surface }
    }
}

#[async_trait]
impl Handler for AccessHandler {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        let identity_hash = request.identity.address_hash.to_hex_string();
        let destination_hash = request.destination_hash.to_hex_string();
        let id8 = short(&identity_hash).to_string();
        let dest8 = short(&destination_hash).to_string();
        let access = match decode_access(&request.body) {
            Ok(access) => access,
            Err(why) => {
                debug!(
                    "Mesh /access from {id8} (instance {dest8}) refused: {}",
                    redact_hashes(why)
                );
                return Reply::Code(RefusalCode::InvalidData);
            }
        };
        let Some(surface) = self.surface.upgrade() else {
            debug!(
                "Mesh /access from {id8} (instance {dest8}) dropped: the session slot behind the provider is gone"
            );
            return Reply::Silent;
        };
        let id = access.id.clone();
        let admitted = tokio::task::spawn_blocking(move || {
            surface.admit_access(InboundAccess {
                identity_hash,
                destination_hash,
                request: access,
                via: PeerVia::Direct,
            })
        })
        .await;
        let outcome = match admitted {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(
                    "Mesh /access from {id8} (instance {dest8}) was not answered: {}",
                    redact_hashes(&err.to_string())
                );
                return Reply::Silent;
            }
        };
        match &outcome {
            AccessOutcome::Refused(refusal) => debug!(
                "Mesh /access from {id8} (instance {dest8}) refused: {}",
                refusal.wire_name()
            ),
            answered => debug!(
                "Mesh /access from {id8} (instance {dest8}) answered {}",
                answered.status()
            ),
        }
        Reply::Value(access_reply(&id, &outcome))
    }
}

/// The one encoder for the three reply shapes: a flat map of `v`, `id`, `status` and,
/// for a grant its `expires`, for a refusal its `reason`.
pub(crate) fn access_reply(id: &str, outcome: &AccessOutcome) -> Value {
    let mut entries = vec![
        (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
        (Value::from("id"), Value::from(id)),
        (Value::from("status"), Value::from(outcome.status())),
    ];
    match outcome {
        AccessOutcome::Pending => {}
        AccessOutcome::Granted { expires } => {
            entries.push((Value::from("expires"), Value::from(*expires)));
        }
        AccessOutcome::Refused(refusal) => {
            entries.push((Value::from("reason"), Value::from(refusal.wire_name())));
        }
    }
    Value::Map(entries)
}

pub(crate) fn decode_access(body: &Value) -> Result<ValidAccess, &'static str> {
    let entries = versioned_map(body)?;
    let id = field(entries, "id")
        .and_then(Value::as_str)
        .ok_or("id is missing or not text")?;
    let paths = field(entries, "paths")
        .and_then(Value::as_array)
        .ok_or("paths is missing or not a list")?
        .iter()
        .map(|path| {
            path.as_str()
                .map(str::to_string)
                .ok_or("a path is not text")
        })
        .collect::<Result<Vec<String>, _>>()?;
    let reason = match field(entries, "reason") {
        None => "",
        Some(value) => value.as_str().ok_or("reason is not text")?,
    };
    validate_access(id, paths, reason)
}

/// The rules an access request is held to on every route it can arrive by. The path cap
/// is judged on what the peer sent and the floor on what is left once exact repeats are
/// dropped, so a padded request is refused and a repeated path is not asked for twice.
/// The reason is cleaned as `display_text` cleans any peer text; an absent or blank one
/// is empty.
pub(crate) fn validate_access(
    id: &str,
    paths: Vec<String>,
    reason: &str,
) -> Result<ValidAccess, &'static str> {
    if !is_wire_id(id) {
        return Err("id is not a wire id");
    }
    if paths.len() > ACCESS_MAX_PATHS {
        return Err("paths names more than the cap allows");
    }
    let mut kept: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        if WirePath::parse(&path).is_err() {
            return Err("a path is not a wire path");
        }
        if !kept.contains(&path) {
            kept.push(path);
        }
    }
    if kept.is_empty() {
        return Err("paths is empty");
    }
    let reason = match display_text(reason, usize::MAX) {
        None => String::new(),
        Some(cleaned) if cleaned.chars().count() > ACCESS_REASON_MAX_CHARS => {
            return Err("reason is longer than the cap allows");
        }
        Some(cleaned) => cleaned,
    };
    Ok(ValidAccess {
        id: id.to_string(),
        paths: kept,
        reason,
    })
}

/// Timeouts for one access request: the direct attempt, then the fallback post.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AccessOptions {
    pub request: RequestOptions,
    pub propagation: PropagationOptions,
}

impl Default for AccessOptions {
    fn default() -> Self {
        Self {
            request: RequestOptions {
                request_timeout: PEER_REQUEST_TIMEOUT,
                link_timeout: DEFAULT_LINK_TIMEOUT,
            },
            propagation: PropagationOptions::default(),
        }
    }
}

/// What a sent request came back as, and by which route. A stored request is pending by
/// definition: the peer has not seen it yet.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccessRequestOutcome {
    pub id: String,
    pub status: AccessOutcome,
    pub via: PeerVia,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AccessError {
    NotRunning,
    /// Unknown to the peer table, known but untrusted, denied or blocked: one text for
    /// all four, as for a send.
    Untrusted,
    /// Trusted, but its identity has not announced since this node started, so there is
    /// no description to link to.
    UnknownDestination,
    /// The request breaks a rule the peer would refuse it for; the text names the rule.
    Invalid(&'static str),
    /// The pending store would not file the request, so nothing was sent.
    NotFiled(String),
    /// The peer answered with a refusal code; nothing is stored for a peer that said no.
    Refused(RefusalCode),
    /// The peer let the request through but nothing there serves `/access`.
    NotServed(DispatchError),
    /// The peer answered with a status or a refusal reason this Coyote does not know.
    UnknownStatus,
    /// The peer answered, but not with an access reply to this request; the text names
    /// what was wrong with it.
    Malformed(&'static str),
    /// The direct attempt failed for a reason that is not the peer being unreachable, so
    /// nothing was stored for it.
    Direct(R3Error),
    IncompatibleVersion {
        destination: String,
        found: Option<u16>,
        min: u16,
        max: u16,
    },
    NoPropagationNode,
    Propagation(PropagationError),
}

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => write!(
                f,
                "The mesh node has been stopped; run `.mesh on` to start it again"
            ),
            Self::Untrusted => write!(
                f,
                "The destination is not trusted for an access request. Trust it first with `.mesh trust <destination>`; `.mesh peers` lists what this Coyote has heard from."
            ),
            Self::UnknownDestination => write!(
                f,
                "The destination is trusted but has not announced since this node started, so there is no path to it yet. Wait for its next announce; `.mesh peers` shows when it was last heard."
            ),
            Self::Invalid(rule) => write!(f, "The access request was refused: {rule}"),
            Self::NotFiled(reason) => write!(
                f,
                "The access request was not sent because it could not be filed as pending: {reason}"
            ),
            Self::Refused(code) => write!(f, "The peer refused the access request: {code}"),
            Self::NotServed(_) => write!(
                f,
                "The peer does not take access requests; it may be running an older Coyote without an access provider"
            ),
            Self::UnknownStatus => write!(
                f,
                "The peer sent an unknown status in its answer to the access request; it may be running a newer Coyote"
            ),
            Self::Malformed(why) => write!(
                f,
                "The peer answered the access request with something that is not an access reply: {why}"
            ),
            Self::Direct(err) => write!(f, "The access request could not be sent: {err}"),
            Self::IncompatibleVersion {
                destination,
                found,
                min,
                max,
            } => write!(
                f,
                "Destination {destination} and this Coyote speak incompatible mesh protocol versions: version {} was refused by the side that supports {min}..={max}. One of the two needs upgrading before they can talk; the peer listing names which.",
                describe_version(*found)
            ),
            Self::NoPropagationNode => write!(
                f,
                "The peer is unreachable and no propagation node is known yet to hold the access request for it. Run `.mesh peers` to see which nodes this Coyote has heard from."
            ),
            Self::Propagation(err) => write!(
                f,
                "The peer is unreachable and the access request could not be stored: {err}"
            ),
        }
    }
}

impl std::error::Error for AccessError {}

impl MeshSlot {
    /// Asks the trusted instance at `peer_destination` to let this node read `paths`.
    /// The correlation is opened before the send so a decision that beats the reply's
    /// acknowledgement still matches, and closed again unless the peer left the request
    /// pending: a grant or refusal on the wire is the whole answer, and a failed send
    /// asked nothing. The paths stay with the caller; the pending record keeps only the
    /// first words of what was asked.
    pub(crate) async fn request_access(
        &self,
        peer_destination: &str,
        paths: &[String],
        reason: &str,
    ) -> Result<AccessRequestOutcome, AccessError> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let request = validate_access(&id, paths.to_vec(), reason).map_err(AccessError::Invalid)?;
        let Some(runtime) = self.get() else {
            return Err(AccessError::NotRunning);
        };
        let Some(destination) = canonical_hash(peer_destination) else {
            return Err(AccessError::Untrusted);
        };
        let Some(peer) = runtime.peers().get(&destination) else {
            return Err(AccessError::Untrusted);
        };
        if runtime
            .trust()
            .authorize(&peer.identity_hash, &destination)
            .decision
            != Decision::Allow
        {
            return Err(AccessError::Untrusted);
        }
        let desc = runtime
            .resolve_destination(&destination)
            .await
            .ok_or(AccessError::UnknownDestination)?;
        let now = SystemTime::now();
        self.correlations()
            .open(PendingRecord {
                version: PENDING_RECORD_VERSION,
                id: id.clone(),
                peer_destination: destination,
                peer_identity: peer.identity_hash,
                thread: id.clone(),
                question: display_text(
                    &format!("access: {}", request.paths.join(", ")),
                    PENDING_QUESTION_MAX_CHARS,
                )
                .unwrap_or_default(),
                sent_at: rfc3339_utc(now),
                timeout_at: rfc3339_utc(now + DEFAULT_COLLECT_TIMEOUT),
                state: PendingState::Open,
                reply: None,
            })
            .map_err(|err| AccessError::NotFiled(format!("{err:#}")))?;
        let outcome = runtime
            .request_access_wire(&desc, &request, AccessOptions::default())
            .await;
        if !matches!(
            outcome,
            Ok(AccessRequestOutcome {
                status: AccessOutcome::Pending,
                ..
            })
        ) {
            self.correlations().abandon(&id);
        }
        outcome
    }
}

impl MeshRuntime {
    /// Puts `request` to `destination` over a link and reads the answer. Only the
    /// peer-unreachable errors fall back to store-and-forward: a refusal of any code
    /// means the peer heard and said no, and an oversize or undecodable frame is this
    /// node's fault. Posts to the propagation node queue behind `posting`, since
    /// `propagate` needs one caller per node at a time.
    pub(crate) async fn request_access_wire(
        &self,
        destination: &DestinationDesc,
        request: &ValidAccess,
        options: AccessOptions,
    ) -> Result<AccessRequestOutcome, AccessError> {
        let dest_hex = destination.address_hash.to_hex_string();
        let dest8 = short(&dest_hex);
        let id = request.id.as_str();
        let unreachable = match self
            .request(
                destination,
                ACCESS_PATH,
                access_body(request),
                options.request,
            )
            .await
        {
            Ok(outcome) => {
                let status = read_access_reply(&outcome.value, id)?;
                debug!(
                    "Mesh access request {id} to {dest8} was answered {} over a link",
                    status.status()
                );
                return Ok(AccessRequestOutcome {
                    id: request.id.clone(),
                    status,
                    via: PeerVia::Direct,
                });
            }
            Err(err @ (R3Error::Timeout { .. } | R3Error::LinkFailed(_) | R3Error::LinkClosed)) => {
                err
            }
            Err(R3Error::Refused(code)) => {
                debug!("Mesh access request {id} to {dest8} was refused: {code}");
                return Err(AccessError::Refused(code));
            }
            Err(R3Error::NotRunning | R3Error::Shutdown) => return Err(AccessError::NotRunning),
            Err(R3Error::UnsupportedVersion { found, min, max }) => {
                return Err(AccessError::IncompatibleVersion {
                    destination: dest_hex,
                    found,
                    min,
                    max,
                });
            }
            Err(err) => {
                debug!(
                    "Mesh access request {id} to {dest8} was not sent over the link: {}",
                    redact_hashes(&err.to_string())
                );
                return Err(AccessError::Direct(err));
            }
        };
        // Selected before queueing behind another post: a requester with no node to fall
        // back on is told so at once rather than after someone else's transfer.
        let node = self
            .propagation_nodes()
            .select_for_posting()
            .map_err(|_| AccessError::NoPropagationNode)?;
        let node_hex = node.destination.address_hash.to_hex_string();
        debug!(
            "Mesh access request {id} to {dest8} could not be delivered over a link ({}); storing it with propagation node {}",
            redact_hashes(&unreachable.to_string()),
            short(&node_hex)
        );
        self.post_to_node(
            &destination.identity,
            &node,
            |origin| access_message(request, origin),
            &options.propagation,
        )
        .await
        .map_err(|err| match err {
            PropagationError::Cancelled
            | PropagationError::Link(R3Error::Shutdown | R3Error::NotRunning) => {
                AccessError::NotRunning
            }
            other => AccessError::Propagation(other),
        })?;
        Ok(AccessRequestOutcome {
            id: request.id.clone(),
            status: AccessOutcome::Pending,
            via: PeerVia::StoreAndForward,
        })
    }
}

pub(crate) fn access_body(request: &ValidAccess) -> Value {
    Value::Map(vec![
        (Value::from("v"), Value::from(PEER_WIRE_VERSION)),
        (Value::from("id"), Value::from(request.id.as_str())),
        (Value::from("paths"), wire_paths(&request.paths)),
        (Value::from("reason"), Value::from(request.reason.as_str())),
    ])
}

fn wire_paths(paths: &[String]) -> Value {
    Value::Array(
        paths
            .iter()
            .map(|path| Value::from(path.as_str()))
            .collect(),
    )
}

/// Reads what came back for the request sent as `sent_id`: a peer with nothing behind
/// `/access` answers with a dispatch error, which is told apart from a malformed access
/// reply before the reply is decoded.
pub(crate) fn read_access_reply(
    value: &Value,
    sent_id: &str,
) -> Result<AccessOutcome, AccessError> {
    if let Some(error) = DispatchError::from_value(value) {
        return Err(AccessError::NotServed(error));
    }
    decode_access_response(value, sent_id)
}

/// Reads the peer's access reply to the request sent as `sent_id`. Keys the peer adds
/// are ignored; a status or refusal reason this Coyote does not know is a typed error,
/// so a newer peer's answer is reported rather than guessed at.
pub(crate) fn decode_access_response(
    value: &Value,
    sent_id: &str,
) -> Result<AccessOutcome, AccessError> {
    let entries = versioned_map(value).map_err(AccessError::Malformed)?;
    if field(entries, "id").and_then(Value::as_str) != Some(sent_id) {
        return Err(AccessError::Malformed("id is missing or not the one sent"));
    }
    let status = field(entries, "status")
        .and_then(Value::as_str)
        .ok_or(AccessError::Malformed("status is missing or not text"))?;
    match status {
        "pending" => Ok(AccessOutcome::Pending),
        "granted" => {
            let expires = field(entries, "expires")
                .and_then(Value::as_f64)
                .ok_or(AccessError::Malformed("expires is missing or not a number"))?;
            Ok(AccessOutcome::Granted { expires })
        }
        "refused" => match field(entries, "reason").and_then(Value::as_str) {
            Some("duplicate") => Ok(AccessOutcome::Refused(AccessRefusal::Duplicate)),
            Some("too_many_pending") => Ok(AccessOutcome::Refused(AccessRefusal::TooManyPending)),
            _ => Err(AccessError::UnknownStatus),
        },
        _ => Err(AccessError::UnknownStatus),
    }
}

impl AccessSurface for MeshSlot {
    /// Admits a peer's access request: granted at once when every path is already
    /// shared, otherwise filed in the inbound store for the human to decide. A missing
    /// or unwritable store is reported to the peer as `too_many_pending`, the only
    /// refusal word that means "try later".
    fn admit_access(&self, request: InboundAccess) -> AccessOutcome {
        let InboundAccess {
            identity_hash,
            destination_hash,
            request,
            via,
        } = request;
        let id8 = short(&identity_hash).to_string();
        let dest8 = short(&destination_hash).to_string();
        let now = SystemTime::now();
        let hooks = self.hooks();
        let root = ShareSource::share_root(self);
        let peer = PeerRef {
            identity: &identity_hash,
            destination: &destination_hash,
        };
        if let (Some(root), Some(serving)) = (&root, ShareSource::serving(self))
            && already_shared(&serving, root, &peer, &request.paths)
        {
            let expires = (now + DEFAULT_GRANT_TTL)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            hooks.fire(MeshEvent::AccessRequested {
                identity: identity_hash.clone(),
                destination: destination_hash.clone(),
                access_id: request.id.clone(),
                path_count: request.paths.len(),
            });
            hooks.fire(MeshEvent::AccessDecided {
                identity: identity_hash,
                destination: destination_hash,
                access_id: request.id,
                decision: AccessDecision::Granted,
            });
            debug!(
                "Mesh /access from {id8} (instance {dest8}) via {} granted at once: every path is already shared",
                via_word(via)
            );
            return AccessOutcome::Granted { expires };
        }
        // Once a node is installed the store is there, and filing is what makes a
        // request pending. A request that cannot be filed is refused with the one word
        // that tells the peer to ask again later, rather than left pending in a store
        // that never heard of it.
        let Some(store) = self.inbound_store() else {
            warn!("Mesh /access from {id8} (instance {dest8}) was not filed: the mesh is off");
            return AccessOutcome::Refused(AccessRefusal::TooManyPending);
        };
        let record = InboundRecord {
            version: INBOUND_RECORD_VERSION,
            id: request.id.clone(),
            peer_destination: destination_hash.clone(),
            peer_identity: identity_hash.clone(),
            thread: request.id.clone(),
            question: String::new(),
            envoy_question: String::new(),
            received_at: rfc3339_utc(now),
            kind: InboundKind::Access,
            paths: request.paths.clone(),
            reason: request.reason.clone(),
        };
        // The rule is judged and the record filed under one lock, so a burst of
        // requests from one identity cannot each see the cap unreached and all be filed.
        // Every open record is judged, whatever its kind or peer, so a re-used id is a
        // duplicate and never a filing failure the peer would hear as `too_many_pending`.
        match store.file_unless(record, now, |open| {
            rate_rule(open, &identity_hash, &request.id, &request.paths)
        }) {
            Ok(Ok(())) => {}
            Ok(Err(refusal)) => {
                debug!(
                    "Mesh /access from {id8} (instance {dest8}) via {} refused: {}",
                    via_word(via),
                    refusal.wire_name()
                );
                return AccessOutcome::Refused(refusal);
            }
            Err(err) => {
                // Logged at debug: the store's own guards are all judged by the rule
                // first, so what reaches here is I/O, never something a peer can trigger.
                debug!(
                    "Mesh /access from {id8} (instance {dest8}) was not filed: {}",
                    redact_hashes(&format!("{err:#}"))
                );
                return AccessOutcome::Refused(AccessRefusal::TooManyPending);
            }
        }
        hooks.fire(MeshEvent::AccessRequested {
            identity: identity_hash.clone(),
            destination: destination_hash.clone(),
            access_id: request.id.clone(),
            path_count: request.paths.len(),
        });
        let label = self.peer_label(&identity_hash, &destination_hash);
        self.push_idle(IdleNotify {
            source: Source::Access,
            origin: Origin::Peer(id8.clone()),
            text: access_text(&label, &request, root.as_deref()),
            model_note: None,
        });
        debug!(
            "Mesh /access from {id8} (instance {dest8}) via {} is pending on {} paths",
            via_word(via),
            request.paths.len()
        );
        AccessOutcome::Pending
    }

    /// Sent off the fetch path, which runs where nothing may await, the way a refusal
    /// reply is. A reply that cannot go is logged: the peer's own request has timed out
    /// by then and it asks again.
    fn settle_granted_at_once(
        &self,
        identity_hash: &str,
        destination_hash: &str,
        request: &ValidAccess,
        expires: f64,
    ) {
        let id8 = short(identity_hash).to_string();
        let dest8 = short(destination_hash).to_string();
        let id = request.id.clone();
        let Some(runtime) = self.get() else {
            warn!(
                "Mesh access {id} from {id8} (instance {dest8}) was granted but the grant could not be sent: the mesh is off"
            );
            return;
        };
        let expires = Duration::try_from_secs_f64(expires)
            .ok()
            .and_then(|since_epoch| UNIX_EPOCH.checked_add(since_epoch));
        let out = match decision_reply(
            &request.id,
            &request.id,
            request.paths.len(),
            AccessDecision::Granted,
            expires,
            false,
            &runtime.part_limits(),
        ) {
            Ok(out) => out,
            Err(err) => {
                warn!(
                    "Mesh access {id} from {id8} (instance {dest8}) was granted but the grant could not be built: {}",
                    redact_hashes(&err.to_string())
                );
                return;
            }
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!(
                "Mesh access {id} from {id8} (instance {dest8}) was granted but the grant could not be sent: no async runtime to send it with"
            );
            return;
        };
        let destination = destination_hash.to_string();
        handle.spawn(async move {
            if let Err(err) = runtime.send_peer(&destination, &out).await {
                warn!(
                    "Mesh access {id} from {id8} (instance {dest8}) was granted but the grant could not be sent: {}",
                    redact_hashes(&err.to_string())
                );
            }
        });
    }
}

fn via_word(via: PeerVia) -> &'static str {
    match via {
        PeerVia::Direct => "its link",
        PeerVia::StoreAndForward => "a propagation node",
    }
}

/// The LXMF custom type a stored access request carries in `FIELD_CUSTOM_TYPE`, so a
/// fetch can tell it from a knock or a peer message before it reads anything else.
/// Versioned in the name: a later layout gets a new type, and a node that does not know
/// it treats the payload as a message.
pub(crate) const ACCESS_TYPE: &str = "scope.access/1";

/// An access request as a propagation node stores it: the reason as the content, the
/// type, this node's origin name, the id and the paths in the custom fields. The
/// recipient recomputes the asking destination from that name and the signer's
/// identity, exactly as the dispatcher does for a link request, so a stored request can
/// only ever name one of the requester's own instances.
pub(crate) fn access_message(request: &ValidAccess, origin: &OriginName) -> OutboundMessage {
    OutboundMessage {
        title: None,
        content: request.reason.as_bytes().to_vec(),
        fields: Some(Value::Map(vec![
            (Value::from(FIELD_CUSTOM_TYPE), Value::from(ACCESS_TYPE)),
            (
                Value::from(FIELD_CUSTOM_DATA),
                Value::Map(vec![
                    (Value::from("name_hash"), Value::Binary(origin.0.to_vec())),
                    (Value::from("id"), Value::from(request.id.as_str())),
                    (Value::from("paths"), wire_paths(&request.paths)),
                ]),
            ),
        ])),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccessMessage {
    NotAnAccess,
    /// Typed as an access request but not laid out as one, or breaking a rule the link
    /// route refuses; never a message either.
    Malformed(&'static str),
    Access {
        name_hash: [u8; NAME_HASH_LEN],
        request: ValidAccess,
    },
}

/// Reads a fetched message as an access request, held to the same rules as one off a
/// link. The custom type is accepted as a string or as its UTF-8 bytes, since msgpack
/// encoders differ on which they emit.
pub(crate) fn decode_access_message(message: &InboundMessage) -> AccessMessage {
    let Some(Value::Map(fields)) = &message.fields else {
        return AccessMessage::NotAnAccess;
    };
    let custom = |key: u8| {
        fields
            .iter()
            .find(|(field, _)| field.as_u64() == Some(u64::from(key)))
            .map(|(_, value)| value)
    };
    let Some(kind) = custom(FIELD_CUSTOM_TYPE) else {
        return AccessMessage::NotAnAccess;
    };
    let is_access = match kind {
        Value::String(text) => text.as_str() == Some(ACCESS_TYPE),
        Value::Binary(bytes) => bytes == ACCESS_TYPE.as_bytes(),
        _ => false,
    };
    if !is_access {
        return AccessMessage::NotAnAccess;
    }
    let Some(Value::Map(data)) = custom(FIELD_CUSTOM_DATA) else {
        return AccessMessage::Malformed("custom data is missing or not a map");
    };
    let Some(Value::Binary(bytes)) = field(data, "name_hash") else {
        return AccessMessage::Malformed("name_hash is missing or not binary");
    };
    let Ok(name_hash) = <[u8; NAME_HASH_LEN]>::try_from(bytes.as_slice()) else {
        return AccessMessage::Malformed("name_hash is not 10 bytes");
    };
    let Some(id) = field(data, "id").and_then(Value::as_str) else {
        return AccessMessage::Malformed("id is missing or not text");
    };
    let Some(paths) = field(data, "paths").and_then(Value::as_array) else {
        return AccessMessage::Malformed("paths is missing or not a list");
    };
    let Ok(paths) = paths
        .iter()
        .map(|path| path.as_str().map(str::to_string).ok_or(()))
        .collect::<Result<Vec<String>, ()>>()
    else {
        return AccessMessage::Malformed("a path is not text");
    };
    let reason = message
        .content
        .as_deref()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    match validate_access(id, paths, &reason) {
        Ok(request) => AccessMessage::Access { name_hash, request },
        Err(rule) => AccessMessage::Malformed(rule),
    }
}

/// An `InboundSink` in front of another: fetched access requests go to the surface,
/// everything else to `inner`. A payload typed as an access request is never a message,
/// so a malformed or untrusted one is dropped rather than forwarded. Store-and-forward
/// has no link to answer over, so a request granted at once is answered with the
/// decision reply and a refused one is not answered at all: the peer's own request has
/// timed out by then and it asks again.
pub(crate) struct AccessRouting<'a> {
    pub trust: &'a TrustStore,
    pub surface: Option<Arc<dyn AccessSurface>>,
    pub inner: &'a dyn InboundSink,
}

impl InboundSink for AccessRouting<'_> {
    fn deliver(&self, message: InboundMessage) {
        let id8 = short(&message.source_identity_hash).to_string();
        let (name_hash, request) = match decode_access_message(&message) {
            AccessMessage::NotAnAccess => return self.inner.deliver(message),
            AccessMessage::Malformed(why) => {
                debug!(
                    "Propagated access request from {id8} dropped: {}",
                    redact_hashes(why)
                );
                return;
            }
            AccessMessage::Access { name_hash, request } => (name_hash, request),
        };
        let Ok(identity) = AddressHash::new_from_hex_string(&message.source_identity_hash) else {
            debug!("Propagated access request from {id8} dropped: the signer's hash is malformed");
            return;
        };
        let standing = match self.trust.identity_standing(&message.source_identity_hash) {
            IdentityStanding::Trusted { .. } => None,
            IdentityStanding::Unknown => Some("unknown"),
            IdentityStanding::Blocked => Some("blocked"),
        };
        if let Some(standing) = standing {
            debug!("Propagated access request from {id8} dropped: {standing} identity");
            return;
        }
        let now = SystemTime::now();
        let OriginVerdict {
            verdict,
            destination,
            collisions,
        } = self.trust.authorize_origin_at(&identity, &name_hash, now);
        let source_destination = destination.to_hex_string();
        let dest8 = short(&source_destination).to_string();
        if !collisions.is_empty() || verdict.rule == Rule::IdentityChanged {
            let outcome = match verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            self.trust.note_key_change(
                &message.source_identity_hash,
                &hex_lower(&name_hash),
                outcome,
                now,
            );
        }
        if verdict.decision != Decision::Allow {
            debug!("Propagated access request from {id8} dropped: instance {dest8} is not trusted");
            return;
        }
        let Some(surface) = &self.surface else {
            debug!(
                "Propagated access request from {id8} (instance {dest8}) dropped: the session slot is gone"
            );
            return;
        };
        let asked = request.clone();
        let outcome = surface.admit_access(InboundAccess {
            identity_hash: message.source_identity_hash.clone(),
            destination_hash: source_destination.clone(),
            request,
            via: PeerVia::StoreAndForward,
        });
        match outcome {
            AccessOutcome::Pending => {}
            AccessOutcome::Refused(refusal) => debug!(
                "Propagated access request from {id8} (instance {dest8}) refused: {}; there is no link to say so over",
                refusal.wire_name()
            ),
            AccessOutcome::Granted { expires } => surface.settle_granted_at_once(
                &message.source_identity_hash,
                &source_destination,
                &asked,
                expires,
            ),
        }
    }
}

/// The human's side of a pending access request: grant it once or for good, or refuse
/// it. Either way the peer hears a reply and the request leaves the inbound store.
pub(crate) struct AccessStore<'a> {
    slot: &'a MeshSlot,
}

impl MeshSlot {
    pub(crate) fn access(&self) -> AccessStore<'_> {
        AccessStore { slot: self }
    }
}

/// How a grant lets the peer in: once, through a grant record that lends each path one
/// use until `ttl` (the default when `None`) runs out, or standing, through an allow
/// entry per path written to the share list `scope` picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantKind {
    OneOff { ttl: Option<Duration> },
    Standing { scope: WriteScope },
}

/// What a decision did, for the line the human reads back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccessDecisionReport {
    pub id: String,
    pub peer_destination: String,
    pub path_count: usize,
    pub decision: AccessDecision,
    /// When a one-off grant runs out; a standing grant and a refusal have no end.
    pub expires: Option<SystemTime>,
    pub standing: bool,
    pub via: PeerVia,
}

impl AccessStore<'_> {
    /// Lets the requesting peer read every path it asked for, as `kind` says. Either way
    /// the request is taken out of the store before anything is written or sent, so two
    /// processes deciding the same id send one reply and write one grant between them,
    /// and put back when the send fails, so it stays pending to decide again. A one-off
    /// grant is written between the take and the send and taken back with the request
    /// when the send fails: the peer never heard yes, and the human may refuse next. The
    /// standing entries are written after the send: they would outlive a reply that
    /// never arrived and a later refusal alike, so the peer must have heard yes first.
    /// The share list is loaded once before the send, so a list that cannot be written
    /// is found before the peer is told anything, and again after it, so an edit made
    /// while the reply was in flight is not written over.
    pub(crate) async fn grant(&self, id: &str, kind: GrantKind) -> Result<AccessDecisionReport> {
        let (store, record, runtime) = self.pending(id)?;
        match kind {
            GrantKind::Standing { scope } => {
                self.standing_shares(&runtime, &record)?;
                let sent = self
                    .send_decision(
                        &store,
                        &record,
                        &runtime,
                        AccessDecision::Granted,
                        None,
                        true,
                    )
                    .await?;
                if let Err(err) = self
                    .standing_shares(&runtime, &record)
                    .and_then(|mut shares| share_standing(&mut shares, &record, scope))
                {
                    let err = anyhow!(
                        "{} was already told yes, but the share list could not be written: {err:#}. Run `.mesh grant {id} --standing` again once the share list is writable; the request stays pending until then.",
                        short(&record.peer_destination)
                    );
                    return Err(put_back(&store, &record, err));
                }
                self.settle(record, AccessDecision::Granted, None, true, sent)
            }
            GrantKind::OneOff { ttl } => self.grant_once(&store, record, &runtime, ttl).await,
        }
    }

    /// The one-off arm of `grant`, from the take on: the grant is written, the reply
    /// sent, and both undone when either fails, so a request back in the store has no
    /// grant standing behind it.
    async fn grant_once(
        &self,
        store: &InboundStore,
        record: InboundRecord,
        runtime: &MeshRuntime,
        ttl: Option<Duration>,
    ) -> Result<AccessDecisionReport> {
        take(store, &record)?;
        let grants = runtime.serving().grants();
        let outcome = async {
            let granted = grants.grant(
                &record.id,
                &record.peer_destination,
                &record.paths,
                ttl,
                SystemTime::now(),
            )?;
            let expires = parse_rfc3339(&granted.expires)
                .context("The grant was written with an expiry that does not read back")?;
            let sent = self
                .send(
                    &record,
                    runtime,
                    AccessDecision::Granted,
                    Some(expires),
                    false,
                )
                .await?;
            Ok((expires, sent))
        }
        .await;
        match outcome {
            Ok((expires, sent)) => {
                self.settle(record, AccessDecision::Granted, Some(expires), false, sent)
            }
            Err(err) => {
                let err = match grants.revoke(&record.id, &record.peer_destination) {
                    Ok(_) => err,
                    Err(revoke) => anyhow!(
                        "{err:#}; the grant written for `{}` could not be taken back either: {revoke:#}",
                        record.id
                    ),
                };
                Err(put_back(store, &record, err))
            }
        }
    }

    /// Tells the requesting peer no. Nothing is written to the grants or the share list.
    pub(crate) async fn refuse(&self, id: &str) -> Result<AccessDecisionReport> {
        let (store, record, runtime) = self.pending(id)?;
        let sent = self
            .send_decision(
                &store,
                &record,
                &runtime,
                AccessDecision::Denied,
                None,
                false,
            )
            .await?;
        self.settle(record, AccessDecision::Denied, None, false, sent)
    }

    fn pending(&self, id: &str) -> Result<(Arc<InboundStore>, InboundRecord, Arc<MeshRuntime>)> {
        let Some(store) = self.slot.inbound_store() else {
            bail!("Mesh is off; turn it on with `.mesh on` before deciding {id}");
        };
        let Some(record) = store.get(id)? else {
            bail!("no open access request {id}");
        };
        if record.kind == InboundKind::Question {
            bail!(question_not_an_access_request(id));
        }
        let Some(runtime) = self.slot.get() else {
            bail!(
                "Mesh is off, so the decision on {id} cannot be sent to {}",
                short(&record.peer_destination)
            );
        };
        Ok((store, record, runtime))
    }

    /// The share list a standing grant for `record` writes to, as it is on disk now: a
    /// list that does not load is left as it is, so the peer is not told yes about
    /// entries that cannot be written, and a list changed under a sent decision is
    /// written through rather than over.
    fn standing_shares(&self, runtime: &MeshRuntime, record: &InboundRecord) -> Result<ShareSet> {
        let Some(root) = ShareSource::share_root(self.slot) else {
            bail!(
                "A standing grant writes to the share list under the workspace root, which is unknown until a turn completes; grant {id} once with `.mesh grant {id}` instead",
                id = record.id
            );
        };
        let (shares, warning) = ShareSet::load_quietly(runtime.serving().share_locations(&root));
        if let Some(warning) = warning {
            bail!("{warning} Nothing was written.");
        }
        Ok(shares)
    }

    /// `take`, then `send`; a send that fails puts the request back.
    async fn send_decision(
        &self,
        store: &InboundStore,
        record: &InboundRecord,
        runtime: &MeshRuntime,
        decision: AccessDecision,
        expires: Option<SystemTime>,
        standing: bool,
    ) -> Result<SendOutcome> {
        take(store, record)?;
        self.send(record, runtime, decision, expires, standing)
            .await
            .map_err(|err| put_back(store, record, err))
    }

    /// Sends the decision reply to the requesting peer; the caller has taken the
    /// request and puts it back if this fails.
    async fn send(
        &self,
        record: &InboundRecord,
        runtime: &MeshRuntime,
        decision: AccessDecision,
        expires: Option<SystemTime>,
        standing: bool,
    ) -> Result<SendOutcome> {
        let reply = decision_reply(
            &record.id,
            &record.thread,
            record.paths.len(),
            decision,
            expires,
            standing,
            &runtime.part_limits(),
        )?;
        Ok(runtime.send_peer(&record.peer_destination, &reply).await?)
    }

    /// Closes a request the peer has heard the decision on: the hook fires and the
    /// human's line is built.
    fn settle(
        &self,
        record: InboundRecord,
        decision: AccessDecision,
        expires: Option<SystemTime>,
        standing: bool,
        sent: SendOutcome,
    ) -> Result<AccessDecisionReport> {
        let InboundRecord {
            id,
            peer_destination,
            peer_identity,
            paths,
            ..
        } = record;
        debug!(
            "Mesh access {} (instance {}) {} on {} paths via {}",
            short(&peer_identity),
            short(&peer_destination),
            decision.wire_name(),
            paths.len(),
            via_word(sent.via)
        );
        self.slot.hooks().fire(MeshEvent::AccessDecided {
            identity: peer_identity,
            destination: peer_destination.clone(),
            access_id: id.clone(),
            decision,
        });
        Ok(AccessDecisionReport {
            id,
            peer_destination,
            path_count: paths.len(),
            decision,
            expires,
            standing,
            via: sent.via,
        })
    }
}

/// Takes the request out of the store: what keeps two processes deciding the same id
/// from both telling the peer, or both writing a grant. The one that finds the record
/// already gone does neither.
fn take(store: &InboundStore, record: &InboundRecord) -> Result<()> {
    if !store.remove(&record.id)? {
        bail!(
            "`{}` was already decided by another process; nothing was sent",
            record.id
        );
    }
    Ok(())
}

/// Puts a taken record back after `err` stopped its decision or answer, so it stays
/// pending to take again; a put-back that fails too is reported beside `err`, since the
/// human needs to know both that the peer heard nothing and that the record is gone.
pub(crate) fn put_back(
    store: &InboundStore,
    record: &InboundRecord,
    err: anyhow::Error,
) -> anyhow::Error {
    match store.upsert(record.clone(), SystemTime::now()) {
        Ok(()) => err,
        Err(put_back) => anyhow!(
            "{err:#}; and `{}` could not be put back as pending: {put_back:#}",
            record.id
        ),
    }
}

/// An allow entry per requested path, each matching that path literally and only for
/// the requesting identity, in the share list `scope` picks.
fn share_standing(shares: &mut ShareSet, record: &InboundRecord, scope: WriteScope) -> Result<()> {
    for path in &record.paths {
        shares.apply(
            Mutation::Allow {
                pattern: globset::escape(path),
                peer: Some(record.peer_identity.clone()),
            },
            scope,
        )?;
    }
    Ok(())
}

/// The reply that settles an admitted access request, on this route and the
/// store-and-forward one alike: a reply in the request's thread, marked answered, whose
/// one data part says `{"access": {"status", "expires"?}}`. `expires` is carried as unix
/// seconds and only for a one-off grant. The paths are never repeated back: the peer
/// knows what it asked for, and the reply may travel through a propagation node.
pub(crate) fn decision_reply(
    record_id: &str,
    thread: &str,
    path_count: usize,
    decision: AccessDecision,
    expires: Option<SystemTime>,
    standing: bool,
    limits: &PartLimits,
) -> Result<OutboundPeer, SendError> {
    let plural = if path_count == 1 { "" } else { "s" };
    let counted = format!("{path_count} path{plural}");
    let content = match (decision, expires) {
        (AccessDecision::Denied, _) => format!("access denied: {counted}"),
        (AccessDecision::Granted, Some(expires)) => {
            format!("access granted: {counted} until {}", rfc3339_utc(expires))
        }
        (AccessDecision::Granted, None) if standing => {
            format!("access granted: {counted}, standing")
        }
        (AccessDecision::Granted, None) => format!("access granted: {counted}"),
    };
    let mut access = json!({ "status": decision.wire_name() });
    if let Some(expires) = expires {
        access["expires"] = json!(
            expires
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64()
        );
    }
    Ok(OutboundPeer::with_parts(
        PeerKind::Reply,
        &content,
        None,
        Some(record_id),
        None,
        vec![RawPart::Data {
            data: json!({ "access": access }),
        }],
        limits,
    )?
    .with_thread(Some(thread.to_string()))?
    .with_disposition(Disposition::Answered, None))
}

/// Whether the share rules already serve every path to `peer`. A root whose case could
/// not be probed shares nothing, as it does for a fetch.
fn already_shared(
    serving: &FetchServing,
    root: &Path,
    peer: &PeerRef<'_>,
    paths: &[String],
) -> bool {
    let Some(case_insensitive) = serving.case_insensitive_for(root) else {
        return false;
    };
    let shares = serving.shares_under(root);
    paths
        .iter()
        .all(|path| shares.is_allowed(peer, path, case_insensitive))
}

/// The refusal, if any, that the open records earn a new access request for `paths`
/// under `id`: any open record under the same id, of either kind and from any peer, is
/// a duplicate whatever the new one names, so the paths the human read stand and an id
/// is never filed twice; among `identity`'s own open access requests the same set of
/// paths already waiting, in any order, is a duplicate; and past
/// `ACCESS_MAX_PENDING_PER_IDENTITY` of them nothing more is filed.
fn rate_rule(
    open: &[InboundRecord],
    identity: &str,
    id: &str,
    paths: &[String],
) -> Option<AccessRefusal> {
    if open.iter().any(|record| record.id == id) {
        return Some(AccessRefusal::Duplicate);
    }
    let mine: Vec<&InboundRecord> = open
        .iter()
        .filter(|record| {
            record.kind == InboundKind::Access && same_hash(&record.peer_identity, identity)
        })
        .collect();
    let asked = path_set(paths);
    if mine.iter().any(|record| path_set(&record.paths) == asked) {
        return Some(AccessRefusal::Duplicate);
    }
    if mine.len() >= ACCESS_MAX_PENDING_PER_IDENTITY {
        return Some(AccessRefusal::TooManyPending);
    }
    None
}

fn path_set(paths: &[String]) -> BTreeSet<&str> {
    paths.iter().map(String::as_str).collect()
}

/// What the person at the keyboard sees: a header naming who asks and how many paths,
/// the paths two to a line with whether each is here and how big, then the reason and
/// the two verbs that settle it on a line of their own. The label and the reason are
/// quoted and each path is in backticks, with any quote or backtick inside them made an
/// apostrophe, so peer text cannot close its own quotes and pose as the frame. Paths
/// and the reason are cut to their display caps so the notification caps never reach
/// the verbs.
fn access_text(label: &str, request: &ValidAccess, root: Option<&Path>) -> String {
    let who = label.replace('"', "'");
    let count = request.paths.len();
    let plural = if count == 1 { "" } else { "s" };
    let mut lines = vec![format!("\"{who}\" asks for {count} path{plural}:")];
    let listed: Vec<String> = request
        .paths
        .iter()
        .map(|path| {
            format!(
                "`{}` ({})",
                shown(&path.replace('`', "'"), ACCESS_PATH_DISPLAY_MAX_CHARS),
                path_state(root, path)
            )
        })
        .collect();
    lines.extend(
        listed
            .chunks(2)
            .map(|pair| format!("  {}", pair.join(", "))),
    );
    let reason = if request.reason.is_empty() {
        String::new()
    } else {
        format!(
            "— \"{}\" · ",
            shown(
                &request.reason.replace('"', "'"),
                ACCESS_REASON_DISPLAY_MAX_CHARS
            )
        )
    };
    lines.push(format!(
        "{reason}grant: .mesh grant {id} [--standing] | refuse: .mesh refuse {id}",
        id = request.id,
    ));
    lines.join("\n")
}

/// `text` whole when it fits in `max_chars`, else its first `max_chars - 1` characters
/// and an ellipsis.
fn shown(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars - 1).collect();
    cut.push('…');
    cut
}

/// `exists, <size>` for a regular file the path names under `root` once both are
/// resolved, so a symlink out of the tree reads as missing; `missing` otherwise. With no
/// root yet there is nothing to look at.
fn path_state(root: Option<&Path>, path: &str) -> String {
    let Some(root) = root else {
        return "unknown until a turn completes".to_string();
    };
    let Ok(wire) = WirePath::parse(path) else {
        return "missing".to_string();
    };
    canonicalize(root)
        .ok()
        .zip(canonicalize(&root.join(wire.to_relative_path())).ok())
        .filter(|(root, file)| file.starts_with(root))
        .and_then(|(_, file)| fs::metadata(file).ok())
        .filter(fs::Metadata::is_file)
        .map_or_else(
            || "missing".to_string(),
            |meta| format!("exists, {}", human_size(meta.len())),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookEvent;
    use crate::mesh::destination_address;
    use crate::mesh::events::{RecordingHookSink, env_value};
    use crate::mesh::grants::GRANT_MAX_PATHS;
    use crate::mesh::idle::IdleSink;
    use crate::mesh::knock::{KnockIntro, KnockSurface, RecordingSurface, knock_message};
    use crate::mesh::message::{
        Part, PeerMessage, RawPeerMessage, from_r3_body, peer_lxmf_message, to_r3_body,
    };
    use crate::mesh::notify::{NOTIFICATION_LINE_MAX_CHARS, Notification};
    use crate::mesh::r3::{PathHash, RequestId, SizeBranch};
    use crate::mesh::test_support::{TempDir, TrustList, snapshot_fixture};
    use crate::mesh::wire_path::WIRE_PATH_MAX_BYTES;
    use rand_core::OsRng;
    use rns_transport::destination::link::LinkId;
    use rns_transport::hash::AddressHash;
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[cfg(unix)]
    use crate::config::Session;
    #[cfg(unix)]
    use crate::mesh::envoy::{EnvoyJob, EnvoySink};
    #[cfg(unix)]
    use crate::mesh::grants::{DEFAULT_GRANT_USES, GrantRecord};
    #[cfg(unix)]
    use crate::mesh::limits::PeerRefusal;
    #[cfg(unix)]
    use crate::mesh::mesh_config_dir;
    #[cfg(unix)]
    use crate::mesh::message::{PeerBody, received_reply};
    #[cfg(unix)]
    use crate::mesh::node::{MeshRuntime, NodeOptions};
    #[cfg(unix)]
    use crate::mesh::peers::PeerSighting;
    #[cfg(unix)]
    use crate::mesh::pending::PendingStore;
    #[cfg(unix)]
    use crate::mesh::protocol::MESH_PROTOCOL_VERSION;
    #[cfg(unix)]
    use crate::mesh::r3::MESSAGE_PATH;
    #[cfg(unix)]
    use crate::mesh::test_support::{
        INTEROP_TIMEOUT, POLL, PeerStub, derived_sighting, loopback_relay, mesh_paths,
        private_config, wait_until,
    };
    #[cfg(unix)]
    use crate::mesh::trust::TrustOptions;
    #[cfg(unix)]
    use rns_transport::iface::tcp_server::TcpServer;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use tokio::task::JoinHandle;

    const IDENTITY: [u8; 16] = [0xab; 16];
    const DESTINATION: [u8; 16] = [0x2b; 16];

    fn identity() -> String {
        hex_lower(&IDENTITY)
    }

    fn destination() -> String {
        hex_lower(&DESTINATION)
    }

    fn map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    fn list(paths: &[&str]) -> Value {
        Value::Array(paths.iter().map(|path| Value::from(*path)).collect())
    }

    fn body(id: &str, paths: &[&str], reason: &str) -> Value {
        map(vec![
            ("v", Value::from(PEER_WIRE_VERSION)),
            ("id", Value::from(id)),
            ("paths", list(paths)),
            ("reason", Value::from(reason)),
        ])
    }

    fn admitted(body: Value, identity: &TransportIdentity) -> AdmittedRequest {
        AdmittedRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: *identity.as_identity(),
            destination_hash: AddressHash::new_from_hex_string(&destination()).unwrap(),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(ACCESS_PATH),
            requested_at: 1_700_000_000.0,
            body,
            branch: SizeBranch::Packet,
        }
    }

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    fn inbound(identity: &str, id: &str, paths: &[&str], reason: &str) -> InboundAccess {
        InboundAccess {
            identity_hash: identity.to_string(),
            destination_hash: destination(),
            request: validate_access(id, strings(paths), reason).unwrap(),
            via: PeerVia::Direct,
        }
    }

    #[derive(Default)]
    struct RecordingIdleSink(parking_lot::Mutex<Vec<IdleNotify>>);

    impl RecordingIdleSink {
        fn texts(&self) -> Vec<String> {
            self.0.lock().iter().map(|note| note.text.clone()).collect()
        }
    }

    impl IdleSink for RecordingIdleSink {
        fn push(&self, note: IdleNotify) -> Result<(), IdleNotify> {
            self.0.lock().push(note);
            Ok(())
        }

        fn request_sync(&self) {}
    }

    /// A slot with everything admission touches but a node: the inbound store, an idle
    /// sink, a hook sink and a snapshot whose `cwd` is `root`. With no node there is no
    /// serving state, so nothing is ever granted at once.
    struct BareSlot {
        slot: Arc<MeshSlot>,
        idle: Arc<RecordingIdleSink>,
        hooks: Arc<RecordingHookSink>,
        root: PathBuf,
        /// Kept so the directory outlives the fixture.
        _tmp: TempDir,
    }

    fn bare_slot(tag: &str) -> BareSlot {
        let tmp = TempDir::new(tag);
        let root = tmp.path.join("ws");
        fs::create_dir_all(&root).unwrap();
        let slot = Arc::new(MeshSlot::default());
        slot.set_inbound_store_for_tests(Arc::new(InboundStore::new(&tmp.path, "inst")));
        let idle = Arc::new(RecordingIdleSink::default());
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let hooks = RecordingHookSink::attach(&slot.hooks());
        slot.publish(crate::mesh::snapshot::MeshSnapshot {
            cwd: root.clone(),
            ..snapshot_fixture()
        });
        BareSlot {
            slot,
            idle,
            hooks,
            root,
            _tmp: tmp,
        }
    }

    fn store_of(slot: &MeshSlot) -> Arc<InboundStore> {
        slot.inbound_store().unwrap()
    }

    fn access_records(slot: &MeshSlot) -> Vec<InboundRecord> {
        store_of(slot).list(SystemTime::now()).unwrap()
    }

    fn question_record(id: &str) -> InboundRecord {
        InboundRecord {
            version: INBOUND_RECORD_VERSION,
            id: id.to_string(),
            peer_destination: destination(),
            peer_identity: identity(),
            thread: id.to_string(),
            question: "may I?".to_string(),
            envoy_question: String::new(),
            received_at: rfc3339_utc(SystemTime::now()),
            kind: InboundKind::Question,
            paths: Vec::new(),
            reason: String::new(),
        }
    }

    /// Whether any map in `value`, at any depth, has a key named `key`.
    fn has_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Map(entries) => entries
                .iter()
                .any(|(name, inner)| name.as_str() == Some(key) || has_key(inner, key)),
            Value::Array(items) => items.iter().any(|item| has_key(item, key)),
            _ => false,
        }
    }

    const EXPIRES_SECS: u64 = 1_790_000_900;

    fn expires() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(EXPIRES_SECS)
    }

    fn granted_data(expires: Option<SystemTime>) -> serde_json::Value {
        let mut access = json!({ "status": "granted" });
        if let Some(expires) = expires {
            access["expires"] = json!(expires.duration_since(UNIX_EPOCH).unwrap().as_secs_f64());
        }
        json!({ "access": access })
    }

    /// `reply` as the requester's node sees it once it has crossed the wire.
    fn delivered(reply: &OutboundPeer) -> PeerMessage {
        let body = from_r3_body(&to_r3_body(reply, 1_700_000_000.0)).unwrap();
        PeerMessage::new(RawPeerMessage {
            source_identity: identity(),
            source_destination: destination(),
            destination: hex_lower(&[0x11; 16]),
            title: body.title,
            content: body.content,
            fields: body.fields,
            timestamp: body.timestamp,
            message_id: body.id,
            in_reply_to: body.in_reply_to,
            kind: body.kind,
            via: PeerVia::Direct,
            thread: body.thread,
            disposition: body.disposition,
            retry_after: body.retry_after,
            parts: body.parts,
            dropped_parts: body.dropped_parts,
        })
    }

    #[test]
    fn access_limits_match_the_grant_store() {
        assert_eq!(ACCESS_MAX_PATHS, GRANT_MAX_PATHS);
    }

    #[test]
    fn an_access_reply_is_a_flat_map_with_status_and_sibling_keys() {
        assert_eq!(
            access_reply("a-1", &AccessOutcome::Pending),
            map(vec![
                ("v", Value::from(1u64)),
                ("id", Value::from("a-1")),
                ("status", Value::from("pending")),
            ])
        );
        assert_eq!(
            access_reply(
                "a-1",
                &AccessOutcome::Granted {
                    expires: 1_790_000_900.0
                }
            ),
            map(vec![
                ("v", Value::from(1u64)),
                ("id", Value::from("a-1")),
                ("status", Value::from("granted")),
                ("expires", Value::from(1_790_000_900.0_f64)),
            ])
        );
        for (refusal, word) in [
            (AccessRefusal::Duplicate, "duplicate"),
            (AccessRefusal::TooManyPending, "too_many_pending"),
        ] {
            assert_eq!(
                access_reply("a-1", &AccessOutcome::Refused(refusal)),
                map(vec![
                    ("v", Value::from(1u64)),
                    ("id", Value::from("a-1")),
                    ("status", Value::from("refused")),
                    ("reason", Value::from(word)),
                ])
            );
        }
    }

    #[tokio::test]
    async fn an_access_body_that_fails_any_rule_earns_the_same_invalid_data_refusal() {
        let fixture = bare_slot("access-invalid");
        let handler = AccessHandler::new(Arc::downgrade(&fixture.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);
        let seventeen: Vec<String> = (0..17).map(|n| format!("src/{n}.rs")).collect();
        let seventeen: Vec<&str> = seventeen.iter().map(String::as_str).collect();
        let long_reason = "r".repeat(ACCESS_REASON_MAX_CHARS + 1);
        let cases: Vec<(&str, Value)> = vec![
            (
                "missing v",
                map(vec![
                    ("id", Value::from("a-1")),
                    ("paths", list(&["src/x.rs"])),
                ]),
            ),
            ("non-wire id", body("a 1", &["src/x.rs"], "")),
            ("no paths", body("a-1", &[], "")),
            ("seventeen paths", body("a-1", &seventeen, "")),
            ("a dot-dot path", body("a-1", &["../x.rs"], "")),
            ("a backslash path", body("a-1", &["src\\x.rs"], "")),
            (
                "a reason past the cap",
                body("a-1", &["src/x.rs"], &long_reason),
            ),
            (
                "paths not a list",
                map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION)),
                    ("id", Value::from("a-1")),
                    ("paths", Value::from("src/x.rs")),
                ]),
            ),
            ("the body not a map", Value::from("src/x.rs")),
        ];
        for (case, body) in cases {
            let reply = handler.handle(admitted(body, &peer)).await;
            assert!(
                matches!(reply, Reply::Code(RefusalCode::InvalidData)),
                "{case}"
            );
        }
        assert!(access_records(&fixture.slot).is_empty());
        assert!(fixture.idle.texts().is_empty());
        assert!(fixture.hooks.snapshot().is_empty());
    }

    #[test]
    fn a_reason_of_exactly_the_cap_is_accepted() {
        let reason = "r".repeat(ACCESS_REASON_MAX_CHARS);
        let valid = validate_access("a-1", strings(&["src/x.rs"]), &reason).unwrap();
        assert_eq!(valid.reason, reason);
        assert!(validate_access("a-1", strings(&["src/x.rs"]), &format!("{reason}r")).is_err());
    }

    #[test]
    fn duplicate_paths_within_one_request_collapse_before_the_count() {
        let mut padded: Vec<String> = (0..15).map(|n| format!("src/{n}.rs")).collect();
        padded.push("src/0.rs".to_string());
        padded.push("src/1.rs".to_string());
        assert_eq!(padded.len(), 17);
        assert!(validate_access("a-1", padded, "").is_err());

        let mut repeated: Vec<String> = (0..14).map(|n| format!("src/{n}.rs")).collect();
        repeated.push("src/0.rs".to_string());
        repeated.push("src/1.rs".to_string());
        let valid = validate_access("a-1", repeated, "").unwrap();
        assert_eq!(valid.paths.len(), 14);
        assert_eq!(valid.paths[0], "src/0.rs");
        assert_eq!(valid.paths[13], "src/13.rs");

        let only_repeats = validate_access("a-1", strings(&["src/x.rs", "src/x.rs"]), "").unwrap();
        assert_eq!(only_repeats.paths, vec!["src/x.rs".to_string()]);
    }

    #[test]
    fn an_access_request_for_an_unshared_path_is_pending_and_stored_as_an_access_record() {
        let fixture = bare_slot("access-pending");
        let outcome = fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/x.rs", "docs/y.md"],
            "need the struct",
        ));
        assert!(matches!(outcome, AccessOutcome::Pending));
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.kind, InboundKind::Access);
        assert_eq!(record.id, "a-1");
        assert_eq!(record.thread, "a-1");
        assert_eq!(record.peer_identity, identity());
        assert_eq!(record.peer_destination, destination());
        assert_eq!(record.paths, strings(&["src/x.rs", "docs/y.md"]));
        assert_eq!(record.reason, "need the struct");
        assert!(record.question.is_empty());
        assert_eq!(fixture.idle.texts().len(), 1);
        let fired = fixture.hooks.drain();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, HookEvent::MeshAccessRequested);
        assert_eq!(env_value(&fired[0].1, "COYOTE_MESH_PATH_COUNT"), Some("2"));
        assert_eq!(env_value(&fired[0].1, "COYOTE_MESH_ACCESS_ID"), Some("a-1"));
    }

    #[test]
    fn a_second_request_for_the_same_path_set_while_the_first_is_pending_is_refused_as_duplicate() {
        let fixture = bare_slot("access-duplicate");
        let first =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-1", &["src/x.rs", "docs/y.md"], ""));
        assert!(matches!(first, AccessOutcome::Pending));

        let again =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-2", &["src/x.rs", "docs/y.md"], ""));
        assert!(matches!(
            again,
            AccessOutcome::Refused(AccessRefusal::Duplicate)
        ));
        let reordered =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-3", &["docs/y.md", "src/x.rs"], ""));
        assert!(matches!(
            reordered,
            AccessOutcome::Refused(AccessRefusal::Duplicate)
        ));
        let different = fixture
            .slot
            .admit_access(inbound(&identity(), "a-4", &["src/x.rs"], ""));
        assert!(matches!(different, AccessOutcome::Pending));

        assert_eq!(access_records(&fixture.slot).len(), 2);
        assert_eq!(fixture.idle.texts().len(), 2);
        assert_eq!(fixture.hooks.snapshot().len(), 2);
    }

    #[test]
    fn a_second_request_reusing_a_pending_id_with_other_paths_is_refused_as_duplicate_and_the_first_paths_stand()
     {
        let fixture = bare_slot("access-duplicate-id");
        let first = fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["README.md"],
            "just the readme",
        ));
        assert_eq!(first, AccessOutcome::Pending);

        let swapped = fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/secrets.rs"],
            "just the readme",
        ));

        assert_eq!(swapped, AccessOutcome::Refused(AccessRefusal::Duplicate));
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "a-1");
        assert_eq!(records[0].paths, strings(&["README.md"]));
        assert_eq!(fixture.idle.texts().len(), 1);
        assert_eq!(fixture.hooks.snapshot().len(), 1);
        assert_eq!(
            rate_rule(&records, &identity(), "a-1", &strings(&["src/secrets.rs"])),
            Some(AccessRefusal::Duplicate)
        );
        assert_eq!(
            rate_rule(&records, &identity(), "a-2", &strings(&["src/secrets.rs"])),
            None
        );
    }

    #[test]
    fn a_removed_access_record_frees_its_path_set_to_be_asked_again() {
        let fixture = bare_slot("access-removed");
        let first = fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert!(matches!(first, AccessOutcome::Pending));
        assert!(store_of(&fixture.slot).remove("a-1").unwrap());

        let again = fixture
            .slot
            .admit_access(inbound(&identity(), "a-2", &["src/x.rs"], ""));
        assert!(matches!(again, AccessOutcome::Pending));
        assert_eq!(access_records(&fixture.slot).len(), 1);
    }

    #[test]
    fn a_sixth_pending_request_from_one_identity_is_refused_as_too_many_pending() {
        let fixture = bare_slot("access-too-many");
        for n in 0..ACCESS_MAX_PENDING_PER_IDENTITY {
            let path = format!("src/{n}.rs");
            let outcome =
                fixture
                    .slot
                    .admit_access(inbound(&identity(), &format!("a-{n}"), &[&path], ""));
            assert!(matches!(outcome, AccessOutcome::Pending), "{n}");
        }
        let sixth = fixture
            .slot
            .admit_access(inbound(&identity(), "a-6", &["src/6.rs"], ""));
        assert!(matches!(
            sixth,
            AccessOutcome::Refused(AccessRefusal::TooManyPending)
        ));
        let other = hex_lower(&[0xcd; 16]);
        let elsewhere = fixture
            .slot
            .admit_access(inbound(&other, "b-1", &["src/6.rs"], ""));
        assert!(matches!(elsewhere, AccessOutcome::Pending));
        assert_eq!(
            access_records(&fixture.slot).len(),
            ACCESS_MAX_PENDING_PER_IDENTITY + 1
        );
        assert_eq!(
            fixture.idle.texts().len(),
            ACCESS_MAX_PENDING_PER_IDENTITY + 1
        );
    }

    /// Both sides of the identity compare are canonical lowercase hex, so the serving
    /// path neither case-folds nor short-circuits on them. The per-identity rule is the
    /// path-set one: a re-used id is a duplicate whoever sends it.
    #[test]
    fn rate_rule_compares_identities_in_constant_time_shape() {
        let source = include_str!("access.rs");
        let production = &source[..source.find("\nmod tests {").unwrap()];
        let needle = ["eq_ignore_", "ascii_case"].concat();
        assert!(
            !production.contains(&needle),
            "the serving path compares identities with same_hash, not {needle}"
        );

        let open = [InboundRecord {
            peer_identity: "ab".repeat(16),
            kind: InboundKind::Access,
            paths: vec!["src/x.rs".to_string()],
            question: String::new(),
            ..question_record("a-1")
        }];
        assert_eq!(
            rate_rule(&open, &"AB".repeat(16), "a-2", &["src/x.rs".to_string()]),
            None,
            "mixed case is another identity under the canonical compare"
        );
        assert_eq!(
            rate_rule(&open, &"ab".repeat(16), "a-2", &["src/x.rs".to_string()]),
            Some(AccessRefusal::Duplicate)
        );
        assert_eq!(
            rate_rule(&open, &"AB".repeat(16), "a-1", &["x".to_string()]),
            Some(AccessRefusal::Duplicate)
        );
    }

    #[test]
    fn a_burst_of_concurrent_requests_from_one_identity_never_files_more_than_the_cap() {
        const BURST: usize = 16;
        let fixture = bare_slot("access-burst");
        let outcomes: Vec<AccessOutcome> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..BURST)
                .map(|n| {
                    let slot = &fixture.slot;
                    scope.spawn(move || {
                        let path = format!("src/{n}.rs");
                        slot.admit_access(inbound(&identity(), &format!("a-{n}"), &[&path], ""))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });

        let refused = outcomes
            .iter()
            .filter(|outcome| **outcome == AccessOutcome::Refused(AccessRefusal::TooManyPending))
            .count();
        let pending = outcomes
            .iter()
            .filter(|outcome| **outcome == AccessOutcome::Pending)
            .count();
        assert_eq!(pending, ACCESS_MAX_PENDING_PER_IDENTITY, "{outcomes:?}");
        assert_eq!(
            refused,
            BURST - ACCESS_MAX_PENDING_PER_IDENTITY,
            "{outcomes:?}"
        );
        assert_eq!(
            access_records(&fixture.slot).len(),
            ACCESS_MAX_PENDING_PER_IDENTITY
        );
        assert_eq!(fixture.idle.texts().len(), ACCESS_MAX_PENDING_PER_IDENTITY);
    }

    #[test]
    fn the_human_line_names_the_peer_the_paths_with_existence_and_size_and_both_verbs() {
        let fixture = bare_slot("access-line");
        fs::create_dir_all(fixture.root.join("src")).unwrap();
        fs::write(fixture.root.join("src/x.rs"), vec![b'x'; 2048]).unwrap();
        fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/x.rs", "src/gone.rs"],
            "need the struct",
        ));
        assert_eq!(
            fixture.idle.texts(),
            vec![
                "\"abababab\" asks for 2 paths:\n  `src/x.rs` (exists, 2 KB), `src/gone.rs` (missing)\n— \"need the struct\" · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
        let notes = fixture.idle.0.lock();
        let note = &notes[0];
        assert_eq!(note.source, Source::Access);
        assert_eq!(note.origin, Origin::Peer("abababab".to_string()));
        assert!(note.model_note.is_none());
        assert!(!note.text.ends_with('\n'));
    }

    #[test]
    fn paths_are_listed_two_to_a_line() {
        let fixture = bare_slot("access-line-pairs");
        fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"],
            "",
        ));
        assert_eq!(
            fixture.idle.texts(),
            vec![
                "\"abababab\" asks for 5 paths:\n  `a.rs` (missing), `b.rs` (missing)\n  `c.rs` (missing), `d.rs` (missing)\n  `e.rs` (missing)\ngrant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_long_path_and_a_long_reason_are_cut_to_their_display_caps() {
        let fixture = bare_slot("access-line-caps");
        let path = format!("src/{}.rs", "p".repeat(200));
        let reason = "r".repeat(ACCESS_REASON_MAX_CHARS);
        fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &[&path], &reason));
        let shown_path = format!("src/{}…", "p".repeat(ACCESS_PATH_DISPLAY_MAX_CHARS - 5));
        let shown_reason = format!("{}…", "r".repeat(ACCESS_REASON_DISPLAY_MAX_CHARS - 1));
        assert_eq!(
            fixture.idle.texts(),
            vec![format!(
                "\"abababab\" asks for 1 path:\n  `{shown_path}` (missing)\n— \"{shown_reason}\" · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
            )]
        );
        let records = access_records(&fixture.slot);
        assert_eq!(records[0].paths, vec![path]);
        assert_eq!(records[0].reason, reason);
    }

    #[test]
    fn the_human_line_keeps_its_verbs_under_the_notification_caps() {
        let fixture = bare_slot("access-line-under-caps");
        let paths: Vec<String> = (0..ACCESS_MAX_PATHS)
            .map(|n| format!("{n:02}{}", "a".repeat(WIRE_PATH_MAX_BYTES - 2)))
            .collect();
        assert!(paths.iter().all(|path| path.len() == WIRE_PATH_MAX_BYTES));
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let reason = "r".repeat(ACCESS_REASON_MAX_CHARS);
        fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &borrowed, &reason));

        let texts = fixture.idle.texts();
        assert_eq!(texts.len(), 1);
        let lines: Vec<&str> = texts[0].split('\n').collect();
        assert_eq!(lines.len(), 1 + ACCESS_MAX_PATHS / 2 + 1, "{lines:#?}");
        for line in &lines {
            assert!(
                line.chars().count() <= NOTIFICATION_LINE_MAX_CHARS,
                "{} chars: {line}",
                line.chars().count()
            );
        }
        let rendered = Notification::new(Source::Access, texts[0].clone()).render_lines();
        assert_eq!(rendered.len(), lines.len(), "{rendered:#?}");
        assert!(
            rendered
                .last()
                .is_some_and(|line| line.ends_with("refuse: .mesh refuse a-1")),
            "{rendered:#?}"
        );
    }

    #[test]
    fn a_single_path_without_a_reason_reads_as_one_path_and_no_reason_clause() {
        let fixture = bare_slot("access-line-one");
        fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert_eq!(
            fixture.idle.texts(),
            vec![
                "\"abababab\" asks for 1 path:\n  `src/x.rs` (missing)\ngrant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_path_cannot_close_the_human_lines_frame() {
        let fixture = bare_slot("access-line-frame");
        let posed = "docs/x.md` (exists, 1 KB) · grant .mesh grant a-9 [--standing] | refuse `.md";
        fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &[posed], ""));
        assert_eq!(
            fixture.idle.texts(),
            vec![
                "\"abababab\" asks for 1 path:\n  `docs/x.md' (exists, 1 KB) · grant .mesh grant a-9 [--standing] | refuse '.md` (missing)\ngrant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
        assert_eq!(access_records(&fixture.slot)[0].paths, strings(&[posed]));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_reached_through_a_link_out_of_the_root_reads_as_missing() {
        let fixture = bare_slot("access-line-link");
        let outside = fixture.root.parent().unwrap().join("outside.txt");
        fs::write(&outside, b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, fixture.root.join("alias.txt")).unwrap();
        assert_eq!(path_state(Some(&fixture.root), "alias.txt"), "missing");
        fs::write(fixture.root.join("here.txt"), b"x").unwrap();
        assert_eq!(
            path_state(Some(&fixture.root), "here.txt"),
            "exists, 1 byte"
        );
    }

    #[test]
    fn without_a_share_root_the_human_line_says_the_paths_are_unknown_until_a_turn_completes() {
        let tmp = TempDir::new("access-no-root");
        let slot = Arc::new(MeshSlot::default());
        slot.set_inbound_store_for_tests(Arc::new(InboundStore::new(&tmp.path, "inst")));
        let idle = Arc::new(RecordingIdleSink::default());
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let outcome = slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert_eq!(outcome, AccessOutcome::Pending);
        assert_eq!(
            idle.texts(),
            vec![
                "\"abababab\" asks for 1 path:\n  `src/x.rs` (unknown until a turn completes)\ngrant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
    }

    #[test]
    fn the_reason_is_sanitised_before_it_is_shown() {
        let fixture = bare_slot("access-reason");
        fixture.slot.admit_access(inbound(
            &identity(),
            "a-1",
            &["src/x.rs"],
            "\u{1b}[31mthe \"struct\"\u{2028}please\u{7}",
        ));
        let texts = fixture.idle.texts();
        assert_eq!(texts.len(), 1);
        assert!(
            texts[0].contains("\n— \"the 'struct' please\" · grant: "),
            "{}",
            texts[0]
        );
        assert_eq!(
            access_records(&fixture.slot)[0].reason,
            "the \"struct\" please"
        );
    }

    #[test]
    fn a_grant_sends_an_answered_reply_with_one_data_part_and_no_paths() {
        let reply = decision_reply(
            "a-1",
            "a-1",
            2,
            AccessDecision::Granted,
            Some(expires()),
            false,
            &PartLimits::default(),
        )
        .unwrap();

        assert_eq!(reply.kind, PeerKind::Reply);
        assert_eq!(reply.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(reply.thread.as_deref(), Some("a-1"));
        assert_eq!(reply.disposition, Some(Disposition::Answered));
        assert_eq!(reply.retry_after, None);
        assert_eq!(reply.title, None);
        assert_eq!(reply.fields, None);
        assert_eq!(
            reply.parts,
            vec![RawPart::Data {
                data: json!({ "access": { "status": "granted", "expires": 1_790_000_900.0 } })
            }]
        );
        assert_eq!(
            reply.content,
            "access granted: 2 paths until 2026-09-21T14:28:20Z"
        );
        let body = to_r3_body(&reply, 1_700_000_000.0);
        assert!(!has_key(&body, "paths"), "{body}");
        assert!(has_key(&body, "expires"));
        let Value::Map(entries) = &body else {
            panic!("not a map");
        };
        let content = entries
            .iter()
            .find(|(key, _)| key.as_str() == Some("content"))
            .map(|(_, value)| value.as_str().unwrap())
            .unwrap();
        assert_eq!(
            content,
            "access granted: 2 paths until 2026-09-21T14:28:20Z"
        );
    }

    #[test]
    fn a_standing_grant_reply_carries_no_expires() {
        let reply = decision_reply(
            "a-1",
            "a-1",
            1,
            AccessDecision::Granted,
            None,
            true,
            &PartLimits::default(),
        )
        .unwrap();

        assert_eq!(
            reply.parts,
            vec![RawPart::Data {
                data: json!({ "access": { "status": "granted" } })
            }]
        );
        assert_eq!(reply.content, "access granted: 1 path, standing");
        assert_eq!(reply.disposition, Some(Disposition::Answered));
        let body = to_r3_body(&reply, 1_700_000_000.0);
        assert!(!has_key(&body, "expires"), "{body}");
        assert!(!has_key(&body, "paths"), "{body}");
    }

    #[test]
    fn a_refusal_sends_the_same_shape_with_status_denied() {
        let reply = decision_reply(
            "a-1",
            "t-9",
            3,
            AccessDecision::Denied,
            None,
            false,
            &PartLimits::default(),
        )
        .unwrap();

        assert_eq!(reply.kind, PeerKind::Reply);
        assert_eq!(reply.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(reply.thread.as_deref(), Some("t-9"));
        assert_eq!(reply.disposition, Some(Disposition::Answered));
        assert_eq!(
            reply.parts,
            vec![RawPart::Data {
                data: json!({ "access": { "status": "denied" } })
            }]
        );
        assert_eq!(reply.content, "access denied: 3 paths");
        let body = to_r3_body(&reply, 1_700_000_000.0);
        assert!(!has_key(&body, "paths"), "{body}");
        assert!(!has_key(&body, "expires"), "{body}");
    }

    #[test]
    fn a_requester_collects_the_decision_under_the_access_id() {
        let slot = MeshSlot::default();
        let now = SystemTime::now();
        slot.correlations()
            .open(PendingRecord {
                version: PENDING_RECORD_VERSION,
                id: "a-1".to_string(),
                peer_destination: destination(),
                peer_identity: identity(),
                thread: "a-1".to_string(),
                question: "access to src/x.rs".to_string(),
                sent_at: rfc3339_utc(now),
                timeout_at: rfc3339_utc(now + Duration::from_secs(60)),
                state: PendingState::Open,
                reply: None,
            })
            .unwrap();
        let reply = decision_reply(
            "a-1",
            "a-1",
            1,
            AccessDecision::Granted,
            Some(expires()),
            false,
            &PartLimits::default(),
        )
        .unwrap();

        slot.deliver_peer(delivered(&reply));

        let answer = slot.correlations().take_answer("a-1").unwrap();
        assert_eq!(
            answer.parts,
            vec![Part::Data {
                data: granted_data(Some(expires()))
            }]
        );
        assert_eq!(answer.disposition, Some(Disposition::Answered));
        assert_eq!(answer.kind, PeerKind::Reply);
        assert_eq!(answer.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(answer.thread.as_deref(), Some("a-1"));
        assert_eq!(answer.dropped_parts, 0);
        assert!(slot.correlations().take_answer("a-1").is_none());
    }

    #[tokio::test]
    async fn a_grant_or_refusal_for_a_question_id_is_refused_with_the_cross_hint() {
        let fixture = bare_slot("access-question-id");
        store_of(&fixture.slot)
            .upsert(question_record("q-1"), SystemTime::now())
            .unwrap();

        let grant = fixture
            .slot
            .access()
            .grant("q-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap_err()
            .to_string();
        let refuse = fixture
            .slot
            .access()
            .refuse("q-1")
            .await
            .unwrap_err()
            .to_string();

        for err in [&grant, &refuse] {
            assert!(err.contains("`q-1` is a question"), "{err}");
            assert!(err.contains(".mesh answer q-1"), "{err}");
        }
        assert!(store_of(&fixture.slot).get("q-1").unwrap().is_some());
        let unknown = fixture
            .slot
            .access()
            .refuse("a-9")
            .await
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("no open access request a-9"), "{unknown}");
        assert!(fixture.hooks.snapshot().is_empty());
    }

    /// An envoy that counts what it is offered; an access request must never reach it.
    #[cfg(unix)]
    #[derive(Default)]
    struct CountingEnvoy(AtomicUsize);

    #[cfg(unix)]
    impl EnvoySink for CountingEnvoy {
        fn accept(&self, _job: EnvoyJob) -> Result<(), PeerRefusal> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn answer(&self, _id: &str, _text: &str) -> bool {
            false
        }

        fn holds(&self, _id: &str) -> bool {
            false
        }

        fn interrupt(&self) {}
    }

    /// A slot with a node installed, so the serving state and the peer table are live:
    /// what the immediate grant and the peer's display name need.
    #[cfg(unix)]
    struct Installed {
        slot: Arc<MeshSlot>,
        runtime: Arc<MeshRuntime>,
        idle: Arc<RecordingIdleSink>,
        hooks: Arc<RecordingHookSink>,
        root: PathBuf,
        relay: JoinHandle<()>,
        tmp: TempDir,
        session: Session,
        port: u16,
    }

    #[cfg(unix)]
    impl Installed {
        async fn start(tag: &str) -> Self {
            let installed = Self::unrooted(tag).await;
            installed.publish_root();
            installed
        }

        /// Joined to a relay with no snapshot published, so there is no share root.
        async fn unrooted(tag: &str) -> Self {
            let (addr, relay, _) = loopback_relay().await;
            Self::start_in(TempDir::new(tag), Session::default(), addr.port(), relay).await
        }

        /// Joined to `stub` as its one peer, each trusting the other, so a decision
        /// reply has somewhere to land.
        async fn beside(tag: &str, stub: &PeerStub) -> Self {
            let installed = Self::start_in(
                TempDir::new(tag),
                Session::default(),
                stub.port(),
                tokio::spawn(std::future::ready(())),
            )
            .await;
            installed.publish_root();
            stub.trust(
                &installed.runtime.current_destination_hash(),
                installed.runtime.fingerprint(),
            );
            installed.learn(stub).await;
            installed
                .runtime
                .trust()
                .trust_destination(
                    installed.slot.as_ref(),
                    &stub.destination_hex(),
                    TrustOptions::default(),
                    SystemTime::now(),
                )
                .unwrap();
            installed.hooks.drain();
            installed
        }

        /// The same instance up again on the same config and cache, which is what the
        /// inbound store, the grants and the trust list are keyed by.
        async fn restart(self, stub: &PeerStub) -> Self {
            assert!(self.slot.stop().await.unwrap());
            let Self {
                tmp, session, port, ..
            } = self;
            let installed =
                Self::start_in(tmp, session, port, tokio::spawn(std::future::ready(()))).await;
            installed.publish_root();
            installed.learn(stub).await;
            installed.hooks.drain();
            installed
        }

        async fn start_in(
            tmp: TempDir,
            mut session: Session,
            port: u16,
            relay: JoinHandle<()>,
        ) -> Self {
            let root = tmp.path.join("ws");
            fs::create_dir_all(root.join("src")).unwrap();
            let slot = Arc::new(MeshSlot::default());
            let hooks = RecordingHookSink::attach(&slot.hooks());
            let runtime = MeshRuntime::start(
                &private_config(port),
                true,
                &mut session,
                mesh_paths(&tmp),
                NodeOptions {
                    hooks: slot.hooks(),
                    ..NodeOptions::default()
                },
            )
            .await
            .unwrap();
            slot.install(Arc::clone(&runtime)).unwrap();
            let idle = Arc::new(RecordingIdleSink::default());
            slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
            hooks.drain();
            Self {
                slot,
                runtime,
                idle,
                hooks,
                root,
                relay,
                tmp,
                session,
                port,
            }
        }

        fn publish_root(&self) {
            self.slot.publish(crate::mesh::snapshot::MeshSnapshot {
                cwd: self.root.clone(),
                ..snapshot_fixture()
            });
        }

        /// Hears `stub` announce, which is what gives the node a path to it. The peer
        /// table is persisted, so after a restart the stub is filed before the announce
        /// even arrives; the wait is on what a send actually needs: the transport's
        /// announce-learned identity behind `resolve_destination` and a route.
        async fn learn(&self, stub: &PeerStub) {
            stub.announce(Some("Stub")).await;
            let peers = self.runtime.peers();
            let to = stub.destination_hex();
            wait_until("the node to file the stub", || peers.get(&to).is_some()).await;
            let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
            while self.runtime.resolve_destination(&to).await.is_none()
                || !self.runtime.path_known(&to).await
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for the node to learn a path to the stub"
                );
                tokio::time::sleep(POLL).await;
            }
        }

        /// Files a pending access request from `stub` for `paths`.
        async fn ask(&self, stub: &PeerStub, id: &str, paths: &[&str]) {
            let request = InboundAccess {
                identity_hash: stub.identity_hex(),
                destination_hash: stub.destination_hex(),
                request: validate_access(id, strings(paths), "").unwrap(),
                via: PeerVia::Direct,
            };
            let slot = Arc::clone(&self.slot);
            let outcome = tokio::task::spawn_blocking(move || slot.admit_access(request))
                .await
                .unwrap();
            assert!(matches!(outcome, AccessOutcome::Pending));
            self.hooks.drain();
        }

        fn grants(&self) -> Vec<GrantRecord> {
            self.runtime.serving().grants().list().unwrap()
        }

        fn shares_yaml(&self) -> serde_yaml::Value {
            let path = mesh_config_dir(&self.tmp.path.join("config")).join("shares.yaml");
            serde_yaml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
        }

        /// Shares `pattern` with the peer whose identity is `peer` through the global list.
        fn share_with(&self, pattern: &str, peer: &str) {
            let path = mesh_config_dir(&self.tmp.path.join("config")).join("shares.yaml");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("version: 1\nallow:\n- pattern: '{pattern}'\n  peer: '{peer}'\n"),
            )
            .unwrap();
        }

        fn name_peer(&self, name: &str) {
            self.runtime.peers().observe(
                PeerSighting {
                    destination_hash: destination(),
                    identity_hash: identity(),
                    name_hash: hex_lower(&[7u8; NAME_HASH_LEN]),
                    display_name: Some(name.to_string()),
                    protocol_version: MESH_PROTOCOL_VERSION,
                    hops: 1,
                },
                SystemTime::now(),
            );
        }

        async fn stop(self) {
            assert!(self.slot.stop().await.unwrap());
            self.relay.abort();
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_attaches_the_slot_as_the_access_surface() {
        let installed = Installed::start("access-attached").await;
        assert!(installed.runtime.access_surface().is_some());
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_access_request_for_paths_already_shared_with_the_peer_is_granted_at_once_without_a_human()
     {
        let installed = Installed::start("access-granted").await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/x.rs", &identity());
        let before = SystemTime::now();

        let outcome = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], "please"))
        })
        .await
        .unwrap();

        let AccessOutcome::Granted { expires } = outcome else {
            panic!("not granted");
        };
        let floor = (before + DEFAULT_GRANT_TTL)
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!(expires >= floor && expires < floor + 60.0, "{expires}");
        assert!(access_records(&installed.slot).is_empty());
        assert!(installed.idle.texts().is_empty());
        let fired = installed.hooks.drain();
        assert_eq!(fired.len(), 2, "{fired:?}");
        assert_eq!(fired[0].0, HookEvent::MeshAccessRequested);
        assert_eq!(fired[1].0, HookEvent::MeshAccessDecided);
        assert_eq!(
            env_value(&fired[1].1, "COYOTE_MESH_DECISION"),
            Some("granted")
        );

        let partly = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-2", &["src/x.rs", "src/y.rs"], ""))
        })
        .await
        .unwrap();
        assert!(matches!(partly, AccessOutcome::Pending));
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_human_line_quotes_the_peers_display_name_with_its_quotes_made_apostrophes() {
        let installed = Installed::start("access-named").await;
        installed.name_peer("Ada \"the\" peer");

        tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""))
        })
        .await
        .unwrap();

        assert_eq!(
            installed.idle.texts(),
            vec![
                "\"Ada 'the' peer\" asks for 1 path:\n  `src/x.rs` (missing)\ngrant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
                    .to_string()
            ]
        );
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_access_request_never_reaches_the_envoy_sink() {
        let installed = Installed::start("access-envoy").await;
        let envoy = Arc::new(CountingEnvoy::default());
        installed
            .slot
            .set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        let handler =
            AccessHandler::new(Arc::downgrade(&installed.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);

        let reply = handler
            .handle(admitted(body("a-1", &["src/x.rs"], "need it"), &peer))
            .await;

        let Reply::Value(value) = reply else {
            panic!("not a value reply");
        };
        assert_eq!(value, access_reply("a-1", &AccessOutcome::Pending));
        assert_eq!(envoy.0.load(Ordering::SeqCst), 0);
        assert_eq!(access_records(&installed.slot).len(), 1);
        assert_eq!(installed.idle.texts().len(), 1);
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn access_events_carry_peer_count_and_decision_but_never_a_path() {
        let installed = Installed::start("access-events").await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/x.rs", &identity());
        let paths = ["src/x.rs", "docs/secret-plan.md"];

        let outcomes = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || {
                (
                    slot.admit_access(inbound(&identity(), "a-1", &paths, "why")),
                    slot.admit_access(inbound(&identity(), "a-2", &paths[..1], "why")),
                )
            }
        })
        .await
        .unwrap();
        assert!(matches!(outcomes.0, AccessOutcome::Pending));
        assert!(matches!(outcomes.1, AccessOutcome::Granted { .. }));

        let fired = installed.hooks.drain();
        let events: Vec<HookEvent> = fired.iter().map(|(event, _)| *event).collect();
        assert_eq!(
            events,
            vec![
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessDecided,
            ]
        );
        let requested = &fired[0].1;
        assert_eq!(
            env_value(requested, "COYOTE_MESH_PEER_IDENTITY"),
            Some(identity().as_str())
        );
        assert_eq!(
            env_value(requested, "COYOTE_MESH_PEER_DESTINATION"),
            Some(destination().as_str())
        );
        assert_eq!(env_value(requested, "COYOTE_MESH_ACCESS_ID"), Some("a-1"));
        assert_eq!(env_value(requested, "COYOTE_MESH_PATH_COUNT"), Some("2"));
        assert_eq!(requested.len(), 4);
        let decided = &fired[2].1;
        assert_eq!(env_value(decided, "COYOTE_MESH_ACCESS_ID"), Some("a-2"));
        assert_eq!(env_value(decided, "COYOTE_MESH_DECISION"), Some("granted"));
        assert_eq!(decided.len(), 4);
        for (_, envs) in &fired {
            for (_, value) in envs {
                for path in paths {
                    assert!(!value.contains(path), "{value}");
                }
                assert!(!value.contains("why"), "{value}");
            }
        }
        installed.stop().await;
    }

    /// The one decision the stub has heard, with its shape checked.
    #[cfg(unix)]
    fn decision_seen(stub: &PeerStub, id: &str) -> PeerBody {
        let mut seen = stub.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        let body = seen.remove(0);
        assert_eq!(body.kind, PeerKind::Reply);
        assert_eq!(body.in_reply_to.as_deref(), Some(id));
        assert_eq!(body.thread.as_deref(), Some(id));
        assert_eq!(body.disposition, Some(Disposition::Answered));
        assert_eq!(body.parts.len(), 1);
        assert_eq!(body.dropped_parts, 0);
        body
    }

    /// One `MeshAccessDecided` for `id` saying `decision`, fired after the reply's own
    /// `MeshMessageSent`: the peer hears first, then the hook.
    #[cfg(unix)]
    fn decided_once(hooks: &RecordingHookSink, id: &str, decision: &str) {
        let fired = hooks.drain();
        let events: Vec<HookEvent> = fired.iter().map(|(event, _)| *event).collect();
        let sent = events
            .iter()
            .position(|event| *event == HookEvent::MeshMessageSent)
            .unwrap_or_else(|| panic!("no send among {events:?}"));
        let decided: Vec<usize> = (0..events.len())
            .filter(|index| events[*index] == HookEvent::MeshAccessDecided)
            .collect();
        assert_eq!(decided.len(), 1, "{events:?}");
        assert!(decided[0] > sent, "{events:?}");
        let envs = &fired[decided[0]].1;
        assert_eq!(env_value(envs, "COYOTE_MESH_ACCESS_ID"), Some(id));
        assert_eq!(env_value(envs, "COYOTE_MESH_DECISION"), Some(decision));
    }

    #[cfg(unix)]
    fn within(expires: SystemTime, from: SystemTime, ttl: Duration) {
        let floor = from + ttl - Duration::from_secs(1);
        assert!(
            expires >= floor && expires < floor + Duration::from_secs(61),
            "{expires:?} is not about {ttl:?} after {from:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_one_off_grant_writes_one_use_per_path_with_the_default_ttl() {
        let stub = PeerStub::listen("access-grant-once-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-grant-once", &stub).await;
        installed
            .ask(&stub, "a-1", &["src/x.rs", "docs/y.md"])
            .await;
        let before = SystemTime::now();

        let report = installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap();

        let expires = report.expires.unwrap();
        within(expires, before, DEFAULT_GRANT_TTL);
        assert_eq!(
            report,
            AccessDecisionReport {
                id: "a-1".to_string(),
                peer_destination: stub.destination_hex(),
                path_count: 2,
                decision: AccessDecision::Granted,
                expires: Some(expires),
                standing: false,
                via: PeerVia::Direct,
            }
        );
        let grants = installed.grants();
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].id, "a-1");
        assert_eq!(grants[0].peer, stub.destination_hex());
        assert_eq!(grants[0].expires, rfc3339_utc(expires));
        let paths: Vec<(&str, u32, u32)> = grants[0]
            .paths
            .iter()
            .map(|path| (path.path.as_str(), path.uses, path.uses_left))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("src/x.rs", DEFAULT_GRANT_USES, DEFAULT_GRANT_USES),
                ("docs/y.md", DEFAULT_GRANT_USES, DEFAULT_GRANT_USES),
            ]
        );
        assert!(access_records(&installed.slot).is_empty());
        decided_once(&installed.hooks, "a-1", "granted");
        let body = decision_seen(&stub, "a-1");
        assert_eq!(
            body.parts,
            vec![RawPart::Data {
                data: granted_data(Some(expires))
            }]
        );
        assert_eq!(
            body.content,
            format!("access granted: 2 paths until {}", rfc3339_utc(expires))
        );
        installed.stop().await;
        stub.stop().await;
    }

    /// A one-off grant whose decision the peer refuses to take is undone whole: the
    /// grant written between the take and the send is taken back, the request is
    /// pending again, the peer heard nothing it could act on and the human reads one
    /// line naming the send, not the race.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_one_off_grant_whose_send_fails_leaves_no_grant_and_the_request_pending() {
        let stub =
            PeerStub::listen("access-one-off-unsent-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        stub.serve(MESSAGE_PATH, Arc::new(Throttling) as Arc<dyn Handler>);
        let installed = Installed::beside("access-one-off-unsent", &stub).await;
        installed
            .ask(&stub, "a-1", &["src/x.rs", "docs/y.md"])
            .await;

        let err = installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("The peer refused the message"), "{err}");
        assert!(!err.contains("already decided"), "{err}");
        assert_eq!(err.lines().count(), 1, "{err}");
        assert!(installed.grants().is_empty(), "{:?}", installed.grants());
        let identity = stub.identity_hex();
        let destination = stub.destination_hex();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        assert!(
            !installed
                .runtime
                .serving()
                .grants()
                .is_granted(&peer, "src/x.rs", SystemTime::now())
                .unwrap()
        );
        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "a-1");
        assert_eq!(records[0].paths, ["src/x.rs", "docs/y.md"]);
        assert!(stub.seen().is_empty());
        let events: Vec<HookEvent> = installed
            .hooks
            .drain()
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert!(
            !events.contains(&HookEvent::MeshAccessDecided),
            "{events:?}"
        );
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grant_for_a_duration_overrides_the_ttl() {
        let stub = PeerStub::listen("access-grant-ttl-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-grant-ttl", &stub).await;
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        let before = SystemTime::now();

        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::OneOff {
                    ttl: Some(Duration::from_secs(60)),
                },
            )
            .await
            .unwrap();

        let expires = report.expires.unwrap();
        within(expires, before, Duration::from_secs(60));
        let grants = installed.grants();
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].expires, rfc3339_utc(expires));
        let body = decision_seen(&stub, "a-1");
        assert_eq!(
            body.content,
            format!("access granted: 1 path until {}", rfc3339_utc(expires))
        );
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_grant_writes_an_allow_entry_per_path_scoped_to_the_requesting_identity() {
        let stub = PeerStub::listen("access-standing-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-standing", &stub).await;
        for name in ["a.rs", "we*ird.rs", "weXird.rs"] {
            fs::write(installed.root.join("src").join(name), b"x").unwrap();
        }
        installed
            .ask(&stub, "a-1", &["src/a.rs", "src/we*ird.rs"])
            .await;

        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            report,
            AccessDecisionReport {
                id: "a-1".to_string(),
                peer_destination: stub.destination_hex(),
                path_count: 2,
                decision: AccessDecision::Granted,
                expires: None,
                standing: true,
                via: PeerVia::Direct,
            }
        );
        let yaml = installed.shares_yaml();
        let allow = yaml["allow"].as_sequence().unwrap();
        let entries: Vec<(&str, &str)> = allow
            .iter()
            .map(|entry| {
                (
                    entry["pattern"].as_str().unwrap(),
                    entry["peer"].as_str().unwrap(),
                )
            })
            .collect();
        let peer = stub.identity_hex();
        assert_eq!(
            entries,
            vec![
                ("src/a.rs", peer.as_str()),
                ("src/we[*]ird.rs", peer.as_str())
            ]
        );
        assert_eq!(globset::escape("src/we*ird.rs"), "src/we[*]ird.rs");
        assert!(
            yaml["override"].as_sequence().is_none_or(Vec::is_empty),
            "{yaml:?}"
        );
        assert!(installed.grants().is_empty());
        let shares = installed.runtime.serving().shares_under(&installed.root);
        let destination = stub.destination_hex();
        let peer = PeerRef {
            identity: &peer,
            destination: &destination,
        };
        assert!(shares.is_allowed(&peer, "src/a.rs", false));
        assert!(shares.is_allowed(&peer, "src/we*ird.rs", false));
        assert!(!shares.is_allowed(&peer, "src/weXird.rs", false));
        let stranger = hex_lower(&[0xcd; 16]);
        let other = PeerRef {
            identity: &stranger,
            destination: &destination,
        };
        assert!(!shares.is_allowed(&other, "src/a.rs", false));
        assert!(access_records(&installed.slot).is_empty());
        decided_once(&installed.hooks, "a-1", "granted");
        let body = decision_seen(&stub, "a-1");
        assert_eq!(
            body.parts,
            vec![RawPart::Data {
                data: granted_data(None)
            }]
        );
        assert_eq!(body.content, "access granted: 2 paths, standing");
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_grant_without_a_share_root_is_refused_with_a_one_off_hint() {
        let installed = Installed::unrooted("access-standing-no-root").await;
        tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""))
        })
        .await
        .unwrap();
        installed.hooks.drain();

        let err = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown until a turn completes"), "{err}");
        assert!(
            err.contains("grant a-1 once with `.mesh grant a-1` instead"),
            "{err}"
        );
        assert_eq!(access_records(&installed.slot).len(), 1);
        assert!(installed.grants().is_empty());
        assert!(installed.hooks.snapshot().is_empty());
        installed.stop().await;
    }

    /// Serves `/message` on a stub in the recorder's place and refuses every message,
    /// so a decision reply fails at once.
    #[cfg(unix)]
    struct Throttling;

    #[cfg(unix)]
    #[async_trait]
    impl Handler for Throttling {
        async fn handle(&self, _request: AdmittedRequest) -> Reply {
            Reply::Code(RefusalCode::Throttled)
        }
    }

    /// Serves `/message` on a stub in the recorder's place: acknowledges each message
    /// and notes whether the share list at `shares` existed when it arrived.
    #[cfg(unix)]
    struct SharesWitness {
        shares: PathBuf,
        heard: parking_lot::Mutex<Vec<(PeerBody, bool)>>,
    }

    #[cfg(unix)]
    #[async_trait]
    impl Handler for SharesWitness {
        async fn handle(&self, request: AdmittedRequest) -> Reply {
            let Ok(body) = from_r3_body(&request.body) else {
                return Reply::Code(RefusalCode::InvalidData);
            };
            let reply = received_reply(&body.id);
            self.heard.lock().push((body, self.shares.exists()));
            Reply::Value(reply)
        }
    }

    #[cfg(unix)]
    fn global_shares_path(installed: &Installed) -> PathBuf {
        mesh_config_dir(&installed.tmp.path.join("config")).join("shares.yaml")
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_grant_whose_decision_cannot_be_sent_writes_nothing_to_the_share_list() {
        let stub =
            PeerStub::listen("access-standing-unsent-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        stub.serve(MESSAGE_PATH, Arc::new(Throttling) as Arc<dyn Handler>);
        let installed = Installed::beside("access-standing-unsent", &stub).await;
        fs::write(installed.root.join("src/x.rs"), b"x").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;

        let err = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(!err.contains("already told yes"), "{err}");
        assert!(!global_shares_path(&installed).exists());
        let identity = stub.identity_hex();
        let destination = stub.destination_hex();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        assert!(
            !installed
                .runtime
                .serving()
                .shares_under(&installed.root)
                .is_allowed(&peer, "src/x.rs", false)
        );
        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "a-1");
        assert!(installed.grants().is_empty());
        assert!(stub.seen().is_empty());
        let events: Vec<HookEvent> = installed
            .hooks
            .drain()
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert!(
            !events.contains(&HookEvent::MeshAccessDecided),
            "{events:?}"
        );
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_grant_writes_the_share_list_only_after_the_peer_heard_yes() {
        let stub =
            PeerStub::listen("access-standing-order-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-standing-order", &stub).await;
        let witness = Arc::new(SharesWitness {
            shares: global_shares_path(&installed),
            heard: parking_lot::Mutex::new(Vec::new()),
        });
        stub.serve(MESSAGE_PATH, Arc::clone(&witness) as Arc<dyn Handler>);
        fs::write(installed.root.join("src/x.rs"), b"x").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;

        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap();

        assert!(report.standing);
        let heard = witness.heard.lock().clone();
        assert_eq!(heard.len(), 1, "{heard:?}");
        let (body, shares_existed) = &heard[0];
        assert!(
            !shares_existed,
            "the share list was written before the peer had the reply"
        );
        assert_eq!(body.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(
            body.parts,
            vec![RawPart::Data {
                data: granted_data(None)
            }]
        );
        let yaml = installed.shares_yaml();
        let allow = yaml["allow"].as_sequence().unwrap();
        assert_eq!(allow.len(), 1, "{yaml:?}");
        assert_eq!(allow[0]["pattern"].as_str(), Some("src/x.rs"));
        assert_eq!(
            allow[0]["peer"].as_str(),
            Some(stub.identity_hex().as_str())
        );
        assert!(access_records(&installed.slot).is_empty());
        decided_once(&installed.hooks, "a-1", "granted");
        installed.stop().await;
        stub.stop().await;
    }

    /// Serves `/message` on a stub in the recorder's place: acknowledges each message
    /// and, before it does, writes `yaml` to the share list at `shares`, which is what
    /// another `.mesh` verb or another Coyote process would do while a decision is in
    /// flight.
    #[cfg(unix)]
    struct SharesEditor {
        shares: PathBuf,
        yaml: &'static str,
    }

    #[cfg(unix)]
    #[async_trait]
    impl Handler for SharesEditor {
        async fn handle(&self, request: AdmittedRequest) -> Reply {
            let Ok(body) = from_r3_body(&request.body) else {
                return Reply::Code(RefusalCode::InvalidData);
            };
            fs::create_dir_all(self.shares.parent().unwrap()).unwrap();
            fs::write(&self.shares, self.yaml).unwrap();
            Reply::Value(received_reply(&body.id))
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_grant_keeps_entries_added_to_the_share_list_while_the_decision_was_in_flight()
     {
        let stub =
            PeerStub::listen("access-standing-edited-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-standing-edited", &stub).await;
        let other = hex_lower(&[0xcd; 16]);
        let edited = "version: 1\nallow:\n- pattern: 'docs/*.md'\n  peer: 'cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd'\ndeny:\n- pattern: 'src/secret.rs'\n";
        stub.serve(
            MESSAGE_PATH,
            Arc::new(SharesEditor {
                shares: global_shares_path(&installed),
                yaml: edited,
            }) as Arc<dyn Handler>,
        );
        fs::write(installed.root.join("src/x.rs"), b"x").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        assert!(!global_shares_path(&installed).exists());

        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap();

        assert!(report.standing);
        let yaml = installed.shares_yaml();
        let allow: Vec<(&str, &str)> = yaml["allow"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["pattern"].as_str().unwrap(),
                    entry["peer"].as_str().unwrap(),
                )
            })
            .collect();
        let peer = stub.identity_hex();
        assert_eq!(
            allow,
            vec![("docs/*.md", other.as_str()), ("src/x.rs", peer.as_str())],
            "{yaml:?}"
        );
        let deny: Vec<&str> = yaml["deny"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|entry| entry["pattern"].as_str().unwrap())
            .collect();
        assert_eq!(deny, vec!["src/secret.rs"], "{yaml:?}");
        assert!(access_records(&installed.slot).is_empty());
        decided_once(&installed.hooks, "a-1", "granted");
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_standing_grant_on_a_share_list_that_does_not_load_sends_nothing_and_leaves_the_request_pending()
     {
        let stub = PeerStub::listen(
            "access-standing-poisoned-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("access-standing-poisoned", &stub).await;
        fs::write(installed.root.join("src/x.rs"), b"x").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        let shares = global_shares_path(&installed);
        fs::create_dir_all(shares.parent().unwrap()).unwrap();
        let malformed = b"version: 1\nallow: [not a list of entries\n";
        fs::write(&shares, malformed).unwrap();

        let err = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("Nothing was written"), "{err}");
        assert!(!err.contains("already told yes"), "{err}");
        assert!(stub.seen().is_empty());
        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "a-1");
        assert!(installed.grants().is_empty());
        let events: Vec<HookEvent> = installed
            .hooks
            .drain()
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert!(
            !events.contains(&HookEvent::MeshAccessDecided),
            "{events:?}"
        );
        assert_eq!(fs::read(&shares).unwrap(), malformed);
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_path_shared_with_another_identity_is_not_granted_at_once() {
        let installed = Installed::start("access-scoped-elsewhere").await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/x.rs", &hex_lower(&[0xcd; 16]));

        let outcome = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""))
        })
        .await
        .unwrap();

        assert_eq!(outcome, AccessOutcome::Pending);
        assert_eq!(access_records(&installed.slot).len(), 1);
        assert_eq!(installed.idle.texts().len(), 1);
        installed.stop().await;
    }

    /// `granted` at once says a path is already fetchable, which the peer could learn
    /// by fetching; it never says a path the rules would cover exists.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rule_covered_path_that_is_missing_is_pending_not_granted() {
        let installed = Installed::start("access-covered-missing").await;
        installed.share_with("src/x.rs", &identity());

        let outcome = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || slot.admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""))
        })
        .await
        .unwrap();

        assert_eq!(outcome, AccessOutcome::Pending);
        assert_eq!(access_records(&installed.slot).len(), 1);
        assert!(
            installed.idle.texts()[0].contains("`src/x.rs` (missing)"),
            "{:?}",
            installed.idle.texts()
        );
        installed.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refusal_removes_the_request_and_writes_no_grant() {
        let stub = PeerStub::listen("access-refuse-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-refuse", &stub).await;
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;

        let report = installed.slot.access().refuse("a-1").await.unwrap();

        assert_eq!(
            report,
            AccessDecisionReport {
                id: "a-1".to_string(),
                peer_destination: stub.destination_hex(),
                path_count: 1,
                decision: AccessDecision::Denied,
                expires: None,
                standing: false,
                via: PeerVia::Direct,
            }
        );
        assert!(access_records(&installed.slot).is_empty());
        assert!(installed.grants().is_empty());
        assert!(
            !mesh_config_dir(&installed.tmp.path.join("config"))
                .join("shares.yaml")
                .exists()
        );
        decided_once(&installed.hooks, "a-1", "denied");
        let body = decision_seen(&stub, "a-1");
        assert_eq!(
            body.parts,
            vec![RawPart::Data {
                data: json!({ "access": { "status": "denied" } })
            }]
        );
        assert_eq!(body.content, "access denied: 1 path");
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pending_access_request_survives_a_restart_and_can_still_be_granted() {
        let stub = PeerStub::listen("access-restart-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-restart", &stub).await;
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;

        let installed = installed.restart(&stub).await;

        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "a-1");
        assert_eq!(records[0].kind, InboundKind::Access);
        let report = installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap();
        assert_eq!(report.via, PeerVia::Direct);
        assert!(access_records(&installed.slot).is_empty());
        assert_eq!(installed.grants().len(), 1);
        decided_once(&installed.hooks, "a-1", "granted");
        decision_seen(&stub, "a-1");
        installed.stop().await;
        stub.stop().await;
    }

    /// `message` as a fetch hands it on once the signer is verified.
    fn fetched(message: &OutboundMessage, source_identity: &str) -> InboundMessage {
        InboundMessage {
            transient_id: [1u8; 32],
            message_id: [2u8; 32],
            source_identity_hash: source_identity.to_string(),
            source_delivery_hash: hex_lower(&[3u8; 16]),
            timestamp: 1_700_000_000.0,
            title: message.title.clone(),
            content: Some(message.content.clone()),
            fields: message.fields.clone(),
            stamp_value: None,
        }
    }

    fn typed(kind: Value, data: Value) -> OutboundMessage {
        OutboundMessage {
            title: None,
            content: b"need the struct".to_vec(),
            fields: Some(Value::Map(vec![
                (Value::from(FIELD_CUSTOM_TYPE), kind),
                (Value::from(FIELD_CUSTOM_DATA), data),
            ])),
        }
    }

    #[test]
    fn scope_access_lxmf_round_trips_and_a_knock_is_not_an_access() {
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let request = validate_access(
            "a-1",
            strings(&["src/x.rs", "docs/y.md"]),
            "need the struct",
        )
        .unwrap();
        let message = access_message(&request, &origin);
        assert_eq!(message.title, None);
        assert_eq!(message.content, b"need the struct");
        assert_eq!(
            decode_access_message(&fetched(&message, &identity())),
            AccessMessage::Access {
                name_hash: origin.0,
                request: request.clone(),
            }
        );

        let knock = knock_message(&KnockIntro::new("hello").unwrap(), &origin);
        assert_eq!(
            decode_access_message(&fetched(&knock, &identity())),
            AccessMessage::NotAnAccess
        );
        let peer = peer_lxmf_message(
            &OutboundPeer::new(PeerKind::Message, "hi", None, None, None).unwrap(),
            &origin,
        );
        assert_eq!(
            decode_access_message(&fetched(&peer, &identity())),
            AccessMessage::NotAnAccess
        );
        let plain = OutboundMessage {
            title: None,
            content: b"hi".to_vec(),
            fields: None,
        };
        assert_eq!(
            decode_access_message(&fetched(&plain, &identity())),
            AccessMessage::NotAnAccess
        );

        let data = map(vec![
            ("name_hash", Value::Binary(origin.0.to_vec())),
            ("id", Value::from("a-1")),
            ("paths", list(&["src/x.rs", "docs/y.md"])),
        ]);
        let as_bytes = typed(Value::Binary(ACCESS_TYPE.as_bytes().to_vec()), data.clone());
        assert_eq!(
            decode_access_message(&fetched(&as_bytes, &identity())),
            AccessMessage::Access {
                name_hash: origin.0,
                request,
            }
        );
        let later_layout = typed(Value::from("scope.access/2"), data);
        assert_eq!(
            decode_access_message(&fetched(&later_layout, &identity())),
            AccessMessage::NotAnAccess
        );
        let without_paths = typed(
            Value::from(ACCESS_TYPE),
            map(vec![
                ("name_hash", Value::Binary(origin.0.to_vec())),
                ("id", Value::from("a-1")),
            ]),
        );
        assert_eq!(
            decode_access_message(&fetched(&without_paths, &identity())),
            AccessMessage::Malformed("paths is missing or not a list")
        );
        let without_data = typed(Value::from(ACCESS_TYPE), Value::from("src/x.rs"));
        assert_eq!(
            decode_access_message(&fetched(&without_data, &identity())),
            AccessMessage::Malformed("custom data is missing or not a map")
        );
    }

    #[test]
    fn a_stored_access_request_is_held_to_the_link_rules() {
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let long_reason = "r".repeat(ACCESS_REASON_MAX_CHARS + 1);
        let unchecked = |id: &str, paths: Vec<String>, reason: &str| ValidAccess {
            id: id.to_string(),
            paths,
            reason: reason.to_string(),
        };
        let cases = [
            (
                unchecked("a-1", strings(&["../x.rs"]), ""),
                "a path is not a wire path",
            ),
            (
                unchecked("a-1", (0..17).map(|n| format!("src/{n}.rs")).collect(), ""),
                "paths names more than the cap allows",
            ),
            (
                unchecked("a-1", strings(&["src/x.rs"]), &long_reason),
                "reason is longer than the cap allows",
            ),
            (
                unchecked("a 1", strings(&["src/x.rs"]), ""),
                "id is not a wire id",
            ),
            (unchecked("a-1", Vec::new(), ""), "paths is empty"),
        ];
        for (request, rule) in cases {
            let message = access_message(&request, &origin);
            assert_eq!(
                decode_access_message(&fetched(&message, &identity())),
                AccessMessage::Malformed(rule),
                "{rule}"
            );
        }
    }

    struct Admitted {
        identity: String,
        destination: String,
        request: ValidAccess,
        via: PeerVia,
    }

    /// A surface that answers every admission with `outcome` and keeps what it was given.
    struct ScriptedSurface {
        outcome: AccessOutcome,
        admitted: parking_lot::Mutex<Vec<Admitted>>,
        settled: parking_lot::Mutex<Vec<(String, String, ValidAccess, f64)>>,
    }

    impl ScriptedSurface {
        fn answering(outcome: AccessOutcome) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                admitted: parking_lot::Mutex::new(Vec::new()),
                settled: parking_lot::Mutex::new(Vec::new()),
            })
        }

        fn admissions(&self) -> usize {
            self.admitted.lock().len()
        }
    }

    impl AccessSurface for ScriptedSurface {
        fn admit_access(&self, request: InboundAccess) -> AccessOutcome {
            self.admitted.lock().push(Admitted {
                identity: request.identity_hash,
                destination: request.destination_hash,
                request: request.request,
                via: request.via,
            });
            self.outcome.clone()
        }

        fn settle_granted_at_once(
            &self,
            identity_hash: &str,
            destination_hash: &str,
            request: &ValidAccess,
            expires: f64,
        ) {
            self.settled.lock().push((
                identity_hash.to_string(),
                destination_hash.to_string(),
                request.clone(),
                expires,
            ));
        }
    }

    #[derive(Default)]
    struct CountingSink(parking_lot::Mutex<Vec<InboundMessage>>);

    impl CountingSink {
        fn count(&self) -> usize {
            self.0.lock().len()
        }
    }

    impl InboundSink for CountingSink {
        fn deliver(&self, message: InboundMessage) {
            self.0.lock().push(message);
        }
    }

    /// The destination a stored request signed by `identity()` from `origin` names.
    fn origin_destination(origin: &OriginName) -> String {
        destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&identity()).unwrap(),
        )
        .to_hex_string()
    }

    #[test]
    fn a_propagated_access_request_from_an_untrusted_instance_is_dropped_before_admission() {
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let request = validate_access("a-1", strings(&["src/x.rs"]), "please").unwrap();
        let message = fetched(&access_message(&request, &origin), &identity());
        let surface = ScriptedSurface::answering(AccessOutcome::Pending);
        let inner = CountingSink::default();
        let (known_only, _known_dir) = TrustList::default()
            .identity(&identity(), false)
            .open("access-route-untrusted");
        let routing = AccessRouting {
            trust: &known_only,
            surface: Some(Arc::clone(&surface) as Arc<dyn AccessSurface>),
            inner: &inner,
        };

        routing.deliver(message.clone());
        assert_eq!(surface.admissions(), 0);
        assert_eq!(inner.count(), 0);

        let knock = knock_message(&KnockIntro::new("hello").unwrap(), &origin);
        routing.deliver(fetched(&knock, &identity()));
        assert_eq!(surface.admissions(), 0);
        assert_eq!(inner.count(), 1);

        let destination = origin_destination(&origin);
        let (trusting, _trusting_dir) = TrustList::default()
            .destination(&destination, &identity())
            .open("access-route-trusted");
        let without_surface = AccessRouting {
            trust: &trusting,
            surface: None,
            inner: &inner,
        };
        without_surface.deliver(message.clone());
        assert_eq!(inner.count(), 1);

        let routing = AccessRouting {
            trust: &trusting,
            surface: Some(Arc::clone(&surface) as Arc<dyn AccessSurface>),
            inner: &inner,
        };
        let unchecked = ValidAccess {
            id: "a-2".to_string(),
            paths: strings(&["../x.rs"]),
            reason: String::new(),
        };
        routing.deliver(fetched(&access_message(&unchecked, &origin), &identity()));
        assert_eq!(surface.admissions(), 0);
        assert_eq!(inner.count(), 1);

        routing.deliver(message);
        let admitted = surface.admitted.lock();
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].identity, identity());
        assert_eq!(admitted[0].destination, destination);
        assert_eq!(admitted[0].request, request);
        assert_eq!(admitted[0].via, PeerVia::StoreAndForward);
        assert!(surface.settled.lock().is_empty());
        assert_eq!(inner.count(), 1);
    }

    #[test]
    fn a_propagated_request_that_is_already_shared_sends_the_decision_reply() {
        let origin = OriginName([8u8; NAME_HASH_LEN]);
        let destination = origin_destination(&origin);
        let request = validate_access("a-1", strings(&["src/x.rs"]), "").unwrap();
        let message = fetched(&access_message(&request, &origin), &identity());
        let inner = CountingSink::default();
        let (trust, _dir) = TrustList::default()
            .destination(&destination, &identity())
            .open("access-route-granted");

        let granting = ScriptedSurface::answering(AccessOutcome::Granted {
            expires: 1_790_000_900.0,
        });
        AccessRouting {
            trust: &trust,
            surface: Some(Arc::clone(&granting) as Arc<dyn AccessSurface>),
            inner: &inner,
        }
        .deliver(message.clone());
        assert_eq!(granting.admissions(), 1);
        assert_eq!(
            *granting.settled.lock(),
            vec![(
                identity(),
                destination.clone(),
                request.clone(),
                1_790_000_900.0
            )]
        );

        let refusing =
            ScriptedSurface::answering(AccessOutcome::Refused(AccessRefusal::TooManyPending));
        AccessRouting {
            trust: &trust,
            surface: Some(Arc::clone(&refusing) as Arc<dyn AccessSurface>),
            inner: &inner,
        }
        .deliver(message);
        assert_eq!(refusing.admissions(), 1);
        assert!(refusing.settled.lock().is_empty());
        assert_eq!(inner.count(), 0);
    }

    /// A trust list holding `identity()` for all destinations and binding `origin` to
    /// another identity, with a key-change recorder attached; the store's protection flag
    /// is left at its default. Returns the bound destination too.
    fn colliding_routing(
        tag: &str,
        origin: &OriginName,
    ) -> (Arc<TrustStore>, Arc<RecordingSurface>, String, TempDir) {
        let bound_to = hex_lower(&[0xcd; 16]);
        let bound = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&bound_to).unwrap(),
        )
        .to_hex_string();
        let (trust, tmp) = TrustList::default()
            .identity(&identity(), true)
            .destination(&bound, &bound_to)
            .open(tag);
        let notes = Arc::new(RecordingSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        (trust, notes, bound, tmp)
    }

    fn mark_on(trust: &TrustStore, bound: &str) -> Option<String> {
        trust
            .records()
            .into_iter()
            .find(|record| record.hash == bound)
            .and_then(|record| record.key_changed)
            .map(|mark| mark.seen_identity)
    }

    /// Routes a stored `a-1` request for `src/x.rs` signed by `identity()` from `origin`
    /// into a surface that holds it, and returns that surface and the inner sink.
    fn deliver_stored(
        trust: &TrustStore,
        origin: &OriginName,
    ) -> (Arc<ScriptedSurface>, CountingSink) {
        let request = validate_access("a-1", strings(&["src/x.rs"]), "please").unwrap();
        let surface = ScriptedSurface::answering(AccessOutcome::Pending);
        let inner = CountingSink::default();
        AccessRouting {
            trust,
            surface: Some(Arc::clone(&surface) as Arc<dyn AccessSurface>),
            inner: &inner,
        }
        .deliver(fetched(&access_message(&request, origin), &identity()));
        (surface, inner)
    }

    /// A stored access request from an identity trusted for all destinations, naming an
    /// instance bound to another identity, is judged as its link request would be: with
    /// collision protection off it is filed, the bound record is marked with the sender
    /// as the identity seen, and the human gets one warning.
    #[test]
    fn a_stored_access_request_from_a_trusted_for_all_identity_over_a_colliding_record_is_filed_with_a_warning()
     {
        let origin = OriginName([9u8; NAME_HASH_LEN]);
        let (trust, notes, bound, _tmp) =
            colliding_routing("access-route-collision-served", &origin);

        let (surface, inner) = deliver_stored(&trust, &origin);

        let admitted = surface.admitted.lock();
        assert_eq!(admitted.len(), 1);
        assert_eq!(admitted[0].identity, identity());
        assert_eq!(admitted[0].destination, origin_destination(&origin));
        assert_eq!(admitted[0].via, PeerVia::StoreAndForward);
        drop(admitted);
        assert_eq!(inner.count(), 0);
        assert_eq!(mark_on(&trust, &bound), Some(identity()));
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("warning: "), "{}", texts[0]);
        assert!(texts[0].contains("is served"), "{}", texts[0]);
    }

    /// The same request under collision protection is dropped as any untrusted instance's
    /// is, the record is marked all the same, and the human gets the error.
    #[test]
    fn collision_protection_refuses_a_stored_access_request_over_a_colliding_record_with_an_error()
    {
        crate::testing::install_log_collector();
        let origin = OriginName([10u8; NAME_HASH_LEN]);
        let (trust, notes, bound, _tmp) =
            colliding_routing("access-route-collision-refused", &origin);
        trust.set_collision_protection(true);

        let (surface, inner) = deliver_stored(&trust, &origin);

        assert_eq!(surface.admissions(), 0, "admission is never asked");
        assert_eq!(inner.count(), 0);
        let expected = format!(
            "Propagated access request from {} dropped: instance {} is not trusted",
            &identity()[..8],
            &origin_destination(&origin)[..8]
        );
        let logs = crate::testing::debug_snapshot();
        assert!(logs.contains(&expected), "{logs:#?}");
        assert_eq!(mark_on(&trust, &bound), Some(identity()));
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains("is refused"), "{}", texts[0]);
    }

    /// Usage probe: the store-and-forward access path judges a presence-only rotation as
    /// the link does. `identity()` is trusted for all destinations and names an instance no
    /// record carries but the peer table holds under another identity trusted for all
    /// destinations. Under `mesh.collision_protection` the request is dropped before
    /// admission is asked, the human gets one `error:` line naming both identities and
    /// `.mesh trust <requester's destination>`, nothing is marked (there is no record to
    /// mark) and `trust.yaml` keeps its bytes; with protection off the same request is
    /// filed and the path adds no line of its own.
    #[test]
    fn usage_probe_a_stored_access_request_over_an_instance_heard_under_another_trusted_for_all_identity_is_refused_under_protection()
     {
        crate::testing::install_log_collector();
        let earlier = hex_lower(&[0x5c; 16]);
        let origin = OriginName([0x5c_u8; NAME_HASH_LEN]);
        let (trust, tmp) = TrustList::default()
            .identity(&identity(), true)
            .identity(&earlier, true)
            .open("access-route-presence-collision-protected");
        let notes = Arc::new(RecordingSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let peers = Arc::new(
            crate::mesh::peers::PeerTable::load(tmp.path.join("peers.json"), SystemTime::now())
                .unwrap(),
        );
        trust.attach_presence(
            Arc::downgrade(&peers) as Weak<dyn crate::mesh::trust::InstancePresence>
        );
        peers.observe(
            PeerSighting {
                destination_hash: destination_address(
                    &origin.0,
                    &AddressHash::new_from_hex_string(&earlier).unwrap(),
                )
                .to_hex_string(),
                identity_hash: earlier.clone(),
                name_hash: hex_lower(&origin.0),
                display_name: None,
                protocol_version: 1,
                hops: 1,
            },
            SystemTime::now(),
        );
        let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
        let before = std::fs::read(&trust_path).unwrap();
        trust.set_collision_protection(true);

        let (surface, inner) = deliver_stored(&trust, &origin);

        assert_eq!(surface.admissions(), 0, "admission is never asked");
        assert_eq!(inner.count(), 0);
        let expected = format!(
            "Propagated access request from {} dropped: instance {} is not trusted",
            &identity()[..8],
            &origin_destination(&origin)[..8]
        );
        let logs = crate::testing::debug_snapshot();
        assert!(logs.contains(&expected), "{logs:#?}");
        assert!(
            trust
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            trust.records()
        );
        assert_eq!(std::fs::read(&trust_path).unwrap(), before);
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("error: "), "{text}");
        assert!(text.contains(&earlier), "{text}");
        assert!(text.contains(&identity()), "{text}");
        assert!(text.contains("is refused when it asks"), "{text}");
        assert!(text.contains("nothing is marked"), "{text}");
        assert!(
            text.contains(&format!(".mesh trust {}", origin_destination(&origin))),
            "{text}"
        );

        trust.set_collision_protection(false);
        let (surface, inner) = deliver_stored(&trust, &origin);
        let admitted = surface.admitted.lock();
        assert_eq!(admitted.len(), 1, "protection off: filed");
        assert_eq!(admitted[0].identity, identity());
        assert_eq!(admitted[0].destination, origin_destination(&origin));
        drop(admitted);
        assert_eq!(inner.count(), 0);
        assert_eq!(
            notes.texts().len(),
            1,
            "the access path adds no line of its own"
        );
        assert!(
            trust
                .records()
                .iter()
                .all(|record| record.key_changed.is_none())
        );
        assert_eq!(std::fs::read(&trust_path).unwrap(), before);
    }

    /// A stranger (no record at all) is silenced before the destination tier: its stored
    /// access request over a colliding record is dropped in either mode, nothing is
    /// marked, `trust.yaml` keeps its bytes and the human hears nothing.
    #[test]
    fn a_strangers_stored_access_request_over_a_colliding_record_is_silent() {
        for protection in [false, true] {
            let origin = OriginName([11u8; NAME_HASH_LEN]);
            let bound_to = hex_lower(&[0xcd; 16]);
            let bound = destination_address(
                &origin.0,
                &AddressHash::new_from_hex_string(&bound_to).unwrap(),
            )
            .to_hex_string();
            let (trust, tmp) = TrustList::default()
                .destination(&bound, &bound_to)
                .open("access-route-collision-stranger");
            trust.set_collision_protection(protection);
            let notes = Arc::new(RecordingSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
            let before = std::fs::read(&trust_path).unwrap();

            let (surface, inner) = deliver_stored(&trust, &origin);

            assert_eq!(surface.admissions(), 0, "protection {protection}");
            assert_eq!(inner.count(), 0, "protection {protection}");
            assert_eq!(mark_on(&trust, &bound), None, "protection {protection}");
            assert!(
                notes.texts().is_empty(),
                "protection {protection}: {:#?}",
                notes.texts()
            );
            assert_eq!(
                std::fs::read(&trust_path).unwrap(),
                before,
                "protection {protection}: trust.yaml untouched"
            );
        }
    }

    /// No collision, no write: a trusted sender whose instance is on no record has its
    /// request filed, leaves `trust.yaml` byte for byte and surfaces nothing.
    #[test]
    fn a_non_colliding_stored_access_request_writes_no_trust_file_and_tells_nobody() {
        let origin = OriginName([12u8; NAME_HASH_LEN]);
        let (trust, tmp) = TrustList::default()
            .identity(&identity(), true)
            .open("access-route-no-collision");
        let notes = Arc::new(RecordingSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
        let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
        let before = std::fs::read(&trust_path).unwrap();

        let (surface, inner) = deliver_stored(&trust, &origin);

        assert_eq!(surface.admissions(), 1);
        assert_eq!(inner.count(), 0);
        assert!(notes.texts().is_empty(), "{:#?}", notes.texts());
        assert_eq!(
            std::fs::read(&trust_path).unwrap(),
            before,
            "trust.yaml untouched"
        );
    }

    /// Usage probe: a blocked identity is silenced at the identity tier, so its stored
    /// access request over a colliding record is dropped in either mode, nothing is
    /// marked, `trust.yaml` keeps its bytes and the human hears nothing.
    #[test]
    fn usage_probe_a_blocked_identitys_stored_access_request_over_a_colliding_record_is_silent() {
        crate::testing::install_log_collector();
        for protection in [false, true] {
            let origin = OriginName([13u8; NAME_HASH_LEN]);
            let bound_to = hex_lower(&[0xcd; 16]);
            let bound = destination_address(
                &origin.0,
                &AddressHash::new_from_hex_string(&bound_to).unwrap(),
            )
            .to_hex_string();
            let (trust, tmp) = TrustList::default()
                .block(&identity())
                .destination(&bound, &bound_to)
                .open("access-route-collision-blocked");
            trust.set_collision_protection(protection);
            let notes = Arc::new(RecordingSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
            let before = std::fs::read(&trust_path).unwrap();

            let (surface, inner) = deliver_stored(&trust, &origin);

            assert_eq!(surface.admissions(), 0, "protection {protection}");
            assert_eq!(inner.count(), 0, "protection {protection}");
            assert_eq!(mark_on(&trust, &bound), None, "protection {protection}");
            assert!(
                notes.texts().is_empty(),
                "protection {protection}: {:#?}",
                notes.texts()
            );
            assert_eq!(
                std::fs::read(&trust_path).unwrap(),
                before,
                "protection {protection}: trust.yaml untouched"
            );
        }
        let logs = crate::testing::debug_snapshot();
        assert!(
            logs.contains(&format!(
                "Propagated access request from {} dropped: blocked identity",
                &identity()[..8]
            )),
            "{logs:#?}"
        );
    }

    /// Usage probe: the identity-standing gate must not regress destination-tier senders.
    /// An identity trusted for its own instance only (no all-destinations grant) still has
    /// its stored access request filed in either mode, with no mark, no line and no write.
    #[test]
    fn usage_probe_a_destination_tier_senders_stored_access_request_for_its_own_instance_is_filed()
    {
        for protection in [false, true] {
            let origin = OriginName([14u8; NAME_HASH_LEN]);
            let (trust, tmp) = TrustList::default()
                .destination(&origin_destination(&origin), &identity())
                .open("access-route-destination-tier-own");
            trust.set_collision_protection(protection);
            let notes = Arc::new(RecordingSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
            let before = std::fs::read(&trust_path).unwrap();

            let (surface, inner) = deliver_stored(&trust, &origin);

            let admitted = surface.admitted.lock();
            assert_eq!(admitted.len(), 1, "protection {protection}");
            assert_eq!(admitted[0].identity, identity());
            assert_eq!(admitted[0].destination, origin_destination(&origin));
            drop(admitted);
            assert_eq!(inner.count(), 0);
            assert_eq!(mark_on(&trust, &origin_destination(&origin)), None);
            assert!(notes.texts().is_empty(), "{:#?}", notes.texts());
            assert_eq!(std::fs::read(&trust_path).unwrap(), before);
        }
    }

    /// Usage probe: an explicit destination allow admits in either mode. With the sender's
    /// recomputed destination on record (and, under protection, the identity trusted for
    /// all destinations too) the stored access request over the old colliding record is
    /// filed, the old record is left unmarked and the human hears nothing.
    #[test]
    fn usage_probe_an_explicit_destination_allow_files_a_stored_access_request_in_either_mode() {
        for (protection, all_destinations) in [(false, false), (true, true), (true, false)] {
            let origin = OriginName([15u8; NAME_HASH_LEN]);
            let bound_to = hex_lower(&[0xcd; 16]);
            let bound = destination_address(
                &origin.0,
                &AddressHash::new_from_hex_string(&bound_to).unwrap(),
            )
            .to_hex_string();
            let mut list = TrustList::default();
            if all_destinations {
                list = list.identity(&identity(), true);
            }
            let (trust, tmp) = list
                .destination(&bound, &bound_to)
                .destination(&origin_destination(&origin), &identity())
                .open("access-route-collision-destination-allow");
            trust.set_collision_protection(protection);
            let notes = Arc::new(RecordingSurface::default());
            trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);
            let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");
            let before = std::fs::read(&trust_path).unwrap();

            let (surface, inner) = deliver_stored(&trust, &origin);

            let tag = format!("protection {protection}, all_destinations {all_destinations}");
            assert_eq!(surface.admissions(), 1, "{tag}");
            assert_eq!(inner.count(), 0, "{tag}");
            assert_eq!(mark_on(&trust, &bound), None, "{tag}: old record unmarked");
            assert_eq!(
                mark_on(&trust, &origin_destination(&origin)),
                None,
                "{tag}: new record unmarked"
            );
            assert!(notes.texts().is_empty(), "{tag}: {:#?}", notes.texts());
            assert_eq!(std::fs::read(&trust_path).unwrap(), before, "{tag}");
        }
    }

    /// Usage probe: the mark is first-wins. A second stored access request from the same
    /// rotated identity over the already-marked record is filed like the first and the
    /// human is not told again.
    #[test]
    fn usage_probe_a_second_stored_access_request_over_a_marked_record_is_filed_and_warns_no_more()
    {
        let origin = OriginName([16u8; NAME_HASH_LEN]);
        let (trust, notes, bound, tmp) = colliding_routing("access-route-collision-twice", &origin);
        let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");

        let (first, _) = deliver_stored(&trust, &origin);
        assert_eq!(first.admissions(), 1);
        assert_eq!(mark_on(&trust, &bound), Some(identity()));
        let marked = std::fs::read(&trust_path).unwrap();

        let (second, inner) = deliver_stored(&trust, &origin);

        assert_eq!(second.admissions(), 1, "the second request is filed too");
        assert_eq!(inner.count(), 0);
        assert_eq!(mark_on(&trust, &bound), Some(identity()), "the mark stands");
        assert_eq!(
            std::fs::read(&trust_path).unwrap(),
            marked,
            "an already-marked record is not written again"
        );
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "told once: {texts:#?}");
        assert!(texts[0].starts_with("warning: "), "{}", texts[0]);
    }

    /// Usage probe: a destination-tier sender (its own instance on record, no
    /// all-destinations grant) whose stored access request names a FOREIGN instance bound
    /// to someone else passes the standing gate, is refused default-closed, marks the
    /// foreign record with itself as the identity seen and earns the human one error; its
    /// own instance's request is then filed as before and the human is not told again.
    #[test]
    fn usage_probe_a_destination_tier_senders_stored_access_request_naming_a_foreign_instance_is_refused_marked_and_an_error()
     {
        let own = OriginName([17u8; NAME_HASH_LEN]);
        let foreign = OriginName([18u8; NAME_HASH_LEN]);
        let bound_to = hex_lower(&[0xcd; 16]);
        let bound = destination_address(
            &foreign.0,
            &AddressHash::new_from_hex_string(&bound_to).unwrap(),
        )
        .to_hex_string();
        let (trust, _tmp) = TrustList::default()
            .destination(&origin_destination(&own), &identity())
            .destination(&bound, &bound_to)
            .open("access-route-destination-tier-foreign");
        let notes = Arc::new(RecordingSurface::default());
        trust.attach_surface(Arc::downgrade(&notes) as Weak<dyn KnockSurface>);

        let (surface, inner) = deliver_stored(&trust, &foreign);

        assert_eq!(surface.admissions(), 0, "refused, never offered");
        assert_eq!(inner.count(), 0);
        assert_eq!(mark_on(&trust, &bound), Some(identity()));
        assert_eq!(mark_on(&trust, &origin_destination(&own)), None);
        let texts = notes.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains("is refused"), "{}", texts[0]);

        let (surface, inner) = deliver_stored(&trust, &own);
        assert_eq!(surface.admissions(), 1, "its own instance is still served");
        assert_eq!(inner.count(), 0);
        assert_eq!(notes.texts().len(), 1, "{:#?}", notes.texts());
    }

    /// Hands every stored peer message straight on, standing in for the slot.
    #[derive(Default)]
    struct HoldingPeerSurface(parking_lot::Mutex<Vec<PeerMessage>>);

    impl crate::mesh::message::PeerSurface for HoldingPeerSurface {
        fn admit_peer_message(
            &self,
            _request: &crate::mesh::message::PeerAdmission,
        ) -> std::result::Result<(), crate::mesh::limits::PeerRefusal> {
            Ok(())
        }

        fn deliver_peer(&self, message: PeerMessage) {
            self.0.lock().push(message);
        }

        fn file_peer(&self, message: PeerMessage) {
            self.0.lock().push(message);
        }

        fn local_destination(&self) -> Option<String> {
            Some(hex_lower(&[0x77; 16]))
        }
    }

    /// Routes a stored peer MESSAGE signed by `identity()` from `origin` the way the
    /// store-and-forward message path does, returning how many reached the surface.
    fn deliver_stored_message(trust: &TrustStore, origin: &OriginName) -> usize {
        let surface = Arc::new(HoldingPeerSurface::default());
        let inner = CountingSink::default();
        let stored = peer_lxmf_message(
            &OutboundPeer::new(PeerKind::Message, "hello", None, None, None).unwrap(),
            origin,
        );
        crate::mesh::message::PeerRouting {
            trust,
            surface: Some(Arc::clone(&surface) as Arc<dyn crate::mesh::message::PeerSurface>),
            inner: &inner,
        }
        .deliver(fetched(&stored, &identity()));
        assert_eq!(inner.count(), 0, "a peer message never falls through");
        surface.0.lock().len()
    }

    /// Usage probe: the two store-and-forward ingress paths share one mark. A rotated
    /// all-destinations identity's stored message and stored access request over the same
    /// colliding record, in either order, mark the record once and tell the human once;
    /// both are served with protection off.
    #[test]
    fn usage_probe_a_stored_message_and_a_stored_access_request_over_one_colliding_record_tell_the_human_once()
     {
        for message_first in [true, false] {
            let origin = OriginName([19u8; NAME_HASH_LEN]);
            let (trust, notes, bound, tmp) =
                colliding_routing("access-route-collision-cross-path", &origin);
            let trust_path = crate::mesh::mesh_config_dir(&tmp.path).join("trust.yaml");

            let (messages, admissions) = if message_first {
                let messages = deliver_stored_message(&trust, &origin);
                let marked = std::fs::read(&trust_path).unwrap();
                let (surface, _) = deliver_stored(&trust, &origin);
                assert_eq!(
                    std::fs::read(&trust_path).unwrap(),
                    marked,
                    "the access request finds the record already marked"
                );
                (messages, surface.admissions())
            } else {
                let (surface, _) = deliver_stored(&trust, &origin);
                let marked = std::fs::read(&trust_path).unwrap();
                let messages = deliver_stored_message(&trust, &origin);
                assert_eq!(
                    std::fs::read(&trust_path).unwrap(),
                    marked,
                    "the message finds the record already marked"
                );
                (messages, surface.admissions())
            };

            assert_eq!(messages, 1, "message first {message_first}: message served");
            assert_eq!(
                admissions, 1,
                "message first {message_first}: access request filed"
            );
            assert_eq!(mark_on(&trust, &bound), Some(identity()));
            let texts = notes.texts();
            assert_eq!(
                texts.len(),
                1,
                "message first {message_first}: told once: {texts:#?}"
            );
            assert!(texts[0].starts_with("warning: "), "{}", texts[0]);
        }
    }

    fn reply_with(entries: Vec<(&str, Value)>) -> Value {
        let mut all = vec![
            ("v", Value::from(PEER_WIRE_VERSION)),
            ("id", Value::from("a-1")),
        ];
        all.extend(entries);
        map(all)
    }

    #[test]
    fn a_reply_with_an_unknown_status_is_a_typed_error_never_a_panic() {
        for outcome in [
            AccessOutcome::Pending,
            AccessOutcome::Granted {
                expires: 1_790_000_900.0,
            },
            AccessOutcome::Refused(AccessRefusal::Duplicate),
            AccessOutcome::Refused(AccessRefusal::TooManyPending),
        ] {
            assert_eq!(
                decode_access_response(&access_reply("a-1", &outcome), "a-1"),
                Ok(outcome)
            );
        }
        assert_eq!(
            decode_access_response(&reply_with(vec![("status", Value::from("later"))]), "a-1"),
            Err(AccessError::UnknownStatus)
        );
        assert_eq!(
            decode_access_response(
                &reply_with(vec![
                    ("status", Value::from("refused")),
                    ("reason", Value::from("busy")),
                ]),
                "a-1"
            ),
            Err(AccessError::UnknownStatus)
        );
        assert_eq!(
            decode_access_response(&reply_with(vec![("status", Value::from("refused"))]), "a-1"),
            Err(AccessError::UnknownStatus)
        );
        assert_eq!(
            decode_access_response(&reply_with(vec![("status", Value::from(2u64))]), "a-1"),
            Err(AccessError::Malformed("status is missing or not text"))
        );
        assert_eq!(
            decode_access_response(
                &reply_with(vec![
                    ("status", Value::from("pending")),
                    ("eta", Value::from(90u64)),
                    ("note", Value::from("the human is away")),
                ]),
                "a-1"
            ),
            Ok(AccessOutcome::Pending)
        );
        assert_eq!(
            decode_access_response(&reply_with(vec![("status", Value::from("granted"))]), "a-1"),
            Err(AccessError::Malformed("expires is missing or not a number"))
        );
        assert_eq!(
            decode_access_response(
                &reply_with(vec![
                    ("status", Value::from("granted")),
                    ("expires", Value::from("soon")),
                ]),
                "a-1"
            ),
            Err(AccessError::Malformed("expires is missing or not a number"))
        );
        assert_eq!(
            decode_access_response(
                &reply_with(vec![
                    ("status", Value::from("granted")),
                    ("expires", Value::from(EXPIRES_SECS)),
                ]),
                "a-1"
            ),
            Ok(AccessOutcome::Granted {
                expires: 1_790_000_900.0
            }),
            "an integer expires reads as the number it is"
        );
    }

    #[test]
    fn a_reply_whose_id_differs_is_malformed() {
        let wrong_id = Err(AccessError::Malformed("id is missing or not the one sent"));
        assert_eq!(
            decode_access_response(&access_reply("a-2", &AccessOutcome::Pending), "a-1"),
            wrong_id
        );
        assert_eq!(
            decode_access_response(
                &map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION)),
                    ("status", Value::from("pending")),
                ]),
                "a-1"
            ),
            wrong_id
        );
        assert_eq!(
            decode_access_response(
                &map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION)),
                    ("id", Value::from(1u64)),
                    ("status", Value::from("pending")),
                ]),
                "a-1"
            ),
            wrong_id
        );
    }

    #[test]
    fn a_reply_without_a_version_is_malformed() {
        let unversioned = Err(AccessError::Malformed(
            "v is missing or not the supported version",
        ));
        assert_eq!(
            decode_access_response(
                &map(vec![
                    ("id", Value::from("a-1")),
                    ("status", Value::from("pending")),
                ]),
                "a-1"
            ),
            unversioned
        );
        assert_eq!(
            decode_access_response(
                &map(vec![
                    ("v", Value::from(PEER_WIRE_VERSION + 1)),
                    ("id", Value::from("a-1")),
                    ("status", Value::from("pending")),
                ]),
                "a-1"
            ),
            unversioned
        );
        assert_eq!(
            decode_access_response(&Value::from("pending"), "a-1"),
            Err(AccessError::Malformed("the body is not a map"))
        );
    }

    #[test]
    fn a_peer_without_an_access_provider_is_reported_as_not_serving_access_not_as_malformed() {
        let no_provider = DispatchError::NoProvider {
            path: ACCESS_PATH.to_string(),
        };
        assert_eq!(
            read_access_reply(&no_provider.to_value(), "a-1"),
            Err(AccessError::NotServed(no_provider.clone()))
        );
        let unknown_path = DispatchError::UnknownPath {
            path_hash: hex_lower(&[0x77; 16]),
        };
        assert_eq!(
            read_access_reply(&unknown_path.to_value(), "a-1"),
            Err(AccessError::NotServed(unknown_path))
        );
        assert!(
            AccessError::NotServed(no_provider)
                .to_string()
                .contains("peer does not take access requests")
        );
        assert_eq!(
            read_access_reply(&access_reply("a-1", &AccessOutcome::Pending), "a-1"),
            Ok(AccessOutcome::Pending)
        );
        assert_eq!(
            read_access_reply(&Value::from("pending"), "a-1"),
            Err(AccessError::Malformed("the body is not a map"))
        );
    }

    #[test]
    fn every_access_error_reads_as_prose_with_the_next_step() {
        for err in [
            AccessError::NotRunning,
            AccessError::Untrusted,
            AccessError::UnknownDestination,
            AccessError::Invalid("a path is not a wire path"),
            AccessError::NotFiled("the pending store is read-only".to_string()),
            AccessError::Refused(RefusalCode::InvalidData),
            AccessError::NotServed(DispatchError::NoProvider {
                path: ACCESS_PATH.to_string(),
            }),
            AccessError::UnknownStatus,
            AccessError::Malformed("status is missing or not text"),
            AccessError::Direct(R3Error::LinkClosed),
            AccessError::IncompatibleVersion {
                destination: destination(),
                found: Some(9),
                min: 1,
                max: 1,
            },
            AccessError::NoPropagationNode,
            AccessError::Propagation(PropagationError::StampExhausted),
        ] {
            let text = err.to_string();
            assert!(!text.is_empty(), "{err:?}");
            assert!(!text.ends_with('\n'), "{text}");
        }
        assert!(AccessError::Untrusted.to_string().contains(".mesh trust"));
        assert!(
            AccessError::UnknownDestination
                .to_string()
                .contains(".mesh peers")
        );
        assert!(
            AccessError::UnknownStatus
                .to_string()
                .contains("peer sent an unknown status")
        );
        assert_eq!(
            AccessError::Invalid("paths is empty").to_string(),
            "The access request was refused: paths is empty"
        );
        assert_eq!(
            AccessError::NotFiled("disk full".to_string()).to_string(),
            "The access request was not sent because it could not be filed as pending: disk full"
        );
    }

    #[cfg(unix)]
    enum Script {
        Answer(AccessOutcome),
        Refuse(RefusalCode),
    }

    /// Serves `/access` on a stub with whatever the script says, keeping each request
    /// it was asked.
    #[cfg(unix)]
    struct ScriptedAccess {
        script: parking_lot::Mutex<Script>,
        asked: parking_lot::Mutex<Vec<ValidAccess>>,
    }

    #[cfg(unix)]
    impl ScriptedAccess {
        fn answering(outcome: AccessOutcome) -> Arc<Self> {
            Arc::new(Self {
                script: parking_lot::Mutex::new(Script::Answer(outcome)),
                asked: parking_lot::Mutex::new(Vec::new()),
            })
        }

        fn set(&self, script: Script) {
            *self.script.lock() = script;
        }

        fn asked(&self) -> Vec<ValidAccess> {
            self.asked.lock().clone()
        }
    }

    #[cfg(unix)]
    #[async_trait]
    impl Handler for ScriptedAccess {
        async fn handle(&self, request: AdmittedRequest) -> Reply {
            let Ok(access) = decode_access(&request.body) else {
                return Reply::Code(RefusalCode::InvalidData);
            };
            let id = access.id.clone();
            self.asked.lock().push(access);
            match &*self.script.lock() {
                Script::Answer(outcome) => Reply::Value(access_reply(&id, outcome)),
                Script::Refuse(code) => Reply::Code(*code),
            }
        }
    }

    #[cfg(unix)]
    fn pending_store(installed: &Installed) -> PendingStore {
        PendingStore::new(
            installed.runtime.cache_dir(),
            &installed.runtime.current_instance_id(),
        )
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_access_opens_a_correlation_only_while_the_answer_is_pending() {
        let stub = PeerStub::listen("access-request-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let responder = ScriptedAccess::answering(AccessOutcome::Pending);
        stub.serve(ACCESS_PATH, Arc::clone(&responder) as Arc<dyn Handler>);
        let installed = Installed::beside("access-request", &stub).await;
        let to = stub.destination_hex();
        let paths = strings(&["src/x.rs", "docs/y.md"]);

        let pending = installed
            .slot
            .request_access(&to, &paths, "need the struct")
            .await
            .unwrap();

        assert!(is_wire_id(&pending.id), "{}", pending.id);
        assert_eq!(pending.status, AccessOutcome::Pending);
        assert_eq!(pending.via, PeerVia::Direct);
        let correlation = installed.slot.correlations().get(&pending.id).unwrap();
        assert_eq!(correlation.record.thread, pending.id);
        assert_eq!(correlation.record.question, "access: src/x.rs, docs/y.md");
        assert_eq!(correlation.record.peer_destination, to);
        assert_eq!(correlation.record.peer_identity, stub.identity_hex());
        assert_eq!(correlation.record.state, PendingState::Open);
        assert!(correlation.reply.is_none());
        let on_disk = pending_store(&installed).list(SystemTime::now()).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].id, pending.id);
        assert_eq!(
            responder.asked(),
            vec![ValidAccess {
                id: pending.id.clone(),
                paths: paths.clone(),
                reason: "need the struct".to_string(),
            }]
        );

        responder.set(Script::Answer(AccessOutcome::Granted {
            expires: 1_790_000_900.0,
        }));
        let granted = installed
            .slot
            .request_access(&to, &paths[..1], "")
            .await
            .unwrap();
        assert_eq!(
            granted.status,
            AccessOutcome::Granted {
                expires: 1_790_000_900.0
            }
        );
        assert_eq!(granted.via, PeerVia::Direct);
        assert!(installed.slot.correlations().get(&granted.id).is_none());

        responder.set(Script::Answer(AccessOutcome::Refused(
            AccessRefusal::Duplicate,
        )));
        let refused = installed
            .slot
            .request_access(&to, &paths, "")
            .await
            .unwrap();
        assert_eq!(
            refused.status,
            AccessOutcome::Refused(AccessRefusal::Duplicate)
        );
        assert!(installed.slot.correlations().get(&refused.id).is_none());

        responder.set(Script::Refuse(RefusalCode::InvalidData));
        let err = installed
            .slot
            .request_access(&to, &paths, "")
            .await
            .unwrap_err();
        assert_eq!(err, AccessError::Refused(RefusalCode::InvalidData));

        let open: Vec<String> = installed
            .slot
            .correlations()
            .list()
            .iter()
            .map(|correlation| correlation.record.id.clone())
            .collect();
        assert_eq!(open, vec![pending.id.clone()]);
        let on_disk = pending_store(&installed).list(SystemTime::now()).unwrap();
        assert_eq!(on_disk.len(), 1);
        assert_eq!(responder.asked().len(), 4);
        assert!(stub.seen().is_empty());
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_access_refuses_an_untrusted_or_unknown_destination_before_sending() {
        let stub = PeerStub::listen("access-refused-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let responder = ScriptedAccess::answering(AccessOutcome::Pending);
        stub.serve(ACCESS_PATH, Arc::clone(&responder) as Arc<dyn Handler>);
        let installed = Installed::beside("access-refused", &stub).await;
        let to = stub.destination_hex();
        let paths = strings(&["src/x.rs"]);
        let slot = &installed.slot;

        let invalid = slot
            .request_access(&to, &strings(&["../x.rs"]), "")
            .await
            .unwrap_err();
        assert_eq!(invalid, AccessError::Invalid("a path is not a wire path"));

        let stranger = hex_lower(&[0x5a; 16]);
        let unknown = slot
            .request_access(&stranger, &paths, "")
            .await
            .unwrap_err();
        assert_eq!(unknown, AccessError::Untrusted);
        let garbled = slot
            .request_access("not a hash", &paths, "")
            .await
            .unwrap_err();
        assert_eq!(garbled, AccessError::Untrusted);

        let silent = derived_sighting("access-silent", None);
        let silent_destination = silent.destination_hash.clone();
        installed.runtime.peers().observe(silent, SystemTime::now());
        installed
            .runtime
            .trust()
            .trust_destination(
                slot.as_ref(),
                &silent_destination,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let unannounced = slot
            .request_access(&silent_destination, &paths, "")
            .await
            .unwrap_err();
        assert_eq!(unannounced, AccessError::UnknownDestination);

        let store = pending_store(&installed);
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "not a pending question\n").unwrap();
        let not_filed = slot.request_access(&to, &paths, "").await.unwrap_err();
        let AccessError::NotFiled(reason) = not_filed else {
            panic!("{not_filed:?}");
        };
        assert!(reason.contains("pending store"), "{reason}");
        fs::remove_file(store.path()).unwrap();

        installed
            .runtime
            .trust()
            .untrust_destination(slot.as_ref(), &to, SystemTime::now(), false)
            .unwrap();
        let withdrawn = slot.request_access(&to, &paths, "").await.unwrap_err();
        assert_eq!(withdrawn, AccessError::Untrusted);

        assert!(responder.asked().is_empty());
        assert!(slot.correlations().list().is_empty());
        installed.stop().await;
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_grant_a_stored_request_earns_at_once_reaches_the_peer_as_a_decision_reply() {
        let stub =
            PeerStub::listen("access-stored-grant-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let installed = Installed::beside("access-stored-grant", &stub).await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/x.rs", &stub.identity_hex());
        let request = validate_access("a-1", strings(&["src/x.rs"]), "").unwrap();
        let message = fetched(
            &access_message(&request, &stub.origin()),
            &stub.identity_hex(),
        );
        let inner = Arc::new(CountingSink::default());
        let before = SystemTime::now();

        tokio::task::spawn_blocking({
            let trust = installed.runtime.trust();
            let slot = Arc::clone(&installed.slot);
            let inner = Arc::clone(&inner);
            move || {
                AccessRouting {
                    trust: &trust,
                    surface: Some(slot as Arc<dyn AccessSurface>),
                    inner: inner.as_ref(),
                }
                .deliver(message);
            }
        })
        .await
        .unwrap();

        wait_until("the stub to hear the grant", || stub.seen().len() == 1).await;
        let body = decision_seen(&stub, "a-1");
        let RawPart::Data { data } = &body.parts[0] else {
            panic!("{:?}", body.parts);
        };
        assert_eq!(data["access"]["status"], json!("granted"));
        let expires = Duration::from_secs_f64(data["access"]["expires"].as_f64().unwrap());
        within(UNIX_EPOCH + expires, before, DEFAULT_GRANT_TTL);
        assert!(
            body.content.starts_with("access granted: 1 path until "),
            "{}",
            body.content
        );
        assert_eq!(inner.count(), 0);
        assert!(access_records(&installed.slot).is_empty());
        assert!(installed.idle.texts().is_empty());
        let events: Vec<HookEvent> = installed
            .hooks
            .drain()
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert_eq!(
            events,
            vec![
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessDecided,
                HookEvent::MeshMessageSent,
            ]
        );
        installed.stop().await;
        stub.stop().await;
    }

    // ---- spec-first tests written from the wire contract, not from the handler ----

    /// Unknown keys are ignored on receipt. A body with keys the receiver has never
    /// heard of, at the top level and inside the list, is admitted exactly as the bare
    /// body would be: `pending`, filed, surfaced once.
    #[tokio::test]
    async fn usage_probe_unknown_keys_in_an_access_body_are_ignored_on_receipt() {
        let fixture = bare_slot("usage-probe-access-unknown-keys");
        let handler = AccessHandler::new(Arc::downgrade(&fixture.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);
        let body = map(vec![
            ("v", Value::from(PEER_WIRE_VERSION)),
            ("id", Value::from("a-1")),
            ("paths", list(&["src/x.rs"])),
            ("reason", Value::from("need the struct")),
            ("priority", Value::from("urgent")),
            ("nested", map(vec![("deep", Value::from(1u64))])),
            ("flags", Value::Array(vec![Value::from(true)])),
        ]);

        let reply = handler.handle(admitted(body, &peer)).await;

        let Reply::Value(value) = reply else {
            panic!("not a value reply");
        };
        assert_eq!(value, access_reply("a-1", &AccessOutcome::Pending));
        assert!(!has_key(&value, "priority"));
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].paths, strings(&["src/x.rs"]));
        assert_eq!(records[0].reason, "need the struct");
        assert_eq!(fixture.idle.texts().len(), 1);
    }

    /// On the wire, with no snapshot published, the request is still answered
    /// `pending` (never refused, never granted) and filed, and the human line says the
    /// paths are unknown until a turn completes.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_without_a_share_root_the_wire_answer_is_pending_and_the_request_is_filed()
    {
        let installed = Installed::unrooted("usage-probe-access-unrooted").await;
        let handler =
            AccessHandler::new(Arc::downgrade(&installed.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);

        let reply = handler
            .handle(admitted(
                body("a-1", &["src/x.rs", "docs/y.md"], "why"),
                &peer,
            ))
            .await;

        let Reply::Value(value) = reply else {
            panic!("not a value reply");
        };
        assert_eq!(value, access_reply("a-1", &AccessOutcome::Pending));
        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, InboundKind::Access);
        let texts = installed.idle.texts();
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(
            texts[0].contains(
                "asks for 2 paths:\n  `src/x.rs` (unknown until a turn completes), `docs/y.md` (unknown until a turn completes)\n"
            ),
            "{}",
            texts[0]
        );
        assert!(!texts[0].contains("exists"), "{}", texts[0]);
        assert!(!texts[0].contains("missing"), "{}", texts[0]);
        installed.stop().await;
    }

    /// A request is decided once. A second `grant` or a `refuse` after the
    /// grant is a typed error naming the id, the peer hears exactly one decision and the
    /// `MeshAccessDecided` hook fires exactly once.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_decided_request_cannot_be_decided_again() {
        let stub = PeerStub::listen(
            "usage-probe-access-decided-twice-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("usage-probe-access-decided-twice", &stub).await;
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;

        installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap();
        let again = installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap_err()
            .to_string();
        let refused = installed
            .slot
            .access()
            .refuse("a-1")
            .await
            .unwrap_err()
            .to_string();

        assert!(again.contains("no open access request a-1"), "{again}");
        assert!(refused.contains("no open access request a-1"), "{refused}");
        assert_eq!(installed.grants().len(), 1);
        decided_once(&installed.hooks, "a-1", "granted");
        decision_seen(&stub, "a-1");
        assert_eq!(stub.seen().len(), 1, "the peer heard one decision");
        installed.stop().await;
        stub.stop().await;
    }

    /// Two processes deciding the same request: both find it pending, one takes it out
    /// of the store and sends, and the other, reaching the take second, bails naming
    /// the race, sends nothing and writes no grant, so the peer hears exactly one
    /// decision and the grant on file is the one it heard of. The late process is
    /// played by a one-off `grant` whose lookup ran before the other's decision and
    /// whose take runs after it, asking a longer TTL than the winner's so its grant
    /// would be told apart.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_another_process_decided_first_is_not_decided_again() {
        let stub = PeerStub::listen(
            "access-decided-elsewhere-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("access-decided-elsewhere", &stub).await;
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        let late = installed.slot.access();
        let (store, record, runtime) = late.pending("a-1").unwrap();

        let report = installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap();
        let err = late
            .grant_once(&store, record, &runtime, Some(Duration::from_secs(3_600)))
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "`a-1` was already decided by another process; nothing was sent"
        );
        assert!(access_records(&installed.slot).is_empty());
        let grants = installed.grants();
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(
            parse_rfc3339(&grants[0].expires),
            report.expires,
            "the grant on file is the winner's, not the loser's hour-long one"
        );
        decided_once(&installed.hooks, "a-1", "granted");
        decision_seen(&stub, "a-1");
        installed.stop().await;
        stub.stop().await;
    }

    /// The per-identity cap counts PENDING entries only. Once the sixth request
    /// is refused as `too_many_pending`, refusing one of the five reopens the cap and the
    /// next request is filed again.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_deciding_one_pending_request_reopens_the_per_identity_cap() {
        let stub = PeerStub::listen(
            "usage-probe-access-cap-reopens-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("usage-probe-access-cap-reopens", &stub).await;
        for n in 0..ACCESS_MAX_PENDING_PER_IDENTITY {
            let path = format!("src/{n}.rs");
            installed.ask(&stub, &format!("a-{n}"), &[&path]).await;
        }
        let admit = |id: &'static str, path: &'static str| {
            let slot = Arc::clone(&installed.slot);
            let identity = stub.identity_hex();
            let destination = stub.destination_hex();
            tokio::task::spawn_blocking(move || {
                slot.admit_access(InboundAccess {
                    identity_hash: identity,
                    destination_hash: destination,
                    request: validate_access(id, strings(&[path]), "").unwrap(),
                    via: PeerVia::Direct,
                })
            })
        };

        let sixth = admit("a-6", "src/6.rs").await.unwrap();
        assert!(
            matches!(sixth, AccessOutcome::Refused(AccessRefusal::TooManyPending)),
            "{sixth:?}"
        );
        installed.slot.access().refuse("a-0").await.unwrap();
        let seventh = admit("a-7", "src/7.rs").await.unwrap();

        assert!(matches!(seventh, AccessOutcome::Pending), "{seventh:?}");
        let ids: Vec<String> = access_records(&installed.slot)
            .into_iter()
            .map(|record| record.id)
            .collect();
        assert_eq!(ids.len(), ACCESS_MAX_PENDING_PER_IDENTITY);
        assert!(!ids.contains(&"a-0".to_string()), "{ids:?}");
        assert!(!ids.contains(&"a-6".to_string()), "{ids:?}");
        assert!(ids.contains(&"a-7".to_string()), "{ids:?}");
        installed.stop().await;
        stub.stop().await;
    }

    /// Spec tension, pinned as observed (advisory in the probe verdict): the Approach
    /// says "immediate `granted` when every path is already served to that peer", and
    /// `is_served` does count a live one-off grant. The admission rule consults the share
    /// RULES only, so a peer that still holds an unspent one-off grant and asks again is
    /// `pending` (the human is asked a second time) rather than `granted`; the earlier
    /// grant is left untouched, so asking never spends a use.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_asking_again_while_holding_a_live_one_off_grant_is_pending_and_spends_nothing()
     {
        let stub = PeerStub::listen(
            "usage-probe-access-reask-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("usage-probe-access-reask", &stub).await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        installed
            .slot
            .access()
            .grant("a-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap();
        installed.hooks.drain();
        let destination = stub.destination_hex();
        let identity = stub.identity_hex();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        assert!(
            installed
                .runtime
                .serving()
                .grants()
                .is_granted(&peer, "src/x.rs", SystemTime::now())
                .unwrap(),
            "the one-off grant is live"
        );

        let again = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            let identity = identity.clone();
            let destination = destination.clone();
            move || {
                slot.admit_access(InboundAccess {
                    identity_hash: identity,
                    destination_hash: destination,
                    request: validate_access("a-2", strings(&["src/x.rs"]), "").unwrap(),
                    via: PeerVia::Direct,
                })
            }
        })
        .await
        .unwrap();

        assert!(matches!(again, AccessOutcome::Pending), "{again:?}");
        let grants = installed.grants();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].paths[0].uses_left, DEFAULT_GRANT_USES);
        assert_eq!(access_records(&installed.slot).len(), 1);
        installed.stop().await;
        stub.stop().await;
    }

    // ---- spec-first probes of the admission hardening, read from its contract ----

    /// "granted at once" means every asked path is fetchable by that peer now. A rule
    /// such as `src/**` covers a directory's NAME, but a directory is not fetchable, so a
    /// request naming one, alone or mixed with a served regular file, is `pending` and
    /// goes to the human (grant-all-or-nothing on the immediate answer).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_rule_covered_directory_alone_or_mixed_with_a_served_file_is_pending() {
        let installed = Installed::start("usage-probe-access-dir").await;
        fs::create_dir_all(installed.root.join("src/sub")).unwrap();
        fs::write(installed.root.join("src/sub/f.rs"), b"fn f() {}").unwrap();
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.share_with("src/**", &identity());

        let admit = |id: &'static str, paths: &'static [&'static str]| {
            let slot = Arc::clone(&installed.slot);
            tokio::task::spawn_blocking(move || {
                slot.admit_access(inbound(&identity(), id, paths, ""))
            })
        };
        // Positive control: the rule does grant a regular file at once.
        let file = admit("a-0", &["src/x.rs"]).await.unwrap();
        assert!(matches!(file, AccessOutcome::Granted { .. }), "{file:?}");

        let directory = admit("a-1", &["src/sub"]).await.unwrap();
        let mixed = admit("a-2", &["src/x.rs", "src/sub"]).await.unwrap();

        assert_eq!(directory, AccessOutcome::Pending);
        assert_eq!(mixed, AccessOutcome::Pending);
        let records = access_records(&installed.slot);
        let mut ids: Vec<&str> = records.iter().map(|record| record.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["a-1", "a-2"]);
        let texts = installed.idle.texts();
        assert_eq!(texts.len(), 2, "{texts:?}");
        assert!(
            texts.iter().all(|text| text.contains("`src/sub`")),
            "{texts:?}"
        );
        installed.stop().await;
    }

    /// Standing grant, write failure AFTER the peer heard yes (the decision is sent
    /// first by contract): the peer holds a `granted` standing reply, the decided hook
    /// and the returned error tells the human the peer was already told yes and to run
    /// the grant again — so the request must still be open for that re-run, and the
    /// re-run must then write the allow entry. Observed and pinned: `mesh.access.decided`
    /// does NOT fire for the yes the peer already holds; it fires once, on the re-run
    /// that settles the request (which also sends the peer a second `granted` reply).
    /// A hook consumer therefore never hears of a yes the human does not re-run.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_standing_grant_whose_share_list_cannot_be_written_says_the_peer_already_heard_yes()
     {
        use std::os::unix::fs::PermissionsExt;

        let stub = PeerStub::listen(
            "usage-probe-access-standing-ro-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("usage-probe-access-standing-ro", &stub).await;
        fs::write(installed.root.join("src/x.rs"), b"struct X;").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        let config_dir = mesh_config_dir(&installed.tmp.path.join("config"));
        fs::create_dir_all(&config_dir).unwrap();
        let writable = fs::metadata(&config_dir).unwrap().permissions();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let err = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap_err()
            .to_string();

        fs::set_permissions(&config_dir, writable).unwrap();
        assert!(err.contains("already told yes"), "{err}");
        assert!(err.contains("a-1"), "{err}");
        assert!(!global_shares_path(&installed).exists(), "{err}");
        assert!(installed.grants().is_empty());
        let body = decision_seen(&stub, "a-1");
        assert_eq!(
            body.parts,
            vec![RawPart::Data {
                data: granted_data(None)
            }]
        );
        let fired: Vec<HookEvent> = installed
            .hooks
            .drain()
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert_eq!(fired, vec![HookEvent::MeshMessageSent], "{fired:?}");
        let records = access_records(&installed.slot);
        assert_eq!(
            records.len(),
            1,
            "the request must stay open for the re-run the error asks for: {records:?}"
        );

        // The re-run the error asks for: the allow entry lands this time.
        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap();
        assert!(report.standing);
        assert_eq!(
            stub.seen().len(),
            2,
            "the re-run told the peer yes a second time"
        );
        decided_once(&installed.hooks, "a-1", "granted");
        let yaml = installed.shares_yaml();
        let allow = yaml["allow"].as_sequence().unwrap();
        assert_eq!(allow.len(), 1, "{yaml:?}");
        assert_eq!(allow[0]["pattern"].as_str(), Some("src/x.rs"));
        assert_eq!(
            allow[0]["peer"].as_str(),
            Some(stub.identity_hex().as_str())
        );
        assert!(access_records(&installed.slot).is_empty());
        installed.stop().await;
        stub.stop().await;
    }

    /// `refused` carries its `reason` as a SIBLING key, and the duplicate
    /// rule to pending entries: the same path set asked under a NEW id while the first is
    /// pending is `duplicate`; after the first is refused by the human the same set is
    /// admitted again and the human sees a second line.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_refused_request_frees_its_path_set_for_a_fresh_ask() {
        let stub = PeerStub::listen(
            "usage-probe-access-refree-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("usage-probe-access-refree", &stub).await;
        installed
            .ask(&stub, "a-1", &["src/x.rs", "docs/y.md"])
            .await;
        let admit = |id: &'static str| {
            let slot = Arc::clone(&installed.slot);
            let identity = stub.identity_hex();
            let destination = stub.destination_hex();
            tokio::task::spawn_blocking(move || {
                slot.admit_access(InboundAccess {
                    identity_hash: identity,
                    destination_hash: destination,
                    // Order and repeats must not defeat the set comparison.
                    request: validate_access(
                        id,
                        strings(&["docs/y.md", "src/x.rs", "docs/y.md"]),
                        "",
                    )
                    .unwrap(),
                    via: PeerVia::Direct,
                })
            })
        };

        let duplicate = admit("a-2").await.unwrap();
        assert_eq!(
            duplicate,
            AccessOutcome::Refused(AccessRefusal::Duplicate),
            "{duplicate:?}"
        );
        let wire = access_reply("a-2", &duplicate);
        let keys: Vec<&str> = wire
            .as_map()
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str().unwrap())
            .collect();
        assert!(
            keys.contains(&"status") && keys.contains(&"reason"),
            "{wire:?}"
        );
        assert_eq!(installed.idle.texts().len(), 1);

        installed.slot.access().refuse("a-1").await.unwrap();
        let fresh = admit("a-3").await.unwrap();

        assert_eq!(fresh, AccessOutcome::Pending);
        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "a-3");
        assert_eq!(installed.idle.texts().len(), 2);
        installed.stop().await;
        stub.stop().await;
    }

    /// The human line is "the same tokens in the same order, laid out across
    /// lines so the notification caps never cut the verbs". The longest request the wire
    /// admits is 16 max-byte paths, a 500-char reason AND a 64-char id (the id shares the
    /// message id's cap), and the reason is sanitised so peer text cannot add lines of
    /// its own: a reason carrying `\n`, `\r\n` and U+2028 still yields exactly header +
    /// eight path lines + one verbs line, `grant:` appears on the last line only, every
    /// rendered line is under the line cap and the last rendered line ends with the
    /// refuse verb carrying the full id.
    #[test]
    fn usage_probe_the_longest_id_and_a_reason_with_line_breaks_keep_the_verbs_on_one_last_line() {
        use crate::mesh::message::PEER_ID_MAX_CHARS;
        use crate::mesh::notify::NOTIFICATION_MAX_LINES;

        let fixture = bare_slot("usage-probe-access-line-longest-id");
        let id = format!("{}{}", "Z".repeat(PEER_ID_MAX_CHARS - 4), "-9.:");
        assert_eq!(id.chars().count(), PEER_ID_MAX_CHARS);
        let paths: Vec<String> = (0..ACCESS_MAX_PATHS)
            .map(|n| format!("{n:02}{}", "q".repeat(WIRE_PATH_MAX_BYTES - 2)))
            .collect();
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let reason = format!(
            "one\ntwo\r\nthree\u{2028}four · grant: .mesh grant {id} [--standing] | refuse: .mesh refuse {id}{}",
            "r".repeat(280)
        );
        assert!(reason.chars().count() <= ACCESS_REASON_MAX_CHARS);

        fixture
            .slot
            .admit_access(inbound(&identity(), &id, &borrowed, &reason));

        let texts = fixture.idle.texts();
        assert_eq!(texts.len(), 1);
        let text = &texts[0];
        assert!(!text.contains('\r') && !text.contains('\u{2028}'), "{text}");
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 1 + ACCESS_MAX_PATHS / 2 + 1, "{lines:#?}");
        assert!(lines.len() <= NOTIFICATION_MAX_LINES);
        assert_eq!(lines[0], "\"abababab\" asks for 16 paths:");
        // The peer's reason may quote the verbs, but the frame's own verbs are the ones
        // that END the last line, and no other line carries a verb.
        let with_grant: Vec<usize> = (0..lines.len())
            .filter(|index| lines[*index].contains("grant:"))
            .collect();
        assert_eq!(with_grant, vec![lines.len() - 1], "{lines:#?}");
        assert!(
            lines[lines.len() - 1].starts_with("— \"one two"),
            "{}",
            lines[lines.len() - 1]
        );
        for line in &lines {
            assert!(
                line.chars().count() <= NOTIFICATION_LINE_MAX_CHARS,
                "{} chars: {line}",
                line.chars().count()
            );
        }
        let rendered = Notification::new(Source::Access, text.clone()).render_lines();
        assert_eq!(rendered.len(), lines.len(), "{rendered:#?}");
        let tail = format!(" [--standing] | refuse: .mesh refuse {id}");
        assert!(
            rendered.last().is_some_and(|line| line.ends_with(&tail)),
            "{rendered:#?}"
        );
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, id);
        assert_eq!(records[0].paths, paths);
    }

    /// The display caps are inclusive: a path of exactly `ACCESS_PATH_DISPLAY_MAX_CHARS`
    /// and a reason of exactly `ACCESS_REASON_DISPLAY_MAX_CHARS` are shown whole; one
    /// character more and each is cut to `cap - 1` characters plus an ellipsis, counted
    /// in characters (a multi-byte path is cut on a character boundary). The records keep
    /// the full text either way.
    #[test]
    fn usage_probe_display_caps_are_inclusive_and_count_characters() {
        let fixture = bare_slot("usage-probe-access-line-cap-boundary");
        let at_cap_path = "p".repeat(ACCESS_PATH_DISPLAY_MAX_CHARS);
        let over_cap_path = "é".repeat(ACCESS_PATH_DISPLAY_MAX_CHARS + 1);
        let at_cap_reason = "r".repeat(ACCESS_REASON_DISPLAY_MAX_CHARS);
        let over_cap_reason = "ß".repeat(ACCESS_REASON_DISPLAY_MAX_CHARS + 1);

        fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &[&at_cap_path], &at_cap_reason));
        fixture.slot.admit_access(inbound(
            &identity(),
            "a-2",
            &[&over_cap_path],
            &over_cap_reason,
        ));

        let texts = fixture.idle.texts();
        assert_eq!(texts.len(), 2, "{texts:#?}");
        assert_eq!(
            texts[0],
            format!(
                "\"abababab\" asks for 1 path:\n  `{at_cap_path}` (missing)\n— \"{at_cap_reason}\" · grant: .mesh grant a-1 [--standing] | refuse: .mesh refuse a-1"
            )
        );
        let shown_path = format!("{}…", "é".repeat(ACCESS_PATH_DISPLAY_MAX_CHARS - 1));
        let shown_reason = format!("{}…", "ß".repeat(ACCESS_REASON_DISPLAY_MAX_CHARS - 1));
        assert_eq!(
            texts[1],
            format!(
                "\"abababab\" asks for 1 path:\n  `{shown_path}` (missing)\n— \"{shown_reason}\" · grant: .mesh grant a-2 [--standing] | refuse: .mesh refuse a-2"
            )
        );
        let records = access_records(&fixture.slot);
        let by_id = |id: &str| {
            records
                .iter()
                .find(|record| record.id == id)
                .unwrap()
                .clone()
        };
        assert_eq!(by_id("a-2").paths, vec![over_cap_path]);
        assert_eq!(by_id("a-2").reason, over_cap_reason);
    }

    /// Serves `/message` in the recorder's place: records each body and, before it
    /// answers the FIRST one, writes `yaml` to the share list — what another process or
    /// verb could do while the decision is in flight.
    #[cfg(unix)]
    struct RecordingSharesEditor {
        shares: PathBuf,
        yaml: String,
        seen: parking_lot::Mutex<Vec<PeerBody>>,
    }

    #[cfg(unix)]
    #[async_trait]
    impl Handler for RecordingSharesEditor {
        async fn handle(&self, request: AdmittedRequest) -> Reply {
            let Ok(body) = from_r3_body(&request.body) else {
                return Reply::Code(RefusalCode::InvalidData);
            };
            let id = body.id.clone();
            let mut seen = self.seen.lock();
            if seen.is_empty() {
                fs::create_dir_all(self.shares.parent().unwrap()).unwrap();
                fs::write(&self.shares, &self.yaml).unwrap();
            }
            seen.push(body);
            drop(seen);
            Reply::Value(received_reply(&id))
        }
    }

    /// A standing grant re-loads the share list AFTER the send, which adds a failure
    /// point the pre-flight cannot see: a list that was fine before the send and is
    /// poisoned by the time of the write. The peer must have heard yes
    /// before standing entries are written, so the peer holds exactly one `granted`
    /// standing reply, the error says so and names the id, the poisoned bytes are left
    /// untouched, nothing standing is written, the request stays pending (no decided
    /// hook), and the re-run the error asks for — once the list is repaired — writes the
    /// entry beside the repaired list's own and settles the request.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_share_list_poisoned_while_the_standing_decision_was_in_flight_says_already_told_yes_and_stays_pending()
     {
        let stub = PeerStub::listen(
            "usage-probe-access-standing-poisoned-inflight-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed =
            Installed::beside("usage-probe-access-standing-poisoned-inflight", &stub).await;
        let shares = global_shares_path(&installed);
        let malformed = "version: 1\nallow: [not a list of entries\n";
        let editor = Arc::new(RecordingSharesEditor {
            shares: shares.clone(),
            yaml: malformed.to_string(),
            seen: parking_lot::Mutex::new(Vec::new()),
        });
        stub.serve(MESSAGE_PATH, Arc::clone(&editor) as Arc<dyn Handler>);
        fs::write(installed.root.join("src/x.rs"), b"x").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs"]).await;
        assert!(!shares.exists());

        let err = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("already told yes"), "{err}");
        assert!(err.contains("a-1"), "{err}");
        assert_eq!(fs::read_to_string(&shares).unwrap(), malformed);
        let heard = editor.seen.lock().clone();
        assert_eq!(heard.len(), 1, "{heard:?}");
        assert_eq!(heard[0].kind, PeerKind::Reply);
        assert_eq!(heard[0].in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(heard[0].disposition, Some(Disposition::Answered));
        assert_eq!(
            heard[0].parts,
            vec![RawPart::Data {
                data: granted_data(None)
            }]
        );
        assert!(installed.grants().is_empty());
        let fired: Vec<HookEvent> = installed
            .hooks
            .drain()
            .iter()
            .map(|(event, _)| *event)
            .collect();
        assert_eq!(fired, vec![HookEvent::MeshMessageSent], "{fired:?}");
        let records = access_records(&installed.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "a-1");

        // Repair the list (with an entry of its own) and re-run: the entry lands beside
        // the repaired list's entry and the request settles.
        let other = hex_lower(&[0xcd; 16]);
        let repaired = "version: 1\nallow:\n- pattern: 'docs/*.md'\n  peer: 'cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd'\n";
        fs::write(&shares, repaired).unwrap();
        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap();
        assert!(report.standing);
        assert_eq!(
            editor.seen.lock().len(),
            2,
            "the re-run told the peer yes again"
        );
        decided_once(&installed.hooks, "a-1", "granted");
        let yaml = installed.shares_yaml();
        let allow: Vec<(&str, &str)> = yaml["allow"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["pattern"].as_str().unwrap(),
                    entry["peer"].as_str().unwrap(),
                )
            })
            .collect();
        let peer = stub.identity_hex();
        assert_eq!(
            allow,
            vec![("docs/*.md", other.as_str()), ("src/x.rs", peer.as_str())],
            "{yaml:?}"
        );
        assert!(access_records(&installed.slot).is_empty());
        installed.stop().await;
        stub.stop().await;
    }

    /// A grant writes standing entries "per the write rule", and the write rule's
    /// `apply` is idempotent: when the edit made while the decision was in flight already
    /// added the very entry the grant would write (same pattern, same peer) for one of
    /// TWO requested paths, the grant adds nothing for that path and exactly one entry
    /// for the other — no duplicate rows, every requested path covered, the in-flight
    /// edit's unrelated rows kept.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_standing_grant_for_two_paths_does_not_duplicate_an_entry_added_while_in_flight()
     {
        let stub = PeerStub::listen(
            "usage-probe-access-standing-dedup-stub",
            TcpServer::DEFAULT_CLIENT_MTU,
        )
        .await;
        let installed = Installed::beside("usage-probe-access-standing-dedup", &stub).await;
        let peer = stub.identity_hex();
        let edited = format!(
            "version: 1\nallow:\n- pattern: 'src/x.rs'\n  peer: '{peer}'\ndeny:\n- pattern: 'src/secret.rs'\n"
        );
        let editor = Arc::new(RecordingSharesEditor {
            shares: global_shares_path(&installed),
            yaml: edited,
            seen: parking_lot::Mutex::new(Vec::new()),
        });
        stub.serve(MESSAGE_PATH, Arc::clone(&editor) as Arc<dyn Handler>);
        fs::write(installed.root.join("src/x.rs"), b"x").unwrap();
        fs::write(installed.root.join("src/y.rs"), b"y").unwrap();
        installed.ask(&stub, "a-1", &["src/x.rs", "src/y.rs"]).await;

        let report = installed
            .slot
            .access()
            .grant(
                "a-1",
                GrantKind::Standing {
                    scope: WriteScope::Auto,
                },
            )
            .await
            .unwrap();

        assert!(report.standing);
        assert_eq!(editor.seen.lock().len(), 1);
        let yaml = installed.shares_yaml();
        let allow: Vec<(&str, &str)> = yaml["allow"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["pattern"].as_str().unwrap(),
                    entry["peer"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            allow,
            vec![("src/x.rs", peer.as_str()), ("src/y.rs", peer.as_str())],
            "{yaml:?}"
        );
        let deny: Vec<&str> = yaml["deny"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|entry| entry["pattern"].as_str().unwrap())
            .collect();
        assert_eq!(deny, vec!["src/secret.rs"], "{yaml:?}");
        assert!(installed.grants().is_empty());
        assert!(access_records(&installed.slot).is_empty());
        decided_once(&installed.hooks, "a-1", "granted");
        installed.stop().await;
        stub.stop().await;
    }

    // ---- usage-probe spec-first tests: per-identity sets, deny-first admission, the
    // debug log and a re-used id ----

    /// Duplicate-set admission is judged per identity and on the collapsed set: a
    /// second identity may ask for exactly the set a first one has pending, while the
    /// first identity's re-ask that only repeats a path is still the same set.
    #[test]
    fn usage_probe_duplicate_sets_are_judged_per_identity_and_after_collapsing_repeats() {
        let fixture = bare_slot("access-dup-identity");
        let other = hex_lower(&[0xcd; 16]);
        let first =
            fixture
                .slot
                .admit_access(inbound(&identity(), "a-1", &["src/x.rs", "docs/y.md"], ""));
        assert_eq!(first, AccessOutcome::Pending);

        let elsewhere =
            fixture
                .slot
                .admit_access(inbound(&other, "c-1", &["docs/y.md", "src/x.rs"], ""));
        assert_eq!(elsewhere, AccessOutcome::Pending);

        let repeated = fixture.slot.admit_access(inbound(
            &identity(),
            "a-2",
            &["src/x.rs", "src/x.rs", "docs/y.md"],
            "",
        ));
        assert_eq!(repeated, AccessOutcome::Refused(AccessRefusal::Duplicate));

        let superset = fixture.slot.admit_access(inbound(
            &identity(),
            "a-3",
            &["src/x.rs", "docs/y.md", "README.md"],
            "",
        ));
        assert_eq!(superset, AccessOutcome::Pending);

        let records = access_records(&fixture.slot);
        let ids: Vec<&str> = records.iter().map(|record| record.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "{ids:?}");
        assert!(ids.contains(&"a-1") && ids.contains(&"c-1") && ids.contains(&"a-3"));
        assert_eq!(fixture.idle.texts().len(), 3);
        assert_eq!(fixture.hooks.snapshot().len(), 3);
    }

    /// Admission is deny-first like a fetch: a path an allow reaches but a user deny
    /// names is `pending`, not `granted`, while a sibling the same allow reaches is
    /// granted at once; a set mixing the two is pending as a whole.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_user_deny_keeps_an_allowed_path_from_being_granted_at_once() {
        let installed = Installed::start("access-deny-first").await;
        fs::write(installed.root.join("src/open.rs"), b"pub struct Open;").unwrap();
        fs::write(installed.root.join("src/secret.rs"), b"struct Secret;").unwrap();
        let shares = mesh_config_dir(&installed.tmp.path.join("config")).join("shares.yaml");
        fs::create_dir_all(shares.parent().unwrap()).unwrap();
        fs::write(
            &shares,
            format!(
                "version: 1\nallow:\n- pattern: 'src/**'\n  peer: '{}'\ndeny:\n- pattern: 'src/secret.rs'\n",
                identity()
            ),
        )
        .unwrap();

        let (denied, open, mixed) = tokio::task::spawn_blocking({
            let slot = Arc::clone(&installed.slot);
            move || {
                (
                    slot.admit_access(inbound(&identity(), "a-1", &["src/secret.rs"], "")),
                    slot.admit_access(inbound(&identity(), "a-2", &["src/open.rs"], "")),
                    slot.admit_access(inbound(
                        &identity(),
                        "a-3",
                        &["src/open.rs", "src/secret.rs"],
                        "",
                    )),
                )
            }
        })
        .await
        .unwrap();

        assert_eq!(denied, AccessOutcome::Pending);
        assert!(matches!(open, AccessOutcome::Granted { .. }), "{open:?}");
        assert_eq!(mixed, AccessOutcome::Pending);
        let records = access_records(&installed.slot);
        let mut ids: Vec<&str> = records.iter().map(|record| record.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["a-1", "a-3"], "{records:?}");
        assert_eq!(installed.idle.texts().len(), 2);
        let fired = installed.hooks.drain();
        let events: Vec<HookEvent> = fired.iter().map(|(event, _)| *event).collect();
        assert_eq!(
            events,
            vec![
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessRequested,
                HookEvent::MeshAccessDecided,
                HookEvent::MeshAccessRequested,
            ]
        );
        assert_eq!(env_value(&fired[2].1, "COYOTE_MESH_ACCESS_ID"), Some("a-2"));
        installed.stop().await;
    }

    /// On the link path every refusal is logged at debug naming the rule the wire
    /// carries (`duplicate`, `too_many_pending`) or the validation failure, and no
    /// debug line from the handler ever carries a requested path or the reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_refusals_are_logged_at_debug_with_the_rule_and_never_a_path_or_reason() {
        use crate::testing::{debug_snapshot, install_log_collector};

        install_log_collector();
        let fixture = bare_slot("access-debug-log");
        let handler = AccessHandler::new(Arc::downgrade(&fixture.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);
        let id8 = short(&peer.as_identity().address_hash.to_hex_string()).to_string();
        let mine = |line: &String| line.contains(&id8) && line.contains("/access");
        let path = "src/probe-leak-zq.rs";
        let reason = "probe-leak-reason-zq";

        let before = debug_snapshot().iter().filter(|line| mine(line)).count();
        let first = handler
            .handle(admitted(body("a-1", &[path], reason), &peer))
            .await;
        assert!(matches!(first, Reply::Value(_)));
        let duplicate = handler
            .handle(admitted(body("a-2", &[path], reason), &peer))
            .await;
        let Reply::Value(value) = &duplicate else {
            panic!("not a wire reply");
        };
        assert_eq!(
            value
                .as_map()
                .unwrap()
                .iter()
                .find(|(key, _)| key.as_str() == Some("reason"))
                .and_then(|(_, value)| value.as_str()),
            Some("duplicate")
        );
        for n in 2..=ACCESS_MAX_PENDING_PER_IDENTITY {
            let reply = handler
                .handle(admitted(
                    body(
                        &format!("a-{n}"),
                        &[&format!("src/probe-leak-{n}.rs")],
                        reason,
                    ),
                    &peer,
                ))
                .await;
            assert!(matches!(reply, Reply::Value(_)), "{n}");
        }
        let sixth = handler
            .handle(admitted(
                body("a-9", &["src/probe-leak-9.rs"], reason),
                &peer,
            ))
            .await;
        let Reply::Value(value) = &sixth else {
            panic!("not a wire reply");
        };
        assert_eq!(
            value
                .as_map()
                .unwrap()
                .iter()
                .find(|(key, _)| key.as_str() == Some("reason"))
                .and_then(|(_, value)| value.as_str()),
            Some("too_many_pending")
        );
        let invalid = handler
            .handle(admitted(
                body("a-10", &["../probe-leak-zq.rs"], reason),
                &peer,
            ))
            .await;
        assert!(matches!(invalid, Reply::Code(RefusalCode::InvalidData)));

        let lines: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(mine)
            .skip(before)
            .collect();
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("refused: duplicate")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("refused: too_many_pending")),
            "{lines:?}"
        );
        // Admission and the handler each log a refusal, so count rules, not lines: the
        // invalid body is refused once, by the handler, before admission.
        assert!(
            lines.iter().any(|line| line.contains("refused: ")
                && !line.contains("duplicate")
                && !line.contains("too_many_pending")),
            "no refusal line for the invalid body: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("answered pending")),
            "{lines:?}"
        );
        for line in &lines {
            assert!(
                !line.contains("probe-leak"),
                "a path or the reason leaked: {line}"
            );
            assert!(!line.contains(reason), "the reason leaked: {line}");
        }
    }

    /// An access request whose id is the one of the same peer's pending QUESTION is a
    /// re-used id: the spec refuses a same-id re-request as `duplicate`, nothing is
    /// filed over the question, and the question stands.
    #[test]
    fn usage_probe_an_access_id_colliding_with_the_peers_pending_question_is_refused_as_duplicate()
    {
        let fixture = bare_slot("access-id-vs-question");
        store_of(&fixture.slot)
            .upsert(question_record("q-1"), SystemTime::now())
            .unwrap();

        let outcome =
            fixture
                .slot
                .admit_access(inbound(&identity(), "q-1", &["src/x.rs"], "same id"));

        assert_eq!(outcome, AccessOutcome::Refused(AccessRefusal::Duplicate));
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].id, "q-1");
        assert_eq!(records[0].kind, InboundKind::Question);
        assert!(records[0].paths.is_empty());
        assert!(fixture.idle.texts().is_empty());
        assert!(fixture.hooks.snapshot().is_empty());
        let fresh = fixture
            .slot
            .admit_access(inbound(&identity(), "a-1", &["src/x.rs"], ""));
        assert_eq!(fresh, AccessOutcome::Pending);

        // The same shape across peers: another identity re-using a pending access id is
        // a re-used id too, and it has no pending request of its own to be "too many".
        let other = hex_lower(&[0xcd; 16]);
        let collided = fixture
            .slot
            .admit_access(inbound(&other, "a-1", &["docs/z.md"], ""));
        assert_eq!(
            collided,
            AccessOutcome::Refused(AccessRefusal::Duplicate),
            "{collided:?}"
        );
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 2, "{records:?}");
        assert!(
            records
                .iter()
                .all(|record| record.peer_identity == identity())
        );
        assert_eq!(fixture.idle.texts().len(), 1);
    }

    /// Spec-first probe on the handler path: a colliding id — the same peer's pending
    /// QUESTION, or ANOTHER identity's pending access request — earns the wire word
    /// `duplicate`, a debug line naming that rule, and NO warn-level line: a re-used id
    /// is judged by the rate rule, never a filing failure the peer could provoke.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn usage_probe_a_colliding_id_on_the_handler_path_is_duplicate_at_debug_and_never_a_warn()
    {
        use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

        install_log_collector();
        let fixture = bare_slot("access-collide-log");
        let handler = AccessHandler::new(Arc::downgrade(&fixture.slot) as Weak<dyn AccessSurface>);
        let peer = TransportIdentity::new_from_rand(OsRng);
        let peer_identity = peer.as_identity().address_hash.to_hex_string();
        let id8 = short(&peer_identity).to_string();
        let mine = |line: &String| line.contains(&id8) && line.contains("/access");
        store_of(&fixture.slot)
            .upsert(
                InboundRecord {
                    peer_identity: peer_identity.clone(),
                    ..question_record("q-probe-same")
                },
                SystemTime::now(),
            )
            .unwrap();
        store_of(&fixture.slot)
            .upsert(
                InboundRecord {
                    id: "acc-probe-other".to_string(),
                    thread: "acc-probe-other".to_string(),
                    peer_identity: hex_lower(&[0x5c; 16]),
                    peer_destination: hex_lower(&[0x5d; 16]),
                    kind: InboundKind::Access,
                    question: String::new(),
                    paths: vec!["docs/probe-leak-theirs.md".to_string()],
                    reason: "probe-leak-theirs".to_string(),
                    ..question_record("acc-probe-other")
                },
                SystemTime::now(),
            )
            .unwrap();
        let debug_before = debug_snapshot().iter().filter(|line| mine(line)).count();
        let warn_before = warn_snapshot().iter().filter(|line| mine(line)).count();

        for colliding in ["q-probe-same", "acc-probe-other"] {
            let reply = handler
                .handle(admitted(
                    body(colliding, &["src/probe-leak-zq.rs"], "probe-leak-reason"),
                    &peer,
                ))
                .await;
            let Reply::Value(value) = &reply else {
                panic!("{colliding}: not a wire reply");
            };
            assert_eq!(
                value
                    .as_map()
                    .unwrap()
                    .iter()
                    .find(|(key, _)| key.as_str() == Some("reason"))
                    .and_then(|(_, value)| value.as_str()),
                Some("duplicate"),
                "{colliding}"
            );
        }

        let debug_lines: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(mine)
            .skip(debug_before)
            .collect();
        assert!(
            debug_lines
                .iter()
                .filter(|line| line.ends_with("refused: duplicate"))
                .count()
                >= 2,
            "each collision is logged under the rule the wire carries: {debug_lines:?}"
        );
        assert!(
            !debug_lines
                .iter()
                .any(|line| line.contains("was not filed")),
            "a collision is never a filing failure: {debug_lines:?}"
        );
        let warn_lines: Vec<String> = warn_snapshot()
            .into_iter()
            .filter(mine)
            .skip(warn_before)
            .collect();
        assert!(warn_lines.is_empty(), "peer-triggered warn: {warn_lines:?}");
        for line in debug_lines.iter().chain(warn_lines.iter()) {
            assert!(
                !line.contains("probe-leak"),
                "a path or reason leaked: {line}"
            );
        }
        let records = access_records(&fixture.slot);
        assert_eq!(records.len(), 2, "{records:?}");
        assert!(fixture.idle.texts().is_empty());
        assert!(fixture.hooks.snapshot().is_empty());
    }

    /// When the inbound store itself cannot be read the request is refused as
    /// `too_many_pending`, the I/O error is logged at debug with hashes redacted and
    /// never a path or the reason, and nothing is filed, announced, or fired.
    #[cfg(unix)]
    #[test]
    fn an_access_that_cannot_be_filed_is_refused_too_many_pending_at_debug_and_leaves_no_trace() {
        use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

        install_log_collector();
        let fixture = bare_slot("access-unfiled");
        let id8 = short(&identity()).to_string();
        let mine = |line: &String| line.contains(&id8) && line.contains("/access");
        let store_path = store_of(&fixture.slot).path().to_path_buf();
        fs::create_dir_all(&store_path).unwrap();
        let debug_before = debug_snapshot().iter().filter(|line| mine(line)).count();
        let warn_before = warn_snapshot().iter().filter(|line| mine(line)).count();

        let outcome = fixture.slot.admit_access(inbound(
            &identity(),
            "acc-io",
            &["src/probe-leak-io.rs"],
            "probe-leak-reason",
        ));
        assert_eq!(
            outcome,
            AccessOutcome::Refused(AccessRefusal::TooManyPending)
        );

        let debug_lines: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(mine)
            .skip(debug_before)
            .collect();
        let unfiled: Vec<&String> = debug_lines
            .iter()
            .filter(|line| line.contains("was not filed: "))
            .collect();
        assert_eq!(unfiled.len(), 1, "{debug_lines:?}");
        let (_, detail) = unfiled[0].split_once("was not filed: ").unwrap();
        assert!(!detail.trim().is_empty(), "{debug_lines:?}");
        assert!(
            !unfiled[0].contains("probe-leak"),
            "a path or reason leaked: {}",
            unfiled[0]
        );
        assert!(
            !unfiled[0].contains(&identity()),
            "the full identity leaked: {}",
            unfiled[0]
        );
        let warn_lines: Vec<String> = warn_snapshot()
            .into_iter()
            .filter(mine)
            .skip(warn_before)
            .collect();
        assert!(warn_lines.is_empty(), "I/O failure warned: {warn_lines:?}");

        fs::remove_dir(&store_path).unwrap();
        assert!(access_records(&fixture.slot).is_empty());
        assert!(fixture.idle.texts().is_empty());
        assert!(fixture.hooks.snapshot().is_empty());
    }
}
