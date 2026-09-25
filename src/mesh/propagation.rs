//! Outbound LXMF store-and-forward: handing one message to a propagation node so it can be
//! fetched later by a recipient that is offline now. This is the client side of the
//! reference `LXMRouter` propagation path and nothing more: no node discovery, no fetching,
//! no retry bookkeeping.
//!
//! Upstream `pack_propagation_transient_with_rng` encrypts to the recipient's long-term
//! identity key (`lxmf-core/src/message/wire.rs:335-358`), where the reference prefers the
//! latest announced ratchet (`LXMessage.py:427-428`). Python recipients still decrypt such
//! a message through their identity-key fallback, so this is a forward-secrecy gap, not an
//! interop one.

use crate::mesh::r3::{
    DEFAULT_LINK_TIMEOUT, Deadline, R3Error, RefusalCode, SizeBranch, open_link, short,
};

use lxmf_core::announce::{pn_stamp_cost_from_app_data, validate_pn_announce_data};
use lxmf_core::constants::APP_NAME;
use lxmf_core::identity::PrivateIdentity;
use lxmf_core::message::{Payload, WireMessage};
use lxmf_core::stamp::{
    PROPAGATION_STAMP_SIZE, generate_propagation_stamp_with_value_until_cancelled,
};
use rand_core::OsRng;
use rns_transport::PacketContext;
use rns_transport::destination::link::{Link, LinkEvent, LinkEventData, LinkId};
use rns_transport::destination::{DestinationDesc, DestinationName, SingleOutputDestination};
use rns_transport::hash::{AddressHash, Hash};
use rns_transport::identity::Identity;
use rns_transport::identity_bridge::{to_core_identity, to_transport_identity};
use rns_transport::resource::{ResourceEvent, ResourceEventKind};
use rns_transport::transport::{SendPacketOutcome, Transport};
use std::fmt;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;

/// The highest announced stamp cost this node will mine for. The reference clamps an
/// operator's configured cost only from below (`PROPAGATION_COST_MIN = 13`, default
/// `PROPAGATION_COST = 16`, `LXMRouter.py:50-54`; `lxmd.py:166-178`), so an announce may
/// demand anything, and each extra bit doubles the expected 2^cost hashes. The reference
/// client mines whatever `[5][0]` says with no ceiling (`LXMRouter.py:404-410`); the only
/// cost ceiling the reference applies anywhere is `MAX_PEERING_COST = 26`
/// (`LXMRouter.py:51`), which a node holds its peers' peering-key costs under
/// (`LXMRouter.py:1893-1898`). That figure is borrowed here as the propagation ceiling.
/// Costs below 13 are accepted as announced: a reference node cannot announce them, and
/// refusing them would buy nothing.
pub(crate) const MAX_ACCEPTED_STAMP_COST: u32 = 26;

/// Bounds the wait for the node to finish receiving an envelope sent as a resource; a
/// transfer still unacknowledged after this is reported as timed out. The reference scales
/// its waits with the link RTT; a fixed bound is simpler and enough for a LAN or a relay
/// hop, and `PropagationOptions` overrides it.
pub(crate) const PROPAGATION_TRANSFER_TIMEOUT: Duration = Duration::from_secs(60);

/// How long after a completed send the link is watched for the node's rejection signal
/// before the transfer is reported as accepted. The reference answers an invalid stamp on a
/// packet with `[ERROR_INVALID_STAMP]` and a teardown (`LXMRouter.py:2134-2136`), and a
/// stamp it accepts with a packet proof only, which upstream surfaces as no event at all.
/// Fixed rather than RTT-scaled for the same reason as `PROPAGATION_TRANSFER_TIMEOUT`.
pub(crate) const PROPAGATION_REJECT_WINDOW: Duration = Duration::from_secs(2);

/// Bounds the cancel sent for a resource the verdict watch gave up on; the transport is
/// asked once and not waited for beyond this.
const PROPAGATION_CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Aspect of the destination a propagation node serves clients on (`LXMRouter.py:173`) and
/// a client posts to (`LXMRouter.py:2722`).
const PROPAGATION_ASPECT: &str = "propagation";
/// Aspect of the destination a message is addressed to and fetched for (`LXMRouter.py:339,
/// 1432`).
const DELIVERY_ASPECT: &str = "delivery";
/// The reference's kilobyte, used wherever it compares a transfer against an announced
/// limit (`LXMRouter.py:1866, 2103`).
const LIMIT_KB_BYTES: u64 = 1000;
/// Names the exchange in the timeouts `Deadline` reports.
const PROPAGATION_PATH_LABEL: &str = "lxmf.propagation";

/// Why an announce could not be read as a propagation node worth posting to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PropagationNodeError {
    /// The announced destination is not `lxmf.propagation`.
    NotAPropagationNode,
    /// The app_data does not have the layout `LXMF.pn_announce_data_is_valid` demands
    /// (`LXMF.py:191-211`), or a validated slot holds a value out of range; the text says
    /// which.
    InvalidAnnounce(String),
    /// The announced target cost is below zero.
    NegativeStampCost(i64),
    /// The announced target cost is above `MAX_ACCEPTED_STAMP_COST`.
    StampCostAboveCeiling { cost: i64, max: u32 },
}

impl fmt::Display for PropagationNodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAPropagationNode => {
                write!(
                    f,
                    "The announced destination is not an LXMF propagation node"
                )
            }
            Self::InvalidAnnounce(reason) => {
                write!(f, "The propagation node announce is malformed: {reason}")
            }
            Self::NegativeStampCost(cost) => {
                write!(
                    f,
                    "The propagation node announces a negative stamp cost ({cost})"
                )
            }
            Self::StampCostAboveCeiling { cost, max } => write!(
                f,
                "The propagation node demands stamp cost {cost}, above the {max} this node will mine"
            ),
        }
    }
}

impl std::error::Error for PropagationNodeError {}

/// A propagation node as its announce described it: where to link, what it charges and
/// how much it takes per transfer. Not `Debug`: upstream `DestinationDesc` is not, and its
/// identity is what a log line should never carry in full anyway.
#[derive(Clone)]
pub(crate) struct PropagationNode {
    /// The `lxmf.propagation` destination the announce came from, identity included.
    pub destination: DestinationDesc,
    /// The stamp cost every posted message must meet, slot `[5][0]` of the announce
    /// (`LXMRouter.py:404-410`).
    pub stamp_cost: u32,
    /// The node's per-transfer limit in its kilobytes, slot `[3]` (`LXMRouter.py:314`).
    pub per_transfer_limit_kb: u64,
    /// Slot `[2]`: whether the node is taking messages from clients at all
    /// (`LXMRouter.py:309-313`).
    // Read by the caller that picks a node, which arrives with the outbound message path.
    #[allow(dead_code)]
    pub propagation_enabled: bool,
}

impl PropagationNode {
    /// Reads a node out of its announce. `app_data` must have the reference layout
    /// `[False, timebase, enabled, per_transfer_kb, per_sync_kb, [cost, flex, peering], {}]`
    /// (`LXMRouter.py:307-319`); the layout check is upstream's, the cost bounds are ours.
    // Reached by the node discovery that arrives with the outbound message path.
    #[allow(dead_code)]
    pub(crate) fn from_announce(
        destination: &DestinationDesc,
        app_data: &[u8],
    ) -> Result<Self, PropagationNodeError> {
        let expected = DestinationName::new(APP_NAME, PROPAGATION_ASPECT);
        if destination.name.as_name_hash_slice() != expected.as_name_hash_slice() {
            return Err(PropagationNodeError::NotAPropagationNode);
        }
        validate_pn_announce_data(app_data)
            .map_err(|err| PropagationNodeError::InvalidAnnounce(err.to_string()))?;
        // Unreachable once validation passed: it requires `[5]` to hold three integers.
        let cost = pn_stamp_cost_from_app_data(Some(app_data)).ok_or_else(|| {
            PropagationNodeError::InvalidAnnounce("stamp cost is not an integer".to_string())
        })?;
        if cost < 0 {
            return Err(PropagationNodeError::NegativeStampCost(cost));
        }
        if cost > i64::from(MAX_ACCEPTED_STAMP_COST) {
            return Err(PropagationNodeError::StampCostAboveCeiling {
                cost,
                max: MAX_ACCEPTED_STAMP_COST,
            });
        }
        let slots = match rmpv::decode::read_value(&mut Cursor::new(app_data)) {
            Ok(rmpv::Value::Array(slots)) => slots,
            _ => {
                return Err(PropagationNodeError::InvalidAnnounce(
                    "not a msgpack array".to_string(),
                ));
            }
        };
        let propagation_enabled = slots.get(2).and_then(rmpv::Value::as_bool).ok_or_else(|| {
            PropagationNodeError::InvalidAnnounce("node state is not a boolean".to_string())
        })?;
        let per_transfer_limit_kb =
            slots.get(3).and_then(rmpv::Value::as_u64).ok_or_else(|| {
                PropagationNodeError::InvalidAnnounce(
                    "per-transfer limit is not a non-negative integer".to_string(),
                )
            })?;
        Ok(Self {
            destination: *destination,
            stamp_cost: u32::try_from(cost).expect("a cost within the ceiling fits u32"),
            per_transfer_limit_kb,
            propagation_enabled,
        })
    }
}

