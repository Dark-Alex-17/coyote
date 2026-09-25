use crate::mesh::r3::error::{R3Error, RefusalCode};
use crate::mesh::r3::frame::{
    Envelope, MAX_R3_PAYLOAD_BYTES, RequestFrame, RequestId, ResponseFrame,
};
use crate::mesh::r3::receipt::RequestReceipt;
use crate::mesh::r3::short;

use parking_lot::Mutex;
use rmpv::Value;
use rns_transport::PacketContext;
use rns_transport::delivery::await_link_activation;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::link::{Link, LinkEvent, LinkEventData, LinkId};
use rns_transport::hash::Hash;
use rns_transport::identity::PrivateIdentity as TransportIdentity;
use rns_transport::resource::{ResourceEvent, ResourceEventKind};
use rns_transport::transport::{SendPacketOutcome, Transport};
use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

/// Ceiling on waiting for a response. RNS derives its default from the link RTT
/// (`rtt * 6 + 11.25s`); a fixed bound is simpler and enough for a LAN or a relay hop.
pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on link establishment, before the request itself is sent.
pub(crate) const DEFAULT_LINK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestOptions {
    pub request_timeout: Duration,
    pub link_timeout: Duration,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            link_timeout: DEFAULT_LINK_TIMEOUT,
        }
    }
}

/// When one phase of a request gives up, and the option that set it, which the `Timeout`
/// error reports. Every wait against the transport or a link lock runs under one, since a
/// transport whose handler lock is held for good (upstream rev 3ed5932 does that to itself
/// on any advertisement-time reject) would never return from a bare await. We register no
/// response-size limit that could trip that path; oversize responses are dropped after
/// assembly in `deliver`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    at: Instant,
    after: Duration,
}

impl Deadline {
    pub(crate) fn after(after: Duration) -> Self {
        Self {
            at: Instant::now() + after,
            after,
        }
    }

    pub(crate) fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    pub(crate) async fn bound<T>(
        self,
        path: &str,
        wait: impl Future<Output = T>,
    ) -> Result<T, R3Error> {
        timeout_at(self.at, wait)
            .await
            .map_err(|_| self.expired(path))
    }

    pub(crate) fn expired(self, path: &str) -> R3Error {
        R3Error::Timeout {
            path: path.to_string(),
            after: self.after,
        }
    }
}

/// How one half of an exchange travelled: as a single link packet when the encoded frame
/// fits the link's MDU, as a resource otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeBranch {
    Packet,
    Resource,
}

// `request_id` and the branches wait for a production reader; the tests assert on them.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct RequestOutcome {
    pub value: Value,
    pub request_id: RequestId,
    pub request_branch: SizeBranch,
    pub response_branch: SizeBranch,
}

type Correlated = Result<(Value, SizeBranch), R3Error>;

struct PendingRequest {
    link_id: LinkId,
    reply: oneshot::Sender<Correlated>,
    /// Fires when the far end proves it holds the request; only a resource gets that proof.
    delivered: Option<oneshot::Sender<()>>,
    resource_hash: Option<Hash>,
}

/// Requests awaiting a response, by request id, and for those sent as a resource the
/// transfer hash upstream reports outbound progress under.
#[derive(Default)]
struct Pending {
    by_request: HashMap<RequestId, PendingRequest>,
    by_resource: HashMap<Hash, RequestId>,
}

impl Pending {
    fn remove(&mut self, request_id: &RequestId) -> Option<PendingRequest> {
        let entry = self.by_request.remove(request_id)?;
        if let Some(hash) = entry.resource_hash {
            self.by_resource.remove(&hash);
        }
        Some(entry)
    }

    fn remove_link(&mut self, link_id: LinkId) -> Vec<PendingRequest> {
        let closed: Vec<PendingRequest> = self
            .by_request
            .extract_if(|_, entry| entry.link_id == link_id)
            .map(|(_, entry)| entry)
            .collect();
        for hash in closed.iter().filter_map(|entry| entry.resource_hash) {
            self.by_resource.remove(&hash);
        }
        closed
    }
}

/// Removes the pending entry when the request future goes away, whichever way it does. A
/// reply has already removed it and a timeout wants it gone; a caller that drops the future
/// mid-wait would otherwise leave it until the link closed.
struct PendingGuard<'a> {
    client: &'a R3Client,
    request_id: RequestId,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.client.remove_pending(&self.request_id);
    }
}

