use crate::mesh::r3::client::SizeBranch;
use crate::mesh::r3::error::{R3Error, RefusalCode};
use crate::mesh::r3::frame::{
    MAX_R3_PAYLOAD_BYTES, PathHash, RequestFrame, RequestId, ResponseFrame,
};

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use rmpv::Value;
use rns_transport::PacketContext;
use rns_transport::destination::link::{LinkEvent, LinkEventData, LinkId};
use rns_transport::identity::Identity;
use rns_transport::resource::{ResourceComplete, ResourceEvent, ResourceEventKind};
use rns_transport::transport::{SendPacketOutcome, Transport};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Ceiling on sending one response, covering the link lookup and lock as well as the send.
pub(crate) const DEFAULT_RESPONSE_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// One decoded request as the handler sees it. `identity` is `Some` only once the peer has
/// proven itself on this link; nothing about the peer is inferred from the link alone.
/// `requested_at` is the peer's timestamp verbatim (`time.time()` in RNS), so it may be NaN,
/// infinite or far from this node's clock; clamp it before using it for freshness.
// Reached by the mesh dispatcher once it lands.
#[allow(dead_code)]
pub(crate) struct InboundRequest {
    pub link_id: LinkId,
    pub identity: Option<Identity>,
    pub request_id: RequestId,
    pub path_hash: PathHash,
    pub requested_at: f64,
    pub data: Value,
    pub branch: SizeBranch,
}

/// What the handler answers with. `Silent` sends nothing, the way a `None` response does
/// in RNS `Link.handle_request`.
// Reached by the mesh dispatcher once it lands.
#[allow(dead_code)]
pub(crate) enum Reply {
    Value(Value),
    Code(RefusalCode),
    Silent,
}

/// The single sink every inbound request is handed to. Routing by path, trust and
/// unknown-path policy live behind this seam, not in the transport. A `Reply::Value` whose
/// body is a bare integer in `0xf0..=0xfe` reads as a refusal code on the requesting side
/// (`RefusalCode::from_wire`), so a handler must not return one as a real value.
#[async_trait]
pub(crate) trait RequestHandler: Send + Sync {
    async fn handle(&self, request: InboundRequest) -> Reply;
}

/// The responding side: decodes requests arriving on in-links, hands them to the installed
/// handler, and sends the reply on whichever branch its size calls for. The only per-peer
/// state it keeps is the identity table, and that is written from `PeerIdentified` alone.
pub(crate) struct R3Server {
    handler: RwLock<Option<Arc<dyn RequestHandler>>>,
    identified: Mutex<HashMap<LinkId, Identity>>,
}