/// Why a message could not be handed to a propagation node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PropagationError {
    /// The cancellation token fired while the stamp was being mined or the node awaited. A
    /// resource still in flight at that point is cancelled on a best-effort basis.
    Cancelled,
    /// The stamp search ran out of nonces, which no attainable cost does.
    StampExhausted,
    /// The envelope would exceed the node's announced per-transfer limit; nothing was mined
    /// or sent. The node itself refuses an incoming resource above its per-sync limit
    /// (`LXMRouter.py:2102-2106`), which is never below the per-transfer one
    /// (`LXMRouter.py:145-146`), so the announced limit is the conservative bound.
    Oversize { len: usize, max: usize },
    /// The message could not be signed, encrypted or packed.
    Encode(String),
    /// The link machinery failed: establishment, a send the transport refused, a bounded
    /// wait that expired (a resource still in flight is then cancelled on a best-effort
    /// basis), or a link that closed under the transfer.
    Link(R3Error),
    /// The resource transfer ended without completing.
    TransferFailed,
    /// The node closed the link after receiving the message. The reference does this for a
    /// resource whose stamp it rejects (`LXMRouter.py:2277-2278`), so retrying with the
    /// same stamp is pointless.
    ClosedAfterTransfer,
    /// The verdict watch fell `skipped` link events behind and the node's answer may have
    /// been among them; the message may or may not have been stored.
    VerdictLost { skipped: u64 },
    /// The node answered the transfer with one of its refusal sentinels
    /// (`LXMPeer.ERROR_*`, `LXMPeer.py:24-31`).
    RejectedByNode(RefusalCode),
}

impl fmt::Display for PropagationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "The propagation attempt was cancelled"),
            Self::StampExhausted => write!(f, "No propagation stamp could be found"),
            Self::Oversize { len, max } => write!(
                f,
                "The propagation envelope is {len} bytes, above the node's {max}-byte limit"
            ),
            Self::Encode(reason) => write!(f, "The message could not be packed: {reason}"),
            Self::Link(R3Error::Timeout { after, .. }) => write!(
                f,
                "The propagation node did not finish the transfer within {:.1}s",
                after.as_secs_f64()
            ),
            Self::Link(err) => write!(f, "{err}"),
            Self::TransferFailed => {
                write!(f, "The transfer to the propagation node did not complete")
            }
            Self::ClosedAfterTransfer => write!(
                f,
                "The propagation node closed the link after receiving the message"
            ),
            Self::VerdictLost { skipped } => write!(
                f,
                "The propagation node's answer was lost among {skipped} skipped link events"
            ),
            Self::RejectedByNode(code) => {
                write!(f, "The propagation node rejected the message: {code}")
            }
        }
    }
}

impl std::error::Error for PropagationError {}

impl From<R3Error> for PropagationError {
    fn from(err: R3Error) -> Self {
        Self::Link(err)
    }
}

impl From<lxmf_core::LxmfError> for PropagationError {
    fn from(err: lxmf_core::LxmfError) -> Self {
        Self::Encode(err.to_string())
    }
}

/// What a caller wants delivered; the timestamp and the addressing are filled in here.
pub(crate) struct OutboundMessage {
    pub title: Option<Vec<u8>>,
    pub content: Vec<u8>,
    pub fields: Option<rmpv::Value>,
}

/// A stamp that reached its target cost, and the value it reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MinedStamp {
    pub stamp: Vec<u8>,
    pub value: u32,
}

/// Timeouts for one propagation attempt.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PropagationOptions {
    pub link_timeout: Duration,
    pub transfer_timeout: Duration,
    pub reject_window: Duration,
}

impl Default for PropagationOptions {
    fn default() -> Self {
        Self {
            link_timeout: DEFAULT_LINK_TIMEOUT,
            transfer_timeout: PROPAGATION_TRANSFER_TIMEOUT,
            reject_window: PROPAGATION_REJECT_WINDOW,
        }
    }
}

/// How a message reached the node. Acceptance is inferred, not proven: the reference
/// answers an accepted packet with a per-packet proof, which upstream turns into no event
/// on an active link (`link_sections/handle_proof_packet.rs:4-16`), so a packet that drew
/// no rejection inside the watch window counts as delivered. A resource is at least
/// confirmed received, by its `OutboundComplete` event.
// The fields wait for a production reader; the tests assert on them.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PropagationOutcome {
    pub representation: SizeBranch,
    /// `sha256` of the encrypted message before the stamp, the id the node stores it under.
    pub transient_id: [u8; 32],
    pub stamp_value: u32,
    pub envelope_len: usize,
}

/// The envelope for one message, mined and ready to send.
pub(crate) struct PreparedEnvelope {
    pub envelope: Vec<u8>,
    pub transient_id: [u8; 32],
    pub stamp_value: u32,
}

/// The `lxmf.delivery` destination hash of `identity`: what a message names as its source
/// and destination, and what the node later serves a fetch for (`LXMRouter.py:339, 1432`).
pub(crate) fn lxmf_delivery_hash(identity: &Identity) -> AddressHash {
    SingleOutputDestination::new(*identity, DestinationName::new(APP_NAME, DELIVERY_ASPECT))
        .desc
        .address_hash
}

/// A signed message from `sender` to the recipient whose delivery hash is
/// `recipient_delivery`, laid out as `LXMessage.pack` does (`LXMessage.py:359-383`). A
/// missing title goes as empty bytes and missing fields as an empty map, which is what the
/// reference constructor substitutes (`LXMessage.py:129, 213-214`); Python consumers call
/// `title.decode` on what arrives (`LXMessage.py:196-197`), so a msgpack nil would crash
/// them. Fields other than a map are refused for the same reason: reference readers treat
/// the slot as a dict (`LXMessage.py:213-214`).
pub(crate) fn build_signed_message(
    sender: &PrivateIdentity,
    recipient_delivery: &AddressHash,
    message: &OutboundMessage,
    timestamp: f64,
) -> Result<WireMessage, PropagationError> {
    let source = lxmf_delivery_hash(&to_transport_identity(sender.as_identity()));
    if matches!(&message.fields, Some(fields) if !matches!(fields, rmpv::Value::Map(_))) {
        return Err(PropagationError::Encode(
            "message fields must be a msgpack map".to_string(),
        ));
    }
    let payload = Payload::new(
        timestamp,
        Some(message.content.clone()),
        Some(message.title.clone().unwrap_or_default()),
        Some(
            message
                .fields
                .clone()
                .unwrap_or_else(|| rmpv::Value::Map(Vec::new())),
        ),
        None,
    );
    let mut wire = WireMessage::new(
        address_bytes(recipient_delivery),
        address_bytes(&source),
        payload,
    );
    wire.sign(sender)?;
    Ok(wire)
}

fn address_bytes(hash: &AddressHash) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(hash.as_slice());
    bytes
}

/// Which way an envelope of `len` bytes travels over a link whose MDU is `mdu`: the same
/// test RNS `Link.request` makes. The reference client uses the stricter
/// `LINK_PACKET_MAX_CONTENT = MDU - LXMF_OVERHEAD` (`LXMessage.py:89, 435-441`); the node
/// accepts both representations (`LXMRouter.py:2110-2136, 2194-2290`), so the physical
/// bound is the one that matters.
pub(crate) fn representation_for(len: usize, mdu: usize) -> SizeBranch {
    if len <= mdu {
        SizeBranch::Packet
    } else {
        SizeBranch::Resource
    }
}

/// Searches for a stamp over `transient_id` reaching `cost` on a blocking thread, giving up
/// when `cancel` fires. The search polls the token every 1024 nonces
/// (`lxmf-core/src/stamp/mod.rs:95-124`). Dropping the future also stops the search: the
/// blocking thread watches a child token that a guard cancels on drop, since a blocking
/// task outlives the future that spawned it otherwise.
pub(crate) async fn mine_propagation_stamp(
    transient_id: [u8; 32],
    cost: u32,
    cancel: CancellationToken,
) -> Result<MinedStamp, PropagationError> {
    let scoped = cancel.child_token();
    let _guard = scoped.clone().drop_guard();
    let mined = tokio::task::spawn_blocking(move || {
        generate_propagation_stamp_with_value_until_cancelled(&transient_id, cost, || {
            scoped.is_cancelled()
        })
    })
    .await
    .map_err(|err| {
        if err.is_cancelled() {
            PropagationError::Cancelled
        } else {
            PropagationError::Encode(format!("stamp mining task panicked: {err}"))
        }
    })?;
    match mined {
        Some((stamp, value)) => Ok(MinedStamp { stamp, value }),
        None if cancel.is_cancelled() => Err(PropagationError::Cancelled),
        None => Err(PropagationError::StampExhausted),
    }
}

/// Signs, encrypts and stamps `message` for `node`, without touching the network. The
/// order is fixed by the wire format: the transient id is the hash of the encrypted message
/// (`LXMessage.py:429-433`), the stamp is mined over that id, and the envelope carries both
/// (`LXMRouter.py:2110-2120`). Encrypting again would mint a fresh ephemeral key and a
/// different id, so the encrypted bytes are made once and reused. The size check runs
/// before any mining so an envelope the node would refuse costs no work.
pub(crate) async fn prepare_envelope(
    sender: &PrivateIdentity,
    recipient: &Identity,
    node: &PropagationNode,
    message: &OutboundMessage,
    cancel: CancellationToken,
) -> Result<PreparedEnvelope, PropagationError> {
    let timestamp = unix_now()?;
    let wire = build_signed_message(sender, &lxmf_delivery_hash(recipient), message, timestamp)?;
    let (lxmf_data, transient_id) =
        wire.pack_propagation_transient_with_rng(&to_core_identity(recipient), OsRng)?;
    let placeholder = [0u8; PROPAGATION_STAMP_SIZE];
    let len =
        WireMessage::pack_propagation_envelope(timestamp, &lxmf_data, Some(&placeholder))?.len();
    let max = usize::try_from(node.per_transfer_limit_kb.saturating_mul(LIMIT_KB_BYTES))
        .unwrap_or(usize::MAX);
    if len > max {
        return Err(PropagationError::Oversize { len, max });
    }
    let node_hex = node.destination.address_hash.to_hex_string();
    debug!(
        "Mesh propagation node {} demands stamp cost {}",
        short(&node_hex),
        node.stamp_cost
    );
    let mined = mine_propagation_stamp(transient_id, node.stamp_cost, cancel).await?;
    debug!(
        "Mesh propagation stamp of value {} mined for transient {}",
        mined.value,
        short(&crate::mesh::hex_lower(&transient_id))
    );
    let envelope =
        WireMessage::pack_propagation_envelope(timestamp, &lxmf_data, Some(&mined.stamp))?;
    Ok(PreparedEnvelope {
        envelope,
        transient_id,
        stamp_value: mined.value,
    })
}

