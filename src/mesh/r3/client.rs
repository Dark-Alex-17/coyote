use crate::mesh::r3::error::{R3Error, RefusalCode};
use crate::mesh::r3::frame::{MAX_R3_PAYLOAD_BYTES, RequestFrame, RequestId, ResponseFrame};

use parking_lot::Mutex;
use rmpv::Value;
use rns_transport::PacketContext;
use rns_transport::delivery::await_link_activation;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::link::{Link, LinkEvent, LinkEventData, LinkId};
use rns_transport::identity::PrivateIdentity as TransportIdentity;
use rns_transport::resource::{ResourceEvent, ResourceEventKind};
use rns_transport::transport::{SendPacketOutcome, Transport};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::io::ErrorKind;
use std::sync::Arc;
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

    fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    async fn bound<T>(self, path: &str, wait: impl Future<Output = T>) -> Result<T, R3Error> {
        timeout_at(self.at, wait)
            .await
            .map_err(|_| self.expired(path))
    }

    fn expired(self, path: &str) -> R3Error {
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

// Reached by the mesh dispatcher once it lands.
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
}

/// The requesting side: sends requests over out-links and correlates the responses that
/// arrive on the transport's out-link and resource event streams. One instance serves every
/// request a node makes; `run` must be spawned before the first request is sent.
pub(crate) struct R3Client {
    pending: Mutex<HashMap<RequestId, PendingRequest>>,
}

impl R3Client {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }

    /// Opens (or reuses) a link to `destination`, proves `identity` on it, and sends one
    /// request for `path`. `link_timeout` covers opening the link and identifying on it;
    /// `request_timeout` starts once that is done and covers sending the request as well as
    /// waiting for the response.
    pub(crate) async fn request(
        &self,
        transport: &Transport,
        identity: &TransportIdentity,
        destination: &DestinationDesc,
        path: &str,
        data: Value,
        options: RequestOptions,
    ) -> Result<RequestOutcome, R3Error> {
        let link_deadline = Deadline::after(options.link_timeout);
        let link = open_link(transport, destination, path, link_deadline).await?;
        identify(transport, &link, identity, path, link_deadline).await?;
        let request_deadline = Deadline::after(options.request_timeout);
        self.request_on_link(transport, &link, path, data, request_deadline)
            .await
    }

    /// Sends one request over an already active link. The branch is decided the way RNS
    /// `Link.request` decides it: by comparing the encoded frame against the link's MDU.
    /// Sending counts against `deadline` as much as waiting for the response does.
    pub(crate) async fn request_on_link(
        &self,
        transport: &Transport,
        link: &Arc<tokio::sync::Mutex<Link>>,
        path: &str,
        data: Value,
        deadline: Deadline,
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
        let (receiver, request_id, request_branch) = if packed.len() <= mdu {
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
            let receiver = self.insert_pending(request_id, link_id);
            let sent = deadline
                .bound(
                    path,
                    transport.send_link_packet_on_bound_iface(link, packet),
                )
                .await
                .and_then(|outcome| match outcome {
                    SendPacketOutcome::SentDirect => Ok(()),
                    outcome => Err(R3Error::LinkFailed(format!(
                        "request packet not sent: {outcome:?}"
                    ))),
                });
            if let Err(err) = sent {
                self.remove_pending(&request_id);
                return Err(err);
            }
            (receiver, request_id, SizeBranch::Packet)
        } else {
            let request_id = RequestId::of_packed(&packed);
            debug!(
                "Sending mesh request {} for {path} as a resource ({} bytes, link MDU {mdu})",
                request_id.to_hex_string(),
                packed.len()
            );
            let receiver = self.insert_pending(request_id, link_id);
            let sent = deadline
                .bound(
                    path,
                    transport.send_request_resource(&link_id, request_id.to_vec(), packed, None),
                )
                .await
                .and_then(|sent| {
                    sent.map(|_| ())
                        .map_err(|err| R3Error::Send(format!("request resource: {err}")))
                });
            if let Err(err) = sent {
                self.remove_pending(&request_id);
                return Err(err);
            }
            (receiver, request_id, SizeBranch::Resource)
        };

        let reply = match timeout_at(deadline.at, receiver).await {
            Ok(Ok(reply)) => reply,
            // The sender only goes away with the pending table itself.
            Ok(Err(_)) => Err(R3Error::Shutdown),
            Err(_) => {
                self.remove_pending(&request_id);
                Err(deadline.expired(path))
            }
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

    fn insert_pending(
        &self,
        request_id: RequestId,
        link_id: LinkId,
    ) -> oneshot::Receiver<Correlated> {
        let (reply, receiver) = oneshot::channel();
        self.pending
            .lock()
            .insert(request_id, PendingRequest { link_id, reply });
        receiver
    }

    fn remove_pending(&self, request_id: &RequestId) -> Option<PendingRequest> {
        self.pending.lock().remove(request_id)
    }

    fn fail_all(&self, error: R3Error) {
        let drained = std::mem::take(&mut *self.pending.lock());
        for (_, pending) in drained {
            let _ = pending.reply.send(Err(error.clone()));
        }
    }

    fn on_link_event(&self, event: LinkEventData) {
        match event.event {
            LinkEvent::Data(payload) if payload.context() == PacketContext::Response => {
                self.deliver(event.id, payload.as_slice(), SizeBranch::Packet);
            }
            LinkEvent::Closed => {
                let closed: Vec<PendingRequest> = self
                    .pending
                    .lock()
                    .extract_if(|_, entry| entry.link_id == event.id)
                    .map(|(_, entry)| entry)
                    .collect();
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
        if let ResourceEventKind::Complete(complete) = event.kind
            && complete.is_response
        {
            self.deliver(event.link_id, &complete.data, SizeBranch::Resource);
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
        let pending = match self.pending.lock().entry(frame.request_id) {
            Entry::Occupied(entry) if entry.get().link_id == link_id => entry.remove(),
            Entry::Occupied(entry) => {
                debug!(
                    "Ignored a mesh response for {} that arrived on link {} instead of {}",
                    frame.request_id.to_hex_string(),
                    link_id.to_hex_string(),
                    entry.get().link_id.to_hex_string()
                );
                return;
            }
            Entry::Vacant(_) => {
                debug!(
                    "Unmatched mesh response {} ({branch:?}); the request timed out or was never ours",
                    frame.request_id.to_hex_string()
                );
                return;
            }
        };
        debug!(
            "Correlated mesh response {} ({branch:?}, {} bytes)",
            frame.request_id.to_hex_string(),
            bytes.len()
        );
        let _ = pending.reply.send(Ok((frame.data, branch)));
    }
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
                identity.as_identity().address_hash.to_hex_string(),
                link_id.to_hex_string()
            );
            Ok(())
        }
        outcome => Err(R3Error::LinkFailed(format!(
            "identify packet not sent: {outcome:?}"
        ))),
    }
}