impl R3Server {
    pub(crate) fn new() -> Self {
        Self {
            handler: RwLock::new(None),
            identified: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn set_handler(&self, handler: Arc<dyn RequestHandler>) {
        *self.handler.write() = Some(handler);
    }

    #[cfg(test)]
    pub(crate) fn identified_peer_count(&self) -> usize {
        self.identified.lock().len()
    }

    /// Serves requests until `cancel` fires or the transport goes away, then waits for the
    /// handlers still running. Each of those watches `cancel` too, through the reply send as
    /// well as the handler, so the wait ends when they notice it.
    pub(crate) async fn run(
        self: Arc<Self>,
        transport: Arc<Transport>,
        mut link_events: broadcast::Receiver<LinkEventData>,
        mut resource_events: broadcast::Receiver<ResourceEvent>,
        cancel: CancellationToken,
    ) {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                event = link_events.recv() => match event {
                    Ok(event) => {
                        if let Some(handling) = self.on_link_event(&transport, event, &cancel) {
                            tasks.spawn(handling);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("Mesh request server fell behind and skipped {skipped} link events");
                        let server = self.clone();
                        let transport = transport.clone();
                        tasks.spawn(async move { server.forget_closed_links(&transport).await });
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                event = resource_events.recv() => match event {
                    Ok(event) => {
                        if let Some(handling) = self.on_resource_event(&transport, event, &cancel) {
                            tasks.spawn(handling);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("Mesh request server fell behind and skipped {skipped} resource events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        while tasks.join_next().await.is_some() {}
    }

    /// Drops identities whose link the transport no longer has; a `Closed` event lost to a
    /// lag would otherwise leave them in the table for good. Runs as a task like the
    /// handlers do, since each lookup waits on the transport.
    async fn forget_closed_links(&self, transport: &Transport) {
        let known: Vec<LinkId> = self.identified.lock().keys().copied().collect();
        for link_id in known {
            if transport.find_in_link(&link_id).await.is_none()
                && self.identified.lock().remove(&link_id).is_some()
            {
                debug!(
                    "Forgot the identity on mesh link {} that closed while the server lagged",
                    link_id.to_hex_string()
                );
            }
        }
    }

    fn on_link_event(
        &self,
        transport: &Arc<Transport>,
        event: LinkEventData,
        cancel: &CancellationToken,
    ) -> Option<impl Future<Output = ()> + Send + use<>> {
        let link_id = event.id;
        match event.event {
            LinkEvent::PeerIdentified(identity) => {
                debug!(
                    "Mesh peer {} identified on link {}",
                    identity.address_hash.to_hex_string(),
                    link_id.to_hex_string()
                );
                self.identified.lock().insert(link_id, *identity);
                None
            }
            LinkEvent::Closed => {
                if self.identified.lock().remove(&link_id).is_some() {
                    debug!(
                        "Forgot the identity on closed mesh link {}",
                        link_id.to_hex_string()
                    );
                }
                None
            }
            LinkEvent::Data(payload) if payload.context() == PacketContext::Request => {
                // Upstream derives this from the packet hash, exactly as the requester did.
                let Some(request_id) = payload.request_id() else {
                    debug!(
                        "Dropped a mesh request packet without a request id on link {}",
                        link_id.to_hex_string()
                    );
                    return None;
                };
                self.dispatch(
                    transport,
                    link_id,
                    RequestId::from(request_id),
                    payload.as_slice(),
                    SizeBranch::Packet,
                    cancel,
                )
            }
            LinkEvent::Activated | LinkEvent::Data(_) => None,
        }
    }

    fn on_resource_event(
        &self,
        transport: &Arc<Transport>,
        event: ResourceEvent,
        cancel: &CancellationToken,
    ) -> Option<impl Future<Output = ()> + Send + use<>> {
        let ResourceEventKind::Complete(complete) = event.kind else {
            return None;
        };
        if !complete.is_request {
            return None;
        }
        let request_id = resource_request_id(&complete);
        self.dispatch(
            transport,
            event.link_id,
            request_id,
            &complete.data,
            SizeBranch::Resource,
            cancel,
        )
    }

    /// Decodes one request and returns the handler task for the loop to spawn, so a
    /// handler that stalls does not stall the event loop; RNS runs each in a thread. The
    /// size check here is where inbound requests are capped: after assembly, on our side,
    /// since the destination carries no `max_request_size` (the upstream reject path
    /// deadlocks the transport, rev 3ed5932) and the upstream 32 MiB advertisement cap is
    /// the only bound before that.
    fn dispatch(
        &self,
        transport: &Arc<Transport>,
        link_id: LinkId,
        request_id: RequestId,
        packed: &[u8],
        branch: SizeBranch,
        cancel: &CancellationToken,
    ) -> Option<impl Future<Output = ()> + Send + use<>> {
        if packed.len() > MAX_R3_PAYLOAD_BYTES {
            debug!(
                "Dropped an oversize mesh request on link {} ({} bytes, max {MAX_R3_PAYLOAD_BYTES})",
                link_id.to_hex_string(),
                packed.len()
            );
            return None;
        }
        let frame = match RequestFrame::decode(packed) {
            Ok(frame) => frame,
            Err(err) => {
                debug!(
                    "Dropped an undecodable mesh request {} on link {}: {err}",
                    request_id.to_hex_string(),
                    link_id.to_hex_string()
                );
                return None;
            }
        };
        let Some(handler) = self.handler.read().clone() else {
            debug!(
                "Dropped mesh request {} on link {}: no request handler installed",
                request_id.to_hex_string(),
                link_id.to_hex_string()
            );
            return None;
        };
        let request = InboundRequest {
            link_id,
            identity: self.identified.lock().get(&link_id).copied(),
            request_id,
            path_hash: frame.path_hash,
            requested_at: frame.time,
            data: frame.data,
            branch,
        };
        debug!(
            "Received mesh request {} on link {} ({branch:?}, {} bytes)",
            request_id.to_hex_string(),
            link_id.to_hex_string(),
            packed.len()
        );
        let transport = transport.clone();
        let cancel = cancel.clone();
        Some(async move {
            let serve = async {
                let value = match handler.handle(request).await {
                    Reply::Value(value) => value,
                    Reply::Code(code) => code.to_wire(),
                    Reply::Silent => return,
                };
                match timeout(
                    DEFAULT_RESPONSE_SEND_TIMEOUT,
                    respond(&transport, link_id, request_id, value),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => warn!(
                        "Failed to send mesh response {} on link {}: {err}",
                        request_id.to_hex_string(),
                        link_id.to_hex_string()
                    ),
                    Err(_) => warn!(
                        "Mesh response {} on link {} was not sent within {}s; gave up on it",
                        request_id.to_hex_string(),
                        link_id.to_hex_string(),
                        DEFAULT_RESPONSE_SEND_TIMEOUT.as_secs()
                    ),
                }
            };
            tokio::select! {
                () = cancel.cancelled() => {}
                () = serve => {}
            }
        })
    }
}

/// The id of a request that arrived as a resource: recomputed from the assembled bytes the
/// way RNS `Link.request_resource_concluded` does, rather than trusted from the advertisement.
fn resource_request_id(complete: &ResourceComplete) -> RequestId {
    let request_id = RequestId::of_packed(&complete.data);
    if let Some(advertised) = &complete.request_id
        && advertised.as_slice() != request_id.as_bytes()
    {
        debug!(
            "Mesh request resource advertised id {} but its bytes hash to {}",
            advertised
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            request_id.to_hex_string()
        );
    }
    request_id
}

/// Encodes and sends one response, as a packet when it fits the link MDU and as a resource
/// otherwise, the decision RNS `Link.handle_request` makes.
async fn respond(
    transport: &Transport,
    link_id: LinkId,
    request_id: RequestId,
    value: Value,
) -> Result<(), R3Error> {
    let bytes = ResponseFrame {
        request_id,
        data: value,
    }
    .encode();
    if bytes.len() > MAX_R3_PAYLOAD_BYTES {
        return Err(R3Error::Oversize {
            len: bytes.len(),
            max: MAX_R3_PAYLOAD_BYTES,
        });
    }
    let link = transport
        .find_in_link(&link_id)
        .await
        .ok_or(R3Error::LinkClosed)?;
    let mdu = link.lock().await.link_mdu();
    if bytes.len() <= mdu {
        debug!(
            "Sending mesh response {} as a packet ({} bytes, link MDU {mdu})",
            request_id.to_hex_string(),
            bytes.len()
        );
        let packet = link
            .lock()
            .await
            .response_packet(&bytes)
            .map_err(|err| R3Error::Send(format!("response packet: {err}")))?;
        match transport
            .send_link_packet_on_bound_iface(&link, packet)
            .await
        {
            SendPacketOutcome::SentDirect => Ok(()),
            outcome => Err(R3Error::LinkFailed(format!(
                "response packet not sent: {outcome:?}"
            ))),
        }
    } else {
        debug!(
            "Sending mesh response {} as a resource ({} bytes, link MDU {mdu})",
            request_id.to_hex_string(),
            bytes.len()
        );
        transport
            .send_response_resource(&link_id, request_id.to_vec(), bytes, None)
            .await
            .map_err(|err| R3Error::Send(format!("response resource: {err}")))?;
        Ok(())
    }
}