/// Hands one message to `node` for later delivery to `recipient`. The link is opened (or
/// reused) without identifying on it, as the reference client does when posting
/// (`LXMRouter.py:2714-2725`; identify is for fetching, `:495`), the envelope goes as a
/// link packet when it fits the link MDU and as a raw resource otherwise, and the link is
/// then watched for the node's verdict. The link is left up afterwards: the transport
/// reuses it for the next message, as the reference does under `P_LINK_MAX_INACTIVITY`.
///
/// Calls for the same node must be serialised by the caller: the refusal sentinel and the
/// teardown carry no message id and the out-link is shared, so two concurrent posts could
/// not tell whose verdict arrived (the reference holds one `for_lxmessage` per link,
/// `LXMRouter.py:2725`). `propagation_enabled` is deliberately not checked here; the node
/// picker owns that decision.
// Reached by the outbound message path once it lands.
#[allow(dead_code)]
pub(crate) async fn propagate(
    transport: &Transport,
    sender: &PrivateIdentity,
    recipient: &Identity,
    node: &PropagationNode,
    message: &OutboundMessage,
    cancel: CancellationToken,
    options: &PropagationOptions,
) -> Result<PropagationOutcome, PropagationError> {
    let prepared = prepare_envelope(sender, recipient, node, message, cancel.clone()).await?;
    send_envelope(transport, node, &prepared, cancel, options).await
}

/// Puts a prepared envelope in front of `node` and waits for its verdict. Both event
/// streams are subscribed before the link is touched so no verdict can slip past. The
/// envelope is borrowed so a retry can resend the same transient id and stamp, as the
/// reference does by caching its encrypted form across attempts (`LXMessage.py:426-428`).
pub(crate) async fn send_envelope(
    transport: &Transport,
    node: &PropagationNode,
    prepared: &PreparedEnvelope,
    cancel: CancellationToken,
    options: &PropagationOptions,
) -> Result<PropagationOutcome, PropagationError> {
    let link_events = transport.out_link_events();
    let resource_events = transport.resource_events();
    let link = open_link(
        transport,
        &node.destination,
        PROPAGATION_PATH_LABEL,
        Deadline::after(options.link_timeout),
    )
    .await?;
    let deadline = Deadline::after(options.transfer_timeout);
    let (link_id, mdu) = deadline
        .bound(PROPAGATION_PATH_LABEL, async {
            let link = link.lock().await;
            (*link.id(), link.link_mdu())
        })
        .await?;
    let envelope_len = prepared.envelope.len();
    let representation = representation_for(envelope_len, mdu);
    let node_hex = node.destination.address_hash.to_hex_string();
    debug!(
        "Sending mesh transient {} to propagation node {} as a {representation:?} ({envelope_len} bytes, link MDU {mdu})",
        short(&crate::mesh::hex_lower(&prepared.transient_id)),
        short(&node_hex)
    );
    let pending = match representation {
        SizeBranch::Packet => {
            send_packet(transport, &link, &prepared.envelope, deadline).await?;
            None
        }
        SizeBranch::Resource => Some(
            deadline
                .bound(
                    PROPAGATION_PATH_LABEL,
                    transport.send_resource(&link_id, prepared.envelope.clone(), None),
                )
                .await?
                .map_err(|err| R3Error::Send(format!("propagation resource: {err}")))?,
        ),
    };
    await_verdict(
        transport,
        VerdictStreams {
            link_events,
            resource_events,
        },
        link_id,
        pending,
        deadline,
        options.reject_window,
        cancel,
    )
    .await?;
    Ok(PropagationOutcome {
        representation,
        transient_id: prepared.transient_id,
        stamp_value: prepared.stamp_value,
        envelope_len,
    })
}

async fn send_packet(
    transport: &Transport,
    link: &Arc<tokio::sync::Mutex<Link>>,
    envelope: &[u8],
    deadline: Deadline,
) -> Result<(), PropagationError> {
    let packet = deadline
        .bound(PROPAGATION_PATH_LABEL, link.lock())
        .await?
        .data_packet(envelope)
        .map_err(|err| R3Error::Send(format!("propagation packet: {err}")))?;
    match deadline
        .bound(
            PROPAGATION_PATH_LABEL,
            transport.send_link_packet_on_bound_iface(link, packet),
        )
        .await?
    {
        SendPacketOutcome::SentDirect => Ok(()),
        outcome => {
            Err(R3Error::LinkFailed(format!("propagation packet not sent: {outcome:?}")).into())
        }
    }
}

/// The two transport event streams a verdict is read from, subscribed before the send.
struct VerdictStreams {
    link_events: broadcast::Receiver<LinkEventData>,
    resource_events: broadcast::Receiver<ResourceEvent>,
}

/// Watches the link after a send and, when the watch gives up on a resource still in
/// flight, asks the transport to cancel it so the node cannot go on to store a message the
/// caller was told failed. The cancel is best-effort: bounded by
/// `PROPAGATION_CANCEL_GRACE`, its outcome only logged.
async fn await_verdict(
    transport: &Transport,
    mut streams: VerdictStreams,
    link_id: LinkId,
    mut pending: Option<Hash>,
    deadline: Deadline,
    reject_window: Duration,
    cancel: CancellationToken,
) -> Result<(), PropagationError> {
    let verdict = watch_verdict(
        &mut streams.link_events,
        &mut streams.resource_events,
        link_id,
        &mut pending,
        deadline,
        reject_window,
        &cancel,
    )
    .await;
    if let (
        Err(PropagationError::Cancelled | PropagationError::Link(R3Error::Timeout { .. })),
        Some(hash),
    ) = (&verdict, pending)
    {
        let outcome = match timeout(
            PROPAGATION_CANCEL_GRACE,
            transport.cancel_resource(&link_id, hash),
        )
        .await
        {
            Ok(Ok(true)) => "cancel sent".to_string(),
            Ok(Ok(false)) => "nothing left to cancel".to_string(),
            Ok(Err(err)) => format!("cancel failed: {err}"),
            Err(_) => "cancel timed out".to_string(),
        };
        debug!(
            "Mesh propagation resource {} abandoned on link {}: {outcome}",
            short(&crate::mesh::hex_lower(hash.as_slice())),
            link_id.to_hex_string()
        );
    }
    verdict
}