/// The requesting side: sends requests over out-links and correlates the responses that
/// arrive on the transport's out-link and resource event streams. One instance serves every
/// request a node makes; `run` must be spawned before the first request is sent.
pub(crate) struct R3Client {
    pending: Mutex<Pending>,
    /// Set once `run` has exited, under the pending lock, so no request can slip into the
    /// table after the last drain and wait out its timeout with nobody to answer it.
    closed: AtomicBool,
}

impl R3Client {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(Pending::default()),
            closed: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.lock().by_request.len()
    }

    /// Opens (or reuses) a link to `destination`, proves `identity` on it, and sends one
    /// request for `path`, its body in the envelope that names the instance asking.
    /// `link_timeout` covers opening the link and identifying on it; `request_timeout`
    /// starts once that is done and covers sending the request as well as waiting for the
    /// response.
    ///
    /// Links are reused per destination for as long as upstream keeps them: its watchdog
    /// closes an idle link and nothing here closes one sooner. The identity is proven on
    /// every request regardless; it is one packet, and it also heals a responder that
    /// missed the first proof.
    pub(crate) async fn request(
        &self,
        transport: &Transport,
        identity: &TransportIdentity,
        destination: &DestinationDesc,
        path: &str,
        envelope: Envelope,
        options: RequestOptions,
    ) -> Result<RequestOutcome, R3Error> {
        let link = link_to(transport, identity, destination, path, options.link_timeout).await?;
        self.request_on_link_with(
            transport,
            &link,
            path,
            envelope,
            Deadline::after(options.request_timeout),
            None,
        )
        .await
    }

    /// `request`, returned at once as a receipt that reports progress while the request
    /// runs on its own task. Dropping the receipt abandons the request; `cancel` firing
    /// fails it with `Shutdown`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn request_with_receipt(
        self: &Arc<Self>,
        transport: Arc<Transport>,
        identity: TransportIdentity,
        destination: DestinationDesc,
        path: String,
        envelope: Envelope,
        options: RequestOptions,
        cancel: CancellationToken,
    ) -> RequestReceipt {
        let client = self.clone();
        RequestReceipt::track(cancel, move |delivered| async move {
            let link = link_to(
                &transport,
                &identity,
                &destination,
                &path,
                options.link_timeout,
            )
            .await?;
            client
                .request_on_link_with(
                    &transport,
                    &link,
                    &path,
                    envelope,
                    Deadline::after(options.request_timeout),
                    Some(delivered),
                )
                .await
        })
    }

    /// Sends `data` as the request body verbatim, with no envelope, over an already active
    /// link. Coyote-to-Coyote requests never take this path. The LXMF `/get` conversation
    /// does: a propagation node reads the body as the bare `[wants, haves, limit]` arrays
    /// of `LXMRouter.message_get_request` (`LXMRouter.py:1436-1475`), so wrapping it in the
    /// envelope would make every round unreadable to it. Tests use the same path to put a
    /// body of their own choosing, malformed ones included, in front of a responder.
    pub(crate) async fn request_on_link(
        &self,
        transport: &Transport,
        link: &Arc<tokio::sync::Mutex<Link>>,
        path: &str,
        data: Value,
        deadline: Deadline,
    ) -> Result<RequestOutcome, R3Error> {
        self.send_on_link(transport, link, path, data, deadline, None)
            .await
    }

    /// Sends one enveloped request over an already active link. `delivered` fires once the
    /// far end has proven it holds the request; only a request that travels as a resource
    /// is proven, so for a packet the hook is dropped unfired, since upstream proves no
    /// link packet.
    pub(crate) async fn request_on_link_with(
        &self,
        transport: &Transport,
        link: &Arc<tokio::sync::Mutex<Link>>,
        path: &str,
        envelope: Envelope,
        deadline: Deadline,
        delivered: Option<oneshot::Sender<()>>,
    ) -> Result<RequestOutcome, R3Error> {
        self.send_on_link(
            transport,
            link,
            path,
            envelope.into_value(),
            deadline,
            delivered,
        )
        .await
    }

    /// The branch is decided the way RNS `Link.request` decides it: by comparing the
    /// encoded frame against the link's MDU. Sending counts against `deadline` as much as
    /// waiting for the response does.
    async fn send_on_link(
        &self,
        transport: &Transport,
        link: &Arc<tokio::sync::Mutex<Link>>,
        path: &str,
        data: Value,
        deadline: Deadline,
        delivered: Option<oneshot::Sender<()>>,
    ) -> Result<RequestOutcome, R3Error> {
        let packed = RequestFrame::new(path, data).encode();
        if packed.len() > MAX_R3_PAYLOAD_BYTES {
            return Err(R3Error::Oversize {
                len: packed.len(),
                max: MAX_R3_PAYLOAD_BYTES,
            });
        }
        let (link_id, mdu) = deadline
            .bound(path, async {
                let link = link.lock().await;
                (*link.id(), link.link_mdu())
            })
            .await?;
        let (receiver, request_id, request_branch, _guard) = if packed.len() <= mdu {
            drop(delivered);
            let packet = deadline
                .bound(path, link.lock())
                .await?
                .request_packet(&packed)
                .map_err(|err| R3Error::Send(format!("request packet: {err}")))?;
            let request_id = RequestId::from_packet(&packet);
            debug!(
                "Sending mesh request {} for {path} as a packet ({} bytes, link MDU {mdu})",
                request_id.to_hex_string(),
                packed.len()
            );
            let receiver = self.insert_pending(request_id, link_id, None)?;
            let guard = PendingGuard {
                client: self,
                request_id,
            };
            match deadline
                .bound(
                    path,
                    transport.send_link_packet_on_bound_iface(link, packet),
                )
                .await?
            {
                SendPacketOutcome::SentDirect => {}
                outcome => {
                    return Err(R3Error::LinkFailed(format!(
                        "request packet not sent: {outcome:?}"
                    )));
                }
            }
            (receiver, request_id, SizeBranch::Packet, guard)
        } else {
            let request_id = RequestId::of_packed(&packed);
            debug!(
                "Sending mesh request {} for {path} as a resource ({} bytes, link MDU {mdu})",
                request_id.to_hex_string(),
                packed.len()
            );
            let receiver = self.insert_pending(request_id, link_id, delivered)?;
            let guard = PendingGuard {
                client: self,
                request_id,
            };
            let resource_hash = deadline
                .bound(
                    path,
                    transport.send_request_resource(&link_id, request_id.to_vec(), packed, None),
                )
                .await?
                .map_err(|err| R3Error::Send(format!("request resource: {err}")))?;
            self.track_outbound(resource_hash, request_id);
            (receiver, request_id, SizeBranch::Resource, guard)
        };

        let reply = match timeout_at(deadline.at, receiver).await {
            Ok(Ok(reply)) => reply,
            // The sender only goes away with the pending table itself.
            Ok(Err(_)) => Err(R3Error::Shutdown),
            Err(_) => Err(deadline.expired(path)),
        };
        let (value, response_branch) = reply?;
        if let Some(code) = RefusalCode::from_wire(&value) {
            return Err(R3Error::Refused(code));
        }
        Ok(RequestOutcome {
            value,
            request_id,
            request_branch,
            response_branch,
        })
    }

    /// Correlates responses until `cancel` fires or the transport goes away, then fails every
    /// waiter with `Shutdown`.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut link_events: broadcast::Receiver<LinkEventData>,
        mut resource_events: broadcast::Receiver<ResourceEvent>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                event = link_events.recv() => match event {
                    Ok(event) => self.on_link_event(event),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("Mesh request client fell behind and skipped {skipped} link events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                event = resource_events.recv() => match event {
                    Ok(event) => self.on_resource_event(event),
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("Mesh request client fell behind and skipped {skipped} resource events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        self.fail_all(R3Error::Shutdown);
    }

    /// `Shutdown` once `run` has exited: a link may outlive the client loop, and a request
    /// filed after the drain would otherwise wait out its full timeout.
    fn insert_pending(
        &self,
        request_id: RequestId,
        link_id: LinkId,
        delivered: Option<oneshot::Sender<()>>,
    ) -> Result<oneshot::Receiver<Correlated>, R3Error> {
        let (reply, receiver) = oneshot::channel();
        let mut pending = self.pending.lock();
        if self.closed.load(Ordering::SeqCst) {
            return Err(R3Error::Shutdown);
        }
        pending.by_request.insert(
            request_id,
            PendingRequest {
                link_id,
                reply,
                delivered,
                resource_hash: None,
            },
        );
        Ok(receiver)
    }

    /// Files the transfer hash of a request sent as a resource, unless the request has
    /// already been settled (a link that closed during the send does that).
    fn track_outbound(&self, resource_hash: Hash, request_id: RequestId) {
        let mut pending = self.pending.lock();
        if let Some(entry) = pending.by_request.get_mut(&request_id) {
            entry.resource_hash = Some(resource_hash);
            pending.by_resource.insert(resource_hash, request_id);
        }
    }

    fn remove_pending(&self, request_id: &RequestId) -> Option<PendingRequest> {
        self.pending.lock().remove(request_id)
    }

    fn fail_all(&self, error: R3Error) {
        let drained = {
            let mut pending = self.pending.lock();
            self.closed.store(true, Ordering::SeqCst);
            std::mem::take(&mut *pending)
        };
        for (_, pending) in drained.by_request {
            let _ = pending.reply.send(Err(error.clone()));
        }
    }

    fn on_link_event(&self, event: LinkEventData) {
        match event.event {
            LinkEvent::Data(payload) if payload.context() == PacketContext::Response => {
                self.deliver(event.id, payload.as_slice(), SizeBranch::Packet);
            }
            LinkEvent::Closed => {
                let closed = self.pending.lock().remove_link(event.id);
                if !closed.is_empty() {
                    debug!(
                        "Mesh link {} closed with {} requests pending",
                        event.id.to_hex_string(),
                        closed.len()
                    );
                }
                for entry in closed {
                    let _ = entry.reply.send(Err(R3Error::LinkClosed));
                }
            }
            LinkEvent::Activated | LinkEvent::Data(_) | LinkEvent::PeerIdentified(_) => {}
        }
    }

    fn on_resource_event(&self, event: ResourceEvent) {
        match event.kind {
            ResourceEventKind::Complete(complete) if complete.is_response => {
                self.deliver(event.link_id, &complete.data, SizeBranch::Resource);
            }
            ResourceEventKind::OutboundComplete => {
                let delivered = {
                    let mut pending = self.pending.lock();
                    let request_id = pending.by_resource.get(&event.hash).copied();
                    request_id.and_then(|request_id| {
                        let hook = pending.by_request.get_mut(&request_id)?.delivered.take();
                        Some((request_id, hook))
                    })
                };
                if let Some((request_id, hook)) = delivered {
                    debug!(
                        "Mesh request {} was delivered as a resource",
                        request_id.to_hex_string()
                    );
                    if let Some(hook) = hook {
                        let _ = hook.send(());
                    }
                }
            }
            ResourceEventKind::OutboundFailed => {
                let failed = {
                    let mut pending = self.pending.lock();
                    let request_id = pending.by_resource.get(&event.hash).copied();
                    request_id
                        .and_then(|request_id| Some((request_id, pending.remove(&request_id)?)))
                };
                if let Some((request_id, entry)) = failed {
                    debug!(
                        "Mesh request {} failed in transfer as a resource on link {}",
                        request_id.to_hex_string(),
                        event.link_id.to_hex_string()
                    );
                    let _ = entry.reply.send(Err(R3Error::Send(
                        "request resource failed in transfer".to_string(),
                    )));
                }
            }
            _ => {}
        }
    }

    /// Resolves the pending request a response answers, provided it arrived on the link the
    /// request went out on. This is where the payload cap is enforced: after assembly, on
    /// our side. The only bound before assembly is the upstream 32 MiB advertisement cap
    /// (`advertisement_limits.rs`), because the upstream reject path deadlocks the transport
    /// (rev 3ed5932).
    fn deliver(&self, link_id: LinkId, bytes: &[u8], branch: SizeBranch) {
        if bytes.len() > MAX_R3_PAYLOAD_BYTES {
            debug!(
                "Dropped an oversize mesh response on link {} ({} bytes, max {MAX_R3_PAYLOAD_BYTES})",
                link_id.to_hex_string(),
                bytes.len()
            );
            return;
        }
        let frame = match ResponseFrame::decode(bytes) {
            Ok(frame) => frame,
            Err(err) => {
                debug!("Dropped an undecodable mesh response ({branch:?}): {err}");
                return;
            }
        };
        let pending = {
            let mut pending = self.pending.lock();
            let Some(entry) = pending.by_request.get(&frame.request_id) else {
                debug!(
                    "Unmatched mesh response {} ({branch:?}); the request timed out or was never ours",
                    frame.request_id.to_hex_string()
                );
                return;
            };
            if entry.link_id != link_id {
                debug!(
                    "Ignored a mesh response for {} that arrived on link {} instead of {}",
                    frame.request_id.to_hex_string(),
                    link_id.to_hex_string(),
                    entry.link_id.to_hex_string()
                );
                return;
            }
            pending.remove(&frame.request_id)
        };
        let Some(pending) = pending else {
            return;
        };
        debug!(
            "Correlated mesh response {} ({branch:?}, {} bytes)",
            frame.request_id.to_hex_string(),
            bytes.len()
        );
        // A response is proof of delivery too, should it overtake the transfer's own.
        if let Some(delivered) = pending.delivered {
            let _ = delivered.send(());
        }
        let _ = pending.reply.send(Ok((frame.data, branch)));
    }
}

/// Opens (or reuses) the link to `destination` and proves `identity` on it, both within
/// `link_timeout`. Exposed so a multi-round exchange can identify once and run every round
/// on the one link, as `LXMRouter` does with `request_receipt.link` (`LXMRouter.py:1535`).
pub(crate) async fn link_to(
    transport: &Transport,
    identity: &TransportIdentity,
    destination: &DestinationDesc,
    path: &str,
    link_timeout: Duration,
) -> Result<Arc<tokio::sync::Mutex<Link>>, R3Error> {
    let deadline = Deadline::after(link_timeout);
    let link = open_link(transport, destination, path, deadline).await?;
    identify(transport, &link, identity, path, deadline).await?;
    Ok(link)
}

/// Links to `destination` and waits for activation. A destination without a known path
/// fails at once rather than waiting out the deadline.
pub(crate) async fn open_link(
    transport: &Transport,
    destination: &DestinationDesc,
    path: &str,
    deadline: Deadline,
) -> Result<Arc<tokio::sync::Mutex<Link>>, R3Error> {
    let destination_hex = destination.address_hash.to_hex_string();
    if !deadline
        .bound(path, transport.has_path(&destination.address_hash))
        .await?
    {
        return Err(R3Error::LinkFailed(format!(
            "no known path to destination {destination_hex}"
        )));
    }
    let link = deadline.bound(path, transport.link(*destination)).await?;
    let activation = deadline
        .bound(
            path,
            await_link_activation(transport, &link, deadline.remaining()),
        )
        .await?;
    match activation {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::TimedOut => return Err(deadline.expired(path)),
        Err(err) => return Err(R3Error::LinkFailed(err.to_string())),
    }
    let link_id = deadline
        .bound(path, async { *link.lock().await.id() })
        .await?;
    debug!(
        "Mesh link {} to destination {destination_hex} is active",
        link_id.to_hex_string()
    );
    Ok(link)
}

/// Proves `identity` to the far end of `link`, which then sees `PeerIdentified`. `path` is
/// the request the proof is for, named by a `Timeout`.
pub(crate) async fn identify(
    transport: &Transport,
    link: &Arc<tokio::sync::Mutex<Link>>,
    identity: &TransportIdentity,
    path: &str,
    deadline: Deadline,
) -> Result<(), R3Error> {
    let (packet, link_id) = {
        let link = deadline.bound(path, link.lock()).await?;
        let payload = link.identify_payload(identity);
        let packet = link
            .identify_packet(&payload)
            .map_err(|err| R3Error::Send(format!("identify packet: {err}")))?;
        (packet, *link.id())
    };
    match deadline
        .bound(
            path,
            transport.send_link_packet_on_bound_iface(link, packet),
        )
        .await?
    {
        SendPacketOutcome::SentDirect => {
            debug!(
                "Sent mesh identify for {} on link {}",
                short(&identity.as_identity().address_hash.to_hex_string()),
                link_id.to_hex_string()
            );
            Ok(())
        }
        outcome => Err(R3Error::LinkFailed(format!(
            "identify packet not sent: {outcome:?}"
        ))),
    }
}