/// The verdict watch. While `pending` names a resource in flight, the wait is bounded by
/// `deadline` and ends in `TransferFailed` or, once the transport reports
/// `OutboundComplete`, clears `pending` and moves on to the rejection watch. That watch
/// lasts `reject_window`: a refusal sentinel on the link rejects the message, the link
/// closing is `ClosedAfterTransfer`, and silence accepts it. The reference tears the link
/// down after a rejected resource without sending a sentinel (`LXMRouter.py:2277-2278`),
/// so the close is the verdict there. Takes no transport so it can be driven from
/// synthetic channels.
async fn watch_verdict(
    link_events: &mut broadcast::Receiver<LinkEventData>,
    resource_events: &mut broadcast::Receiver<ResourceEvent>,
    link_id: LinkId,
    pending: &mut Option<Hash>,
    deadline: Deadline,
    reject_window: Duration,
    cancel: &CancellationToken,
) -> Result<(), PropagationError> {
    let mut until = if pending.is_some() {
        Instant::now() + deadline.remaining()
    } else {
        Instant::now() + reject_window
    };
    loop {
        tokio::select! {
            // A buffered verdict must beat the window expiring, and a buffered
            // `OutboundComplete` must be seen before the close that may follow it.
            biased;
            () = cancel.cancelled() => return Err(PropagationError::Cancelled),
            event = resource_events.recv(), if pending.is_some() => match event {
                Ok(event) if Some(event.hash) == *pending => match event.kind {
                    ResourceEventKind::OutboundComplete => {
                        debug!(
                            "Mesh propagation resource {} was received on link {}",
                            short(&crate::mesh::hex_lower(event.hash.as_slice())),
                            link_id.to_hex_string()
                        );
                        *pending = None;
                        until = Instant::now() + reject_window;
                    }
                    ResourceEventKind::OutboundFailed | ResourceEventKind::OutboundCancelled => {
                        return Err(PropagationError::TransferFailed);
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!("Mesh propagation watch fell behind and skipped {skipped} resource events");
                }
                Err(broadcast::error::RecvError::Closed) => return Err(R3Error::Shutdown.into()),
            },
            event = link_events.recv() => match event {
                Ok(event) if event.id == link_id => match event.event {
                    LinkEvent::Data(payload) if payload.context() == PacketContext::None => {
                        if let Some(code) = refusal_in(payload.as_slice()) {
                            debug!(
                                "Mesh link {} carried a propagation refusal: {code}",
                                link_id.to_hex_string()
                            );
                            return Err(PropagationError::RejectedByNode(code));
                        }
                    }
                    LinkEvent::Closed if pending.is_none() => {
                        return Err(PropagationError::ClosedAfterTransfer);
                    }
                    LinkEvent::Closed => return Err(R3Error::LinkClosed.into()),
                    LinkEvent::Activated | LinkEvent::Data(_) | LinkEvent::PeerIdentified(_) => {}
                },
                Ok(_) => {}
                // While a resource is in flight its outcome is re-derivable from later
                // events; inside the rejection watch a skipped refusal is gone for good.
                Err(broadcast::error::RecvError::Lagged(skipped)) if pending.is_some() => {
                    debug!("Mesh propagation watch fell behind and skipped {skipped} link events");
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    return Err(PropagationError::VerdictLost { skipped });
                }
                Err(broadcast::error::RecvError::Closed) => return Err(R3Error::Shutdown.into()),
            },
            () = sleep_until(until) => {
                return if pending.is_some() {
                    Err(deadline.expired(PROPAGATION_PATH_LABEL).into())
                } else {
                    Ok(())
                };
            }
        }
    }
}

/// The refusal sentinel in a signalling packet, `msgpack([code])`
/// (`LXMRouter.py:2501-2511`); anything else on the link is not a verdict.
fn refusal_in(bytes: &[u8]) -> Option<RefusalCode> {
    match rmpv::decode::read_value(&mut Cursor::new(bytes)).ok()? {
        rmpv::Value::Array(items) => RefusalCode::from_wire(items.first()?),
        _ => None,
    }
}

fn unix_now() -> Result<f64, PropagationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .map_err(|_| PropagationError::Encode("system clock is before the UNIX epoch".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_support::rust_sources;
    use crate::testing::{debug_snapshot, install_log_collector};

    use lxmf_core::stamp::validate_propagation_stamp;
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant as StdInstant;
    use tokio::time::{MissedTickBehavior, interval, sleep, timeout};

    /// A cost every mine reaches within a handful of nonces.
    const CHEAP_COST: u32 = 1;
    /// A cost no mine reaches inside a test's lifetime: 2^60 expected hashes.
    const ENDLESS_COST: u32 = 60;
    /// A cost a debug build mines in roughly half a second (measured at 0.1-0.8 s), so a
    /// run of them fills the window the not-blocked test measures over.
    const MEASURED_COST: u32 = 16;
    /// The reference's default per-transfer limit, `PROPAGATION_LIMIT` (`LXMRouter.py:55`).
    const DEFAULT_LIMIT_KB: i64 = 256;

    fn app_data_from(slots: Vec<rmpv::Value>) -> Vec<u8> {
        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, &rmpv::Value::Array(slots)).unwrap();
        out
    }

    /// `LXMRouter.get_propagation_node_app_data` (`LXMRouter.py:307-319`), slot for slot.
    fn pn_slots(cost: i64, per_transfer_kb: i64) -> Vec<rmpv::Value> {
        vec![
            rmpv::Value::Boolean(false),
            rmpv::Value::from(1_700_000_000i64),
            rmpv::Value::Boolean(true),
            rmpv::Value::from(per_transfer_kb),
            rmpv::Value::from(per_transfer_kb * 40),
            rmpv::Value::Array(vec![
                rmpv::Value::from(cost),
                rmpv::Value::from(3),
                rmpv::Value::from(18),
            ]),
            rmpv::Value::Map(vec![]),
        ]
    }

    fn pn_app_data(cost: i64, per_transfer_kb: i64) -> Vec<u8> {
        app_data_from(pn_slots(cost, per_transfer_kb))
    }

    fn desc_for(identity: &TransportIdentity, aspects: &str) -> DestinationDesc {
        SingleOutputDestination::new(
            *identity.as_identity(),
            DestinationName::new("lxmf", aspects),
        )
        .desc
    }

    fn propagation_desc() -> DestinationDesc {
        desc_for(&TransportIdentity::new_from_rand(OsRng), PROPAGATION_ASPECT)
    }

    fn node_with(cost: u32, per_transfer_limit_kb: u64) -> PropagationNode {
        PropagationNode {
            destination: propagation_desc(),
            stamp_cost: cost,
            per_transfer_limit_kb,
            propagation_enabled: true,
        }
    }

    fn parse_error(destination: &DestinationDesc, app_data: &[u8]) -> PropagationNodeError {
        match PropagationNode::from_announce(destination, app_data) {
            Ok(node) => panic!(
                "expected a parse error, got a node charging {}",
                node.stamp_cost
            ),
            Err(err) => err,
        }
    }

    fn message(content: &[u8]) -> OutboundMessage {
        OutboundMessage {
            title: Some(b"hello".to_vec()),
            content: content.to_vec(),
            fields: None,
        }
    }

    fn recipient_of(identity: &PrivateIdentity) -> Identity {
        to_transport_identity(identity.as_identity())
    }

    /// Node hash prefix as the demand log names it.
    fn logged_node(node: &PropagationNode) -> String {
        format!(
            "Mesh propagation node {} demands stamp cost",
            short(&node.destination.address_hash.to_hex_string())
        )
    }

    #[test]
    fn signed_message_verifies_for_the_sender_only_and_names_delivery_hashes() {
        let sender = PrivateIdentity::new_from_rand(OsRng);
        let recipient = PrivateIdentity::new_from_rand(OsRng);
        let stranger = PrivateIdentity::new_from_rand(OsRng);
        let recipient_delivery = lxmf_delivery_hash(&recipient_of(&recipient));

        let wire = build_signed_message(
            &sender,
            &recipient_delivery,
            &message(b"content"),
            1_700_000_000.5,
        )
        .unwrap();
        let unpacked = WireMessage::unpack(&wire.pack().unwrap()).unwrap();
        assert_eq!(unpacked.verify(sender.as_identity()), Ok(true));
        assert_eq!(unpacked.verify(stranger.as_identity()), Ok(false));

        let expected_destination = SingleOutputDestination::new(
            recipient_of(&recipient),
            DestinationName::new("lxmf", "delivery"),
        )
        .desc
        .address_hash;
        let expected_source = SingleOutputDestination::new(
            recipient_of(&sender),
            DestinationName::new("lxmf", "delivery"),
        )
        .desc
        .address_hash;
        assert_eq!(&unpacked.destination[..], expected_destination.as_slice());
        assert_eq!(&unpacked.source[..], expected_source.as_slice());
        assert_ne!(
            &unpacked.destination[..],
            recipient.address_hash().as_slice()
        );
        assert_eq!(
            unpacked.payload.content.as_deref().map(Vec::as_slice),
            Some(&b"content"[..])
        );
        assert_eq!(
            unpacked.payload.title.as_deref().map(Vec::as_slice),
            Some(&b"hello"[..])
        );
    }

    /// `LXMessage.pack` sends `[timestamp, b"", content, {}]` for a message built without
    /// a title or fields, and its readers `decode` the title, so nil is not an option.
    #[test]
    fn a_missing_title_and_fields_go_as_empty_bytes_and_an_empty_map() {
        let sender = PrivateIdentity::new_from_rand(OsRng);
        let recipient = PrivateIdentity::new_from_rand(OsRng);
        let bare = OutboundMessage {
            title: None,
            content: b"hi".to_vec(),
            fields: None,
        };

        let wire = build_signed_message(
            &sender,
            &lxmf_delivery_hash(&recipient_of(&recipient)),
            &bare,
            1_700_000_000.0,
        )
        .unwrap();
        let unpacked = WireMessage::unpack(&wire.pack().unwrap()).unwrap();
        assert_eq!(
            unpacked.payload.title.as_deref().map(Vec::as_slice),
            Some(&[][..])
        );
        assert_eq!(unpacked.payload.fields, Some(rmpv::Value::Map(vec![])));

        // fixarray(4), float64 timestamp, bin8 of 0 bytes, bin8 "hi", fixmap(0).
        let mut expected = vec![0x94, 0xcb];
        expected.extend_from_slice(&1_700_000_000.0f64.to_be_bytes());
        expected.extend_from_slice(&[0xc4, 0x00, 0xc4, 0x02, b'h', b'i', 0x80]);
        assert_eq!(
            unpacked.payload.to_msgpack_without_stamp().unwrap(),
            expected
        );
    }

    #[test]
    fn fields_that_are_not_a_map_are_refused_before_signing() {
        let sender = PrivateIdentity::new_from_rand(OsRng);
        let recipient = PrivateIdentity::new_from_rand(OsRng);
        let odd = OutboundMessage {
            title: None,
            content: b"hi".to_vec(),
            fields: Some(rmpv::Value::Integer(7.into())),
        };
        let outcome = build_signed_message(
            &sender,
            &lxmf_delivery_hash(&recipient_of(&recipient)),
            &odd,
            1_700_000_000.0,
        );
        assert!(matches!(outcome, Err(PropagationError::Encode(_))));
    }

    #[test]
    fn from_announce_reads_the_reference_layout() {
        let desc = propagation_desc();
        let node = PropagationNode::from_announce(&desc, &pn_app_data(20, 128)).unwrap();
        assert_eq!(node.stamp_cost, 20);
        assert_eq!(node.per_transfer_limit_kb, 128);
        assert!(node.propagation_enabled);
        assert_eq!(node.destination.address_hash, desc.address_hash);

        let mut disabled = pn_slots(20, 128);
        disabled[2] = rmpv::Value::Boolean(false);
        let node = PropagationNode::from_announce(&desc, &app_data_from(disabled)).unwrap();
        assert!(!node.propagation_enabled);
    }

    #[tokio::test]
    async fn a_stamp_mined_at_the_announced_cost_validates_at_that_cost() {
        let node =
            PropagationNode::from_announce(&propagation_desc(), &pn_app_data(20, 128)).unwrap();
        assert_eq!(node.stamp_cost, 20);
        // A payload whose cost-20 stamp lies at nonce 567 (value 21), found by scanning
        // constant payloads offline, so the mine here is deterministic and quick; random
        // ids are mined in `mining_does_not_starve_the_runtime`. It has to be longer than
        // the validator's minimum message length.
        let lxmf_data = [10u8; 128];
        let transient_id: [u8; 32] = Sha256::digest(lxmf_data).into();

        let mined = mine_propagation_stamp(transient_id, node.stamp_cost, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(mined.stamp.len(), PROPAGATION_STAMP_SIZE);
        assert!(mined.value >= 20, "value {}", mined.value);

        let mut transient = lxmf_data.to_vec();
        transient.extend_from_slice(&mined.stamp);
        let value = validate_propagation_stamp(&transient, 20).unwrap();
        assert_eq!(value, mined.value);
        assert!(value >= 20);
        // This fixture overshoots by one bit, so it validates at 21 too and fails at 22.
        assert_eq!(validate_propagation_stamp(&transient, 21), Some(value));
        assert_eq!(validate_propagation_stamp(&transient, 22), None);
    }

    #[test]
    fn from_announce_refuses_costs_outside_the_accepted_range() {
        let desc = propagation_desc();
        assert_eq!(
            parse_error(&desc, &pn_app_data(-1, DEFAULT_LIMIT_KB)),
            PropagationNodeError::NegativeStampCost(-1)
        );
        let above = parse_error(&desc, &pn_app_data(27, DEFAULT_LIMIT_KB));
        assert_eq!(
            above,
            PropagationNodeError::StampCostAboveCeiling { cost: 27, max: 26 }
        );
        assert!(above.to_string().contains("26"), "{above}");
        let at_ceiling =
            PropagationNode::from_announce(&desc, &pn_app_data(26, DEFAULT_LIMIT_KB)).unwrap();
        assert_eq!(at_ceiling.stamp_cost, MAX_ACCEPTED_STAMP_COST);
        let below_reference_minimum =
            PropagationNode::from_announce(&desc, &pn_app_data(0, DEFAULT_LIMIT_KB)).unwrap();
        assert_eq!(below_reference_minimum.stamp_cost, 0);
    }

    #[test]
    fn from_announce_refuses_malformed_app_data() {
        let desc = propagation_desc();
        let malformed = |app_data: &[u8]| match parse_error(&desc, app_data) {
            PropagationNodeError::InvalidAnnounce(reason) => reason,
            other => panic!("expected a malformed announce, got {other:?}"),
        };

        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, &rmpv::Value::from(7)).unwrap();
        assert!(!malformed(&out).is_empty());

        let short_array = app_data_from(vec![rmpv::Value::Boolean(false); 5]);
        assert!(!malformed(&short_array).is_empty());

        let mut no_costs = pn_slots(20, DEFAULT_LIMIT_KB);
        no_costs[5] = rmpv::Value::Nil;
        assert!(!malformed(&app_data_from(no_costs)).is_empty());
        let mut short_costs = pn_slots(20, DEFAULT_LIMIT_KB);
        short_costs[5] = rmpv::Value::Array(vec![rmpv::Value::from(20)]);
        assert!(!malformed(&app_data_from(short_costs)).is_empty());

        assert!(!malformed(b"\xc1not msgpack").is_empty());
        assert!(!malformed(&pn_app_data(20, -5)).is_empty());
    }

    #[test]
    fn from_announce_refuses_a_destination_that_is_not_a_propagation_node() {
        let identity = TransportIdentity::new_from_rand(OsRng);
        let other = SingleOutputDestination::new(
            *identity.as_identity(),
            DestinationName::new("coyote", "mesh.x"),
        )
        .desc;
        assert_eq!(
            parse_error(&other, &pn_app_data(20, DEFAULT_LIMIT_KB)),
            PropagationNodeError::NotAPropagationNode
        );
        let delivery = desc_for(&identity, DELIVERY_ASPECT);
        assert_eq!(
            parse_error(&delivery, &pn_app_data(20, DEFAULT_LIMIT_KB)),
            PropagationNodeError::NotAPropagationNode
        );
    }

    #[test]
    fn mesh_module_never_names_the_default_stamp_cost() {
        // Assembled at runtime so this test's own text does not match the probe.
        let needle = ["DEFAULT_PROPAGATION", "_STAMP_COST"].concat();
        let sources = rust_sources();
        for path in &sources {
            let source = fs::read_to_string(path).unwrap();
            assert!(
                !source.contains(&needle),
                "{} must not reference {needle}",
                path.display()
            );
        }
        assert!(
            sources.iter().any(|path| path.ends_with("propagation.rs")),
            "the scan must cover this module"
        );
    }

    #[test]
    fn representation_is_a_packet_up_to_the_mdu_and_a_resource_above() {
        let mdu = 431;
        assert_eq!(representation_for(mdu - 1, mdu), SizeBranch::Packet);
        assert_eq!(representation_for(mdu, mdu), SizeBranch::Packet);
        assert_eq!(representation_for(mdu + 1, mdu), SizeBranch::Resource);
    }

    #[test]
    fn refusal_is_read_from_signalling_packets_only() {
        let mut rejected = Vec::new();
        rmpv::encode::write_value(
            &mut rejected,
            &rmpv::Value::Array(vec![RefusalCode::InvalidStamp.to_wire()]),
        )
        .unwrap();
        assert_eq!(rejected, [0x91, 0xcc, 0xf5]);
        assert_eq!(refusal_in(&rejected), Some(RefusalCode::InvalidStamp));

        let bare_code = RefusalCode::InvalidStamp.to_wire();
        let mut bare = Vec::new();
        rmpv::encode::write_value(&mut bare, &bare_code).unwrap();
        assert_eq!(refusal_in(&bare), None);
        let mut other = Vec::new();
        rmpv::encode::write_value(&mut other, &rmpv::Value::Array(vec![rmpv::Value::from(7)]))
            .unwrap();
        assert_eq!(refusal_in(&other), None);
        assert_eq!(refusal_in(&[]), None);
        assert_eq!(refusal_in(b"\xc1"), None);
    }

    /// Under `current_thread` a mine run inline would starve the ticker; `Skip` keeps a
    /// starved ticker from making its count up in a burst afterwards. Together they make
    /// the tick count tell an offloaded mine from an inline one.
    #[tokio::test(flavor = "current_thread")]
    async fn mining_does_not_starve_the_runtime() {
        const TICK: Duration = Duration::from_millis(10);
        const MINIMUM_RUN: Duration = Duration::from_millis(300);
        let ticks = Arc::new(AtomicUsize::new(0));
        let counter = ticks.clone();
        let ticker = tokio::spawn(async move {
            let mut ticker = interval(TICK);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        let started = StdInstant::now();
        let mut mined = 0u8;
        while started.elapsed() < MINIMUM_RUN {
            let transient_id: [u8; 32] = Sha256::digest([mined; 64]).into();
            let stamp =
                mine_propagation_stamp(transient_id, MEASURED_COST, CancellationToken::new())
                    .await
                    .unwrap();
            assert!(stamp.value >= MEASURED_COST);
            mined += 1;
        }
        let elapsed = started.elapsed();
        let counted = ticks.load(Ordering::SeqCst);
        ticker.abort();

        let expected = usize::try_from(elapsed.as_millis() / TICK.as_millis()).unwrap();
        assert!(
            counted >= expected / 2,
            "the ticker ran {counted} times in {elapsed:?} across {mined} mines; expected at least {}",
            expected / 2
        );
    }

    /// Under `current_thread` an inline mine would block the `sleep` and the cancel for
    /// good, so this doubles as a deterministic proof that the search is offloaded.
    #[tokio::test(flavor = "current_thread")]
    async fn mining_stops_with_cancelled_when_the_token_fires() {
        let cancel = CancellationToken::new();
        let mine = tokio::spawn(mine_propagation_stamp(
            [7u8; 32],
            ENDLESS_COST,
            cancel.clone(),
        ));
        sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        let outcome = timeout(Duration::from_secs(1), mine)
            .await
            .expect("a cancelled mine must return promptly")
            .unwrap();
        assert_eq!(outcome, Err(PropagationError::Cancelled));
    }

    /// With a single blocking thread, a search that outlived its dropped future would
    /// hold that thread and starve the second mine.
    #[test]
    fn dropping_the_mining_future_stops_the_search() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let abandoned = timeout(
                Duration::from_millis(50),
                mine_propagation_stamp([8u8; 32], ENDLESS_COST, CancellationToken::new()),
            )
            .await;
            assert!(abandoned.is_err(), "an endless mine finished");

            let next = timeout(
                Duration::from_secs(1),
                mine_propagation_stamp([8u8; 32], CHEAP_COST, CancellationToken::new()),
            )
            .await
            .expect("the dropped mine must have freed the blocking thread")
            .unwrap();
            assert!(next.value >= CHEAP_COST);
        });
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mining_stops_when_the_runtime_stops() {
        use crate::mesh::node::{MeshSlot, SHUTDOWN_GRACE};
        use crate::mesh::test_support::started_runtime;

        let started = started_runtime("propagation-cancel").await;
        let mine = tokio::spawn(mine_propagation_stamp(
            [9u8; 32],
            ENDLESS_COST,
            started.runtime.cancellation_token(),
        ));
        sleep(Duration::from_millis(50)).await;

        let slot = MeshSlot::default();
        slot.install(started.runtime.clone()).unwrap();
        assert!(slot.stop().await.unwrap());
        let outcome = timeout(SHUTDOWN_GRACE + Duration::from_secs(1), mine)
            .await
            .expect("stopping the node must cancel the mine")
            .unwrap();
        assert_eq!(outcome, Err(PropagationError::Cancelled));
        started.relay_handle.abort();
    }

    #[tokio::test]
    async fn preparing_an_envelope_logs_the_demanded_cost_and_stamps_the_transient() {
        install_log_collector();
        let sender = PrivateIdentity::new_from_rand(OsRng);
        let recipient = PrivateIdentity::new_from_rand(OsRng);
        let node = node_with(CHEAP_COST, DEFAULT_LIMIT_KB as u64);

        let prepared = prepare_envelope(
            &sender,
            &recipient_of(&recipient),
            &node,
            &message(b"short"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(prepared.stamp_value >= CHEAP_COST);

        let needle = logged_node(&node);
        let demand = debug_snapshot()
            .into_iter()
            .find(|line| line.contains(&needle))
            .unwrap_or_else(|| panic!("no debug line contains {needle:?}"));
        assert_eq!(demand, format!("{needle} {CHEAP_COST}"));

        let (_, elements) = decode_envelope(&prepared.envelope);
        assert_eq!(elements.len(), 1);
        let unpacked = unpack_transient(&elements[0], &recipient, CHEAP_COST, &prepared);
        assert_eq!(unpacked.verify(sender.as_identity()), Ok(true));
    }

    #[tokio::test]
    async fn oversize_envelopes_are_refused_before_any_mining() {
        install_log_collector();
        let sender = PrivateIdentity::new_from_rand(OsRng);
        let recipient = PrivateIdentity::new_from_rand(OsRng);
        let node = node_with(ENDLESS_COST, 1);

        let started = StdInstant::now();
        let outcome = prepare_envelope(
            &sender,
            &recipient_of(&recipient),
            &node,
            &message(&[b'x'; 2000]),
            CancellationToken::new(),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        match outcome {
            Err(PropagationError::Oversize { len, max }) => {
                assert_eq!(max, 1000);
                assert!(len > 2000 && len < 2300, "envelope of {len} bytes");
            }
            Err(other) => panic!("expected Oversize, got {other:?}"),
            Ok(_) => panic!("an oversize envelope was prepared"),
        }
        let needle = logged_node(&node);
        assert!(
            !debug_snapshot().iter().any(|line| line.contains(&needle)),
            "no stamp may be demanded for an envelope the node would refuse"
        );
    }

    /// `(timestamp, [transient payloads])`, the envelope `LXMessage.py:433` packs.
    fn decode_envelope(bytes: &[u8]) -> (f64, Vec<Vec<u8>>) {
        let value = rmpv::decode::read_value(&mut Cursor::new(bytes)).unwrap();
        let rmpv::Value::Array(outer) = value else {
            panic!("envelope is not an array: {value}");
        };
        assert_eq!(outer.len(), 2, "{outer:?}");
        let timestamp = outer[0].as_f64().expect("envelope timestamp is a float");
        let rmpv::Value::Array(inner) = &outer[1] else {
            panic!("envelope messages are not an array: {}", outer[1]);
        };
        let elements = inner
            .iter()
            .map(|element| match element {
                rmpv::Value::Binary(bytes) => bytes.clone(),
                other => panic!("envelope element is not binary: {other}"),
            })
            .collect();
        (timestamp, elements)
    }

    /// Checks the stamp, splits it off, checks the addressing and the transient id, and
    /// decrypts the message as the recipient's node would for a fetch.
    fn unpack_transient(
        element: &[u8],
        recipient: &PrivateIdentity,
        cost: u32,
        prepared: &PreparedEnvelope,
    ) -> WireMessage {
        let value = validate_propagation_stamp(element, cost)
            .unwrap_or_else(|| panic!("the stamp does not reach cost {cost}"));
        assert_eq!(value, prepared.stamp_value);
        let (lxmf_data, stamp) = element.split_at(element.len() - PROPAGATION_STAMP_SIZE);
        assert_eq!(stamp.len(), PROPAGATION_STAMP_SIZE);
        let transient_id: [u8; 32] = Sha256::digest(lxmf_data).into();
        assert_eq!(transient_id, prepared.transient_id);
        assert_eq!(
            &lxmf_data[..16],
            lxmf_delivery_hash(&recipient_of(recipient)).as_slice()
        );
        WireMessage::unpack_paper(lxmf_data, recipient).unwrap()
    }

    /// `watch_verdict` driven from hand-built event streams, with no transport behind them.
    mod verdict {
        use super::*;
        use rns_transport::destination::link::LinkPayload;

        const WINDOW: Duration = Duration::from_millis(100);
        /// A bound no test is meant to reach.
        const FAR: Duration = Duration::from_secs(10);
        /// Generous room over `WINDOW`, but far short of `FAR`.
        const PROMPT: Duration = Duration::from_secs(2);

        struct Watch {
            link_tx: broadcast::Sender<LinkEventData>,
            resource_tx: broadcast::Sender<ResourceEvent>,
            link_rx: broadcast::Receiver<LinkEventData>,
            resource_rx: broadcast::Receiver<ResourceEvent>,
            link_id: LinkId,
            resource: Hash,
            cancel: CancellationToken,
        }

        impl Watch {
            fn new() -> Self {
                Self::with_capacity(16)
            }

            fn with_capacity(capacity: usize) -> Self {
                let (link_tx, link_rx) = broadcast::channel(capacity);
                let (resource_tx, resource_rx) = broadcast::channel(capacity);
                Self {
                    link_tx,
                    resource_tx,
                    link_rx,
                    resource_rx,
                    link_id: AddressHash::new_from_rand(OsRng),
                    resource: Hash::new_from_rand(OsRng),
                    cancel: CancellationToken::new(),
                }
            }

            fn link(&self, id: LinkId, event: LinkEvent) {
                let sent = self
                    .link_tx
                    .send(LinkEventData {
                        id,
                        address_hash: AddressHash::new_from_rand(OsRng),
                        event,
                    })
                    .is_ok();
                assert!(sent, "the watch's link receiver is gone");
            }

            fn resource(&self, hash: Hash, kind: ResourceEventKind) {
                self.resource_tx
                    .send(ResourceEvent {
                        hash,
                        link_id: self.link_id,
                        kind,
                    })
                    .unwrap();
            }

            async fn run(
                mut self,
                in_flight: bool,
                deadline: Duration,
                window: Duration,
            ) -> Result<(), PropagationError> {
                let mut pending = in_flight.then_some(self.resource);
                watch_verdict(
                    &mut self.link_rx,
                    &mut self.resource_rx,
                    self.link_id,
                    &mut pending,
                    Deadline::after(deadline),
                    window,
                    &self.cancel,
                )
                .await
            }
        }

        fn data(bytes: &[u8], context: PacketContext) -> LinkEvent {
            LinkEvent::Data(Box::new(LinkPayload::new_from_slice_with_context(
                bytes, context,
            )))
        }

        fn refusal() -> Vec<u8> {
            let mut bytes = Vec::new();
            rmpv::encode::write_value(
                &mut bytes,
                &rmpv::Value::Array(vec![RefusalCode::InvalidStamp.to_wire()]),
            )
            .unwrap();
            bytes
        }

        #[tokio::test]
        async fn a_completed_transfer_is_accepted_one_window_later_not_at_the_deadline() {
            let watch = Watch::new();
            watch.resource(watch.resource, ResourceEventKind::OutboundComplete);
            let started = StdInstant::now();
            assert_eq!(watch.run(true, FAR, WINDOW).await, Ok(()));
            let elapsed = started.elapsed();
            assert!(elapsed >= WINDOW && elapsed < PROMPT, "{elapsed:?}");
        }

        #[tokio::test]
        async fn a_failed_or_cancelled_transfer_fails() {
            for kind in [
                ResourceEventKind::OutboundFailed,
                ResourceEventKind::OutboundCancelled,
            ] {
                let watch = Watch::new();
                watch.resource(watch.resource, kind);
                assert_eq!(
                    watch.run(true, FAR, FAR).await,
                    Err(PropagationError::TransferFailed)
                );
            }
        }

        #[tokio::test]
        async fn a_close_after_the_transfer_is_the_nodes_rejection() {
            let watch = Watch::new();
            watch.resource(watch.resource, ResourceEventKind::OutboundComplete);
            watch.link(watch.link_id, LinkEvent::Closed);
            assert_eq!(
                watch.run(true, FAR, FAR).await,
                Err(PropagationError::ClosedAfterTransfer)
            );
        }

        #[tokio::test]
        async fn a_close_during_the_transfer_is_a_closed_link() {
            let watch = Watch::new();
            watch.link(watch.link_id, LinkEvent::Closed);
            assert_eq!(
                watch.run(true, FAR, FAR).await,
                Err(PropagationError::Link(R3Error::LinkClosed))
            );
        }

        #[tokio::test]
        async fn the_deadline_expiring_during_the_transfer_is_a_timeout() {
            let watch = Watch::new();
            assert_eq!(
                watch.run(true, WINDOW, FAR).await,
                Err(PropagationError::Link(R3Error::Timeout {
                    path: PROPAGATION_PATH_LABEL.to_string(),
                    after: WINDOW,
                }))
            );
        }

        #[tokio::test]
        async fn a_refusal_sentinel_rejects_the_message() {
            let watch = Watch::new();
            watch.link(watch.link_id, data(&refusal(), PacketContext::None));
            assert_eq!(
                watch.run(false, FAR, FAR).await,
                Err(PropagationError::RejectedByNode(RefusalCode::InvalidStamp))
            );
        }

        #[tokio::test]
        async fn other_links_and_non_signalling_data_are_not_verdicts() {
            let watch = Watch::new();
            let other = AddressHash::new_from_rand(OsRng);
            watch.link(other, LinkEvent::Closed);
            watch.link(other, data(&refusal(), PacketContext::None));
            watch.link(watch.link_id, data(&refusal(), PacketContext::Response));
            watch.link(watch.link_id, LinkEvent::Activated);
            watch.resource(
                Hash::new_from_rand(OsRng),
                ResourceEventKind::OutboundFailed,
            );
            assert_eq!(watch.run(false, FAR, WINDOW).await, Ok(()));
        }

        #[tokio::test]
        async fn cancelling_inside_the_window_is_cancelled() {
            let watch = Watch::new();
            let cancel = watch.cancel.clone();
            tokio::spawn(async move {
                sleep(Duration::from_millis(50)).await;
                cancel.cancel();
            });
            let started = StdInstant::now();
            assert_eq!(
                watch.run(false, FAR, FAR).await,
                Err(PropagationError::Cancelled)
            );
            assert!(started.elapsed() < PROMPT);
        }

        #[tokio::test]
        async fn lagging_inside_the_window_loses_the_verdict() {
            let watch = Watch::with_capacity(1);
            for _ in 0..3 {
                watch.link(watch.link_id, LinkEvent::Activated);
            }
            assert_eq!(
                watch.run(false, FAR, FAR).await,
                Err(PropagationError::VerdictLost { skipped: 2 })
            );
        }

        #[tokio::test]
        async fn lagging_during_the_transfer_is_tolerated() {
            let watch = Watch::with_capacity(1);
            for _ in 0..3 {
                watch.link(watch.link_id, LinkEvent::Activated);
            }
            let resource_tx = watch.resource_tx.clone();
            let event = ResourceEvent {
                hash: watch.resource,
                link_id: watch.link_id,
                kind: ResourceEventKind::OutboundComplete,
            };
            tokio::spawn(async move {
                sleep(Duration::from_millis(50)).await;
                resource_tx.send(event).unwrap();
            });
            assert_eq!(watch.run(true, FAR, WINDOW).await, Ok(()));
        }
    }

    #[cfg(unix)]
    mod network {
        use super::*;
        use crate::mesh::node::SHUTDOWN_GRACE;

        use rns_transport::destination::SingleInputDestination;
        use rns_transport::iface::tcp_client::TcpClient;
        use rns_transport::iface::tcp_server::TcpServer;
        use rns_transport::resource::{LINK_PACKET_MDU, ResourceComplete};
        use rns_transport::transport::{AnnounceEvent, TransportConfig};
        use std::sync::atomic::AtomicBool;
        use tokio::net::TcpListener;
        use tokio::sync::mpsc;
        use tokio::task::JoinHandle;

        const POLL: Duration = Duration::from_millis(100);
        const INTEROP_TIMEOUT: Duration = Duration::from_secs(15);
        /// Reticulum's original 500-byte MTU, at which the link MDU is 431 and a small
        /// message can reach the packet/resource boundary.
        const LEGACY_LINK_MTU: usize = 500;

        async fn closed_port() -> u16 {
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = probe.local_addr().unwrap().port();
            drop(probe);
            port
        }

        async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
            let deadline = Instant::now() + INTEROP_TIMEOUT;
            while !condition() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                sleep(POLL).await;
            }
        }

        /// What the fake node saw arrive on its propagation destination.
        enum Received {
            Packet {
                context: PacketContext,
                bytes: Vec<u8>,
            },
            Resource(ResourceComplete),
        }

        /// A bare transport serving `lxmf.propagation`: it records what arrives and, when
        /// told to, answers each arrival with the reference's stamp refusal or tears the
        /// link down. It is a listening post, not a propagation node.
        struct FakeNode {
            transport: Arc<Transport>,
            dest: Arc<tokio::sync::Mutex<SingleInputDestination>>,
            desc: DestinationDesc,
            iface: AddressHash,
            received: mpsc::UnboundedReceiver<Received>,
            reject: Arc<AtomicBool>,
            teardown: Arc<AtomicBool>,
            drain: JoinHandle<()>,
            port: u16,
        }

        impl FakeNode {
            async fn listen() -> Self {
                let port = closed_port().await;
                let transport = Arc::new(Transport::new(TransportConfig::new(
                    "pn",
                    &TransportIdentity::new_from_rand(OsRng),
                    false,
                )));
                let tcp = TcpServer::new(format!("127.0.0.1:{port}"), transport.iface_manager())
                    .with_client_mtu(LEGACY_LINK_MTU);
                let status = tcp.runtime_status_handle();
                let iface = transport
                    .iface_manager()
                    .lock()
                    .await
                    .spawn(tcp, TcpServer::spawn);
                wait_until("the fake node to listen", || {
                    status.to_json()["listener_state"].as_str() == Some("listening")
                })
                .await;
                let dest = transport
                    .add_destination(
                        TransportIdentity::new_from_rand(OsRng),
                        DestinationName::new("lxmf", PROPAGATION_ASPECT),
                    )
                    .await;
                let desc = dest.lock().await.desc;
                let (tx, received) = mpsc::unbounded_channel();
                let reject = Arc::new(AtomicBool::new(false));
                let teardown = Arc::new(AtomicBool::new(false));
                let drain = tokio::spawn(drain(
                    transport.clone(),
                    transport.in_link_events(),
                    transport.resource_events(),
                    tx,
                    reject.clone(),
                    teardown.clone(),
                ));
                Self {
                    transport,
                    dest,
                    desc,
                    iface,
                    received,
                    reject,
                    teardown,
                    drain,
                    port,
                }
            }

            async fn announce(&self, app_data: &[u8]) {
                let packet = self
                    .dest
                    .lock()
                    .await
                    .announce(OsRng, Some(app_data))
                    .unwrap();
                self.transport.send_packet(packet).await;
            }

            async fn next_received(&mut self) -> Received {
                timeout(INTEROP_TIMEOUT, self.received.recv())
                    .await
                    .expect("the fake node must receive the transfer")
                    .unwrap()
            }

            fn nothing_else_received(&mut self) {
                assert!(
                    self.received.try_recv().is_err(),
                    "the fake node received more than one transfer"
                );
            }

            async fn stop(self) {
                self.drain.abort();
                self.transport
                    .iface_manager()
                    .lock()
                    .await
                    .stop_interface(self.iface);
            }
        }

        /// Forwards data packets and completed resources on the node's in-links, and sends
        /// `msgpack([ERROR_INVALID_STAMP])` back on the same link when `reject` is set, as
        /// `LXMRouter.propagation_packet` does (`LXMRouter.py:2134-2136`), or tears the link
        /// down when `teardown` is set, as `LXMRouter.propagation_resource_concluded` does
        /// for a rejected resource (`LXMRouter.py:2277-2278`).
        async fn drain(
            transport: Arc<Transport>,
            mut link_events: broadcast::Receiver<LinkEventData>,
            mut resource_events: broadcast::Receiver<ResourceEvent>,
            tx: mpsc::UnboundedSender<Received>,
            reject: Arc<AtomicBool>,
            teardown: Arc<AtomicBool>,
        ) {
            loop {
                let (link_id, received) = tokio::select! {
                    event = link_events.recv() => match event {
                        Ok(LinkEventData { id, event: LinkEvent::Data(payload), .. }) => (
                            id,
                            Received::Packet {
                                context: payload.context(),
                                bytes: payload.as_slice().to_vec(),
                            },
                        ),
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    event = resource_events.recv() => match event {
                        Ok(ResourceEvent { link_id, kind: ResourceEventKind::Complete(complete), .. }) => {
                            (link_id, Received::Resource(complete))
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                };
                if reject.load(Ordering::SeqCst) {
                    let mut signal = Vec::new();
                    rmpv::encode::write_value(
                        &mut signal,
                        &rmpv::Value::Array(vec![RefusalCode::InvalidStamp.to_wire()]),
                    )
                    .unwrap();
                    let link = transport
                        .find_in_link(&link_id)
                        .await
                        .expect("the in-link the transfer arrived on is still up");
                    let packet = link.lock().await.data_packet(&signal).unwrap();
                    transport
                        .send_link_packet_on_bound_iface(&link, packet)
                        .await;
                }
                if teardown.load(Ordering::SeqCst) {
                    let link = transport
                        .find_in_link(&link_id)
                        .await
                        .expect("the in-link the transfer arrived on is still up");
                    let packet = link
                        .lock()
                        .await
                        .teardown()
                        .expect("an active in-link yields a teardown packet");
                    transport
                        .send_link_packet_on_bound_iface(&link, packet)
                        .await;
                }
                if tx.send(received).is_err() {
                    return;
                }
            }
        }

        /// The posting side: a bare transport joined to the fake node over TCP.
        struct Client {
            transport: Arc<Transport>,
            announces: broadcast::Receiver<AnnounceEvent>,
            iface: AddressHash,
            iface_task: JoinHandle<()>,
        }

        impl Client {
            async fn connect(port: u16) -> Self {
                let transport = Arc::new(Transport::new(TransportConfig::new(
                    "client",
                    &TransportIdentity::new_from_rand(OsRng),
                    false,
                )));
                let announces = transport.recv_announces().await;
                let tcp = TcpClient::new(format!("127.0.0.1:{port}")).with_mtu(LEGACY_LINK_MTU);
                let status = tcp.runtime_status_handle();
                let context = transport.iface_manager().lock().await.new_context(tcp);
                let iface = *context.channel.address();
                let iface_task = tokio::spawn(TcpClient::spawn(context));
                wait_until("the client to connect", || {
                    status.to_json()["stream_state"].as_str() == Some("connected")
                })
                .await;
                Self {
                    transport,
                    announces,
                    iface,
                    iface_task,
                }
            }

            /// The announce for `hash` as the transport delivers it: the learned destination
            /// and the app_data that came with it.
            async fn learn(&mut self, hash: &AddressHash) -> (DestinationDesc, Vec<u8>) {
                let deadline = Instant::now() + INTEROP_TIMEOUT;
                loop {
                    let event = tokio::time::timeout_at(deadline, self.announces.recv())
                        .await
                        .expect("the client must hear the fake node's announce")
                        .unwrap();
                    let desc = event.destination.lock().await.desc;
                    if desc.address_hash == *hash {
                        return (desc, event.app_data.as_slice().to_vec());
                    }
                }
            }

            async fn stop(self) {
                let _ = timeout(SHUTDOWN_GRACE, self.transport.stop_interface(self.iface)).await;
                self.iface_task.abort();
            }
        }

        /// Everything one end-to-end post needs: the two transports, the node as the client
        /// learned it from the announce, and fresh sender and recipient identities.
        struct Post {
            node: FakeNode,
            client: Client,
            learned: PropagationNode,
            sender: PrivateIdentity,
            recipient: PrivateIdentity,
            options: PropagationOptions,
        }

        impl Post {
            async fn start(cost: i64, per_transfer_kb: i64) -> Self {
                let node = FakeNode::listen().await;
                let mut client = Client::connect(node.port).await;
                node.announce(&pn_app_data(cost, per_transfer_kb)).await;
                let (desc, app_data) = client.learn(&node.desc.address_hash).await;
                let learned = PropagationNode::from_announce(&desc, &app_data).unwrap();
                Self {
                    node,
                    client,
                    learned,
                    sender: PrivateIdentity::new_from_rand(OsRng),
                    recipient: PrivateIdentity::new_from_rand(OsRng),
                    // A post the node accepts spends the whole window waiting; loopback
                    // needs far less of it than a real link.
                    options: PropagationOptions {
                        reject_window: Duration::from_millis(300),
                        ..Default::default()
                    },
                }
            }

            async fn propagate(
                &self,
                message: &OutboundMessage,
            ) -> Result<PropagationOutcome, PropagationError> {
                propagate(
                    &self.client.transport,
                    &self.sender,
                    &recipient_of(&self.recipient),
                    &self.learned,
                    message,
                    CancellationToken::new(),
                    &self.options,
                )
                .await
            }

            /// Decodes what the node received and checks it against `outcome` and the
            /// message that was sent.
            fn check_received(
                &self,
                bytes: &[u8],
                outcome: &PropagationOutcome,
                message: &OutboundMessage,
            ) {
                assert_eq!(bytes.len(), outcome.envelope_len);
                let (timestamp, elements) = decode_envelope(bytes);
                assert!(timestamp > 1_700_000_000.0, "{timestamp}");
                assert_eq!(elements.len(), 1, "one message per transfer");
                let prepared = PreparedEnvelope {
                    envelope: bytes.to_vec(),
                    transient_id: outcome.transient_id,
                    stamp_value: outcome.stamp_value,
                };
                let unpacked = unpack_transient(
                    &elements[0],
                    &self.recipient,
                    self.learned.stamp_cost,
                    &prepared,
                );
                assert_eq!(unpacked.verify(self.sender.as_identity()), Ok(true));
                assert_eq!(
                    unpacked.payload.content.as_deref().map(Vec::as_slice),
                    Some(message.content.as_slice())
                );
                assert_eq!(
                    unpacked.payload.title.as_deref().map(Vec::as_slice),
                    Some(message.title.as_deref().unwrap_or_default())
                );
                assert_eq!(
                    unpacked.payload.fields,
                    Some(
                        message
                            .fields
                            .clone()
                            .unwrap_or_else(|| rmpv::Value::Map(vec![]))
                    )
                );
                assert_eq!(
                    &unpacked.source[..],
                    lxmf_delivery_hash(&recipient_of(&self.sender)).as_slice()
                );
            }

            async fn stop(self) {
                self.client.stop().await;
                self.node.stop().await;
            }
        }

        /// The envelope length a message of `content_len` bytes produces; every part of
        /// the envelope apart from the ciphertext is fixed-width, so it is a function of
        /// the content alone.
        fn envelope_len_for(
            sender: &PrivateIdentity,
            recipient: &Identity,
            content_len: usize,
        ) -> usize {
            let wire = build_signed_message(
                sender,
                &lxmf_delivery_hash(recipient),
                &OutboundMessage {
                    title: None,
                    content: vec![b'x'; content_len],
                    fields: None,
                },
                1_700_000_000.0,
            )
            .unwrap();
            let (lxmf_data, _) = wire
                .pack_propagation_transient_with_rng(&to_core_identity(recipient), OsRng)
                .unwrap();
            WireMessage::pack_propagation_envelope(
                1_700_000_000.0,
                &lxmf_data,
                Some(&[0u8; PROPAGATION_STAMP_SIZE]),
            )
            .unwrap()
            .len()
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_short_message_travels_as_a_link_packet_the_node_can_decode() {
            let mut post = Post::start(1, DEFAULT_LIMIT_KB).await;
            assert_eq!(post.learned.stamp_cost, 1);
            assert_eq!(post.learned.per_transfer_limit_kb, 256);
            let message = message(b"a short propagated message");

            let outcome = post.propagate(&message).await.unwrap();
            assert_eq!(outcome.representation, SizeBranch::Packet);
            assert!(outcome.envelope_len <= LINK_PACKET_MDU);

            match post.node.next_received().await {
                Received::Packet { context, bytes, .. } => {
                    assert_eq!(context, PacketContext::None);
                    post.check_received(&bytes, &outcome, &message);
                }
                Received::Resource(_) => panic!("a short message must travel as a packet"),
            }
            post.node.nothing_else_received();
            post.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_long_message_travels_as_a_raw_resource_the_node_can_decode() {
            let mut post = Post::start(1, DEFAULT_LIMIT_KB).await;
            let message = message(&[b'y'; 2000]);

            let outcome = post.propagate(&message).await.unwrap();
            assert_eq!(outcome.representation, SizeBranch::Resource);
            assert!(outcome.envelope_len > LINK_PACKET_MDU);

            match post.node.next_received().await {
                Received::Resource(complete) => {
                    assert!(!complete.is_request);
                    assert!(!complete.is_response);
                    assert_eq!(complete.request_id, None);
                    post.check_received(&complete.data, &outcome, &message);
                }
                Received::Packet { .. } => panic!("a long message must travel as a resource"),
            }
            post.node.nothing_else_received();
            post.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn envelope_sizes_either_side_of_the_mdu_pick_packet_and_resource() {
            let mut post = Post::start(1, DEFAULT_LIMIT_KB).await;
            let recipient = recipient_of(&post.recipient);
            let mdu = LINK_PACKET_MDU;
            assert_eq!(mdu, 431);

            // The envelope is 142 bytes of fixed framing plus a 16-byte-padded ciphertext
            // of the msgpack payload (empty title and map included), so its length steps
            // by 16 as the content grows.
            let largest_fitting = (1..600)
                .rev()
                .find(|&len| envelope_len_for(&post.sender, &recipient, len) <= mdu)
                .unwrap();
            let fitting_len = envelope_len_for(&post.sender, &recipient, largest_fitting);
            let overflowing_len = envelope_len_for(&post.sender, &recipient, largest_fitting + 1);
            assert!(
                fitting_len <= mdu && mdu < overflowing_len,
                "{fitting_len} / {overflowing_len}"
            );
            assert!(
                mdu - fitting_len < 16,
                "the fitting envelope is {fitting_len} bytes"
            );
            assert_eq!((fitting_len - 142) % 16, 0, "{fitting_len}");
            assert_eq!(overflowing_len, fitting_len + 16);

            for (content_len, expected_len, expected) in [
                (largest_fitting, fitting_len, SizeBranch::Packet),
                (largest_fitting + 1, overflowing_len, SizeBranch::Resource),
            ] {
                let message = OutboundMessage {
                    title: None,
                    content: vec![b'x'; content_len],
                    fields: None,
                };
                let outcome = post.propagate(&message).await.unwrap();
                assert_eq!(outcome.envelope_len, expected_len);
                assert_eq!(
                    outcome.representation, expected,
                    "{content_len} bytes of content"
                );
                match (post.node.next_received().await, expected) {
                    (Received::Packet { bytes, context, .. }, SizeBranch::Packet) => {
                        assert_eq!(context, PacketContext::None);
                        post.check_received(&bytes, &outcome, &message);
                    }
                    (Received::Resource(complete), SizeBranch::Resource) => {
                        post.check_received(&complete.data, &outcome, &message);
                    }
                    (Received::Packet { .. }, SizeBranch::Resource) => {
                        panic!("{content_len} bytes of content arrived as a packet")
                    }
                    (Received::Resource(_), SizeBranch::Packet) => {
                        panic!("{content_len} bytes of content arrived as a resource")
                    }
                }
            }
            post.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_refusal_sentinel_from_the_node_rejects_the_message() {
            let mut post = Post::start(1, DEFAULT_LIMIT_KB).await;
            post.options = PropagationOptions::default();
            post.node.reject.store(true, Ordering::SeqCst);

            let outcome = post.propagate(&message(b"rejected")).await;
            assert_eq!(
                outcome,
                Err(PropagationError::RejectedByNode(RefusalCode::InvalidStamp))
            );
            assert!(matches!(
                post.node.next_received().await,
                Received::Packet { .. }
            ));
            post.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_teardown_after_a_received_resource_is_the_nodes_rejection() {
            let mut post = Post::start(1, DEFAULT_LIMIT_KB).await;
            post.node.teardown.store(true, Ordering::SeqCst);

            let outcome = post.propagate(&message(&[b'y'; 2000])).await;
            assert_eq!(outcome, Err(PropagationError::ClosedAfterTransfer));
            assert!(matches!(
                post.node.next_received().await,
                Received::Resource(_)
            ));
            post.stop().await;
        }

        /// At this MTU a 200 KB envelope takes the transport about 0.1 s to accept and
        /// about 1.5 s to deliver, so a 400 ms deadline expires with the resource in
        /// flight.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_transfer_the_deadline_overtakes_is_cancelled_at_the_transport() {
            const TRANSFER_TIMEOUT: Duration = Duration::from_millis(400);
            let mut post = Post::start(1, 2000).await;
            post.options.transfer_timeout = TRANSFER_TIMEOUT;
            let mut resource_events = post.client.transport.resource_events();

            let outcome = post.propagate(&message(&[b'z'; 200_000])).await;
            assert_eq!(
                outcome,
                Err(PropagationError::Link(R3Error::Timeout {
                    path: PROPAGATION_PATH_LABEL.to_string(),
                    after: TRANSFER_TIMEOUT,
                }))
            );
            timeout(Duration::from_secs(2), async {
                loop {
                    match resource_events.recv().await {
                        Ok(ResourceEvent {
                            kind: ResourceEventKind::OutboundCancelled,
                            ..
                        }) => break,
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {
                            panic!("the client transport went away")
                        }
                    }
                }
            })
            .await
            .expect("the client transport must report the cancelled resource");

            sleep(Duration::from_secs(1)).await;
            assert!(
                post.node.received.try_recv().is_err(),
                "the node completed a transfer the client had cancelled"
            );
            post.stop().await;
        }
    }
}
