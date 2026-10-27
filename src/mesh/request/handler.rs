use crate::mesh::request::error::RefusalCode;
use crate::mesh::request::frame::{PathHash, RequestId};
use crate::mesh::request::redact_hashes;

use async_trait::async_trait;
use parking_lot::RwLock;
use rmpv::Value;
use rns_transport::destination::link::LinkId;
use rns_transport::destination::request::{
    InboundRequest as LibraryInboundRequest, RequestHandler as LibraryRequestHandler,
    RequestResponse, ResponseFrame as LibraryResponseFrame,
};
use rns_transport::identity::Identity;
use rns_transport::resource::OutboundResult;
use rns_transport::transport::{SendPacketOutcome, Transport};
use std::future::Future;
use std::pin::Pin;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

/// Requests handled at once; any beyond that are dropped without a reply rather than queued,
/// so a flood from an authenticated peer costs one decoded frame and never a handler.
pub(crate) const MAX_CONCURRENT_INBOUND_REQUESTS: usize = 16;
/// Ceiling on one handler call. A handler that overruns it answers nothing.
pub(crate) const HANDLER_TIMEOUT: Duration = Duration::from_secs(20);

/// One decoded request as the handler sees it. `identity` is `Some` only once the peer has
/// proven itself on this link; nothing about the peer is inferred from the link alone.
pub(crate) struct InboundRequest {
    pub link_id: LinkId,
    pub identity: Option<Identity>,
    pub request_id: RequestId,
    pub path_hash: PathHash,
    pub data: Value,
}

/// What the handler answers with. `Silent` sends nothing, the way a `None` response does
/// in RNS `Link.handle_request`. `Settled` is a value whose producer must hear whether it
/// went out: `settlement.sent()` is called once the transport reports the reply
/// delivered, and the settlement is dropped unsent on every other outcome.
pub(crate) enum Reply {
    Value(Value),
    Code(RefusalCode),
    Silent,
    Settled {
        value: Value,
        settlement: Box<dyn Settlement>,
    },
}

/// The producer's side of a `Reply::Settled`. `sent` is called once the reply is known to
/// have gone out: a packet put on its interface, or a resource whose transfer the peer
/// proved complete. Dropping the settlement unsent means failure, whatever the cause: a
/// send error, a transfer that failed or was refused, or a handler that was cancelled or
/// timed out while it still held the reply. `sent` runs on an async task and must not
/// block; an unsent settlement is dropped on a blocking thread where the adapter can
/// arrange it, and wherever the dropped handler future happens to be otherwise, so a
/// `Drop` that writes to disk should stay small.
pub(crate) trait Settlement: Send {
    fn sent(self: Box<Self>);
}

/// Whether a request may reach `handle` at all.
pub(crate) enum Admission {
    Admit,
    Drop,
}

/// The single sink every inbound request is handed to. Routing by path and trust live
/// behind this seam, not in the transport. A `Reply::Value` whose body is a bare integer
/// in `0xf0..=0xfe` reads as a refusal code on the requesting side
/// (`RefusalCode::from_wire`), so a handler must not return one as a real value; the same
/// goes for a map shaped like the version refusal (`VersionRefusal::from_value`).
#[async_trait]
pub(crate) trait RequestHandler: Send + Sync {
    /// Runs before the body is read, with nothing but the link and its proven identity.
    /// It is synchronous on purpose: nothing here may wait on I/O.
    fn admit(&self, link_id: LinkId, identity: Option<&Identity>) -> Admission;
    async fn handle(&self, request: InboundRequest) -> Reply;
}

/// The handler the library's request layer calls for every registered path, in front of
/// Coyote's `RequestHandler`. It bounds the handler (the slots and `HANDLER_TIMEOUT`),
/// asks it whether to admit the request, and maps its reply onto the library's: a
/// `Reply::Value` or `Reply::Code` is handed back for the library to send, while a
/// `Reply::Settled` is sent here, since its producer must hear whether it went out.
///
/// The transport is held weakly: the destination owns this handler and the transport owns
/// the destination, so a strong reference would keep the transport alive past its last
/// owner. The weak reference is upgraded only for a settled reply's own send.
pub(crate) struct LibraryHandler {
    handler: RwLock<Arc<dyn RequestHandler>>,
    transport: Weak<Transport>,
    permits: Arc<Semaphore>,
    handler_timeout: Duration,
    #[cfg(test)]
    admitted: AtomicUsize,
}

impl LibraryHandler {
    pub(crate) fn new(handler: Arc<dyn RequestHandler>, transport: &Arc<Transport>) -> Self {
        Self {
            handler: RwLock::new(handler),
            transport: Arc::downgrade(transport),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_INBOUND_REQUESTS)),
            handler_timeout: HANDLER_TIMEOUT,
            #[cfg(test)]
            admitted: AtomicUsize::new(0),
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_handler_timeout_for_test(mut self, handler_timeout: Duration) -> Self {
        self.handler_timeout = handler_timeout;
        self
    }

    /// Replaces the handler behind the registered paths; requests already being handled
    /// finish with the handler they started with.
    #[cfg(all(test, unix))]
    pub(crate) fn set_handler(&self, handler: Arc<dyn RequestHandler>) {
        *self.handler.write() = handler;
    }

    /// How many requests `admit` let through to `handle`.
    #[cfg(all(test, unix))]
    pub(crate) fn admitted_count(&self) -> usize {
        self.admitted.load(Ordering::SeqCst)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn available_permits(&self) -> usize {
        self.permits.available_permits()
    }

    /// Runs Coyote's handler under one permit. `None` is a request the handler was never
    /// entered for or answers nothing to.
    async fn reply(
        &self,
        permit: OwnedSemaphorePermit,
        request: LibraryInboundRequest,
    ) -> Option<Reply> {
        let _permit = permit;
        let handler = self.handler.read().clone();
        let (link_id, request_id) = (request.link_id(), request.request_id());
        if let Admission::Drop = handler.admit(link_id, request.remote_identity()) {
            return None;
        }
        #[cfg(test)]
        self.admitted.fetch_add(1, Ordering::SeqCst);
        let inbound = InboundRequest {
            link_id,
            identity: request.remote_identity().copied(),
            request_id,
            path_hash: request.path_hash(),
            data: request.into_data(),
        };
        match timeout(self.handler_timeout, handler.handle(inbound)).await {
            Ok(reply) => Some(reply),
            Err(_) => {
                warn!(
                    "Mesh request {request_id} on link {} was not handled within {:?}; sent nothing",
                    link_id.to_hex_string(),
                    self.handler_timeout
                );
                None
            }
        }
    }

    /// Sends a settled reply the way the library sends its own: a packet when the frame
    /// fits the link MDU, a resource otherwise. The settlement is confirmed by the
    /// transfer outcome, which is awaited on a detached task: it is the transfer that is
    /// waited for, not the handler, so neither the handler slot nor `HANDLER_TIMEOUT`
    /// bounds it.
    async fn send_settled(
        &self,
        link_id: LinkId,
        request_id: RequestId,
        value: Value,
        settlement: Box<dyn Settlement>,
    ) {
        let Some(transport) = self.transport.upgrade() else {
            warn!(
                "Failed to send mesh response {request_id} on link {}: the transport is gone",
                link_id.to_hex_string()
            );
            drop_unsent(settlement);
            return;
        };
        let bytes = LibraryResponseFrame::new(request_id, value).encode();
        let Some(link) = transport.find_in_link(&link_id).await else {
            warn!(
                "Failed to send mesh response {request_id} on link {}: the link is gone",
                link_id.to_hex_string()
            );
            drop_unsent(settlement);
            return;
        };
        let mdu = link.lock().await.link_mdu();
        if bytes.len() <= mdu {
            debug!(
                "Sending mesh response {request_id} as a packet ({} bytes, link MDU {mdu})",
                bytes.len()
            );
            let packet = match link.lock().await.response_packet(&bytes) {
                Ok(packet) => packet,
                Err(err) => {
                    warn!(
                        "Failed to send mesh response {request_id} on link {}: response packet: {err}",
                        link_id.to_hex_string()
                    );
                    drop_unsent(settlement);
                    return;
                }
            };
            match transport
                .send_link_packet_on_bound_iface(&link, packet)
                .await
            {
                SendPacketOutcome::SentDirect => settlement.sent(),
                outcome => {
                    warn!(
                        "Failed to send mesh response {request_id} on link {}: response packet not sent: {outcome:?}",
                        link_id.to_hex_string()
                    );
                    drop_unsent(settlement);
                }
            }
            return;
        }
        debug!(
            "Sending mesh response {request_id} as a resource ({} bytes, link MDU {mdu})",
            bytes.len()
        );
        let sent = transport
            .send_response_resource_with_outcome(
                &link_id,
                request_id.to_bytes().to_vec(),
                bytes,
                None,
                true,
            )
            .await;
        match sent {
            Ok((_, outcome)) => {
                tokio::spawn(async move {
                    match outcome.await {
                        OutboundResult::Complete => settlement.sent(),
                        verdict => {
                            warn!(
                                "Mesh response {request_id} on link {} did not complete: {verdict:?}",
                                link_id.to_hex_string()
                            );
                            drop_unsent(settlement);
                        }
                    }
                });
            }
            Err(err) => {
                warn!(
                    "Failed to send mesh response {request_id} on link {}: response resource: {}",
                    link_id.to_hex_string(),
                    redact_hashes(&err.to_string())
                );
                drop_unsent(settlement);
            }
        }
    }
}

/// The settlement is dropped unsent off the async task: an implementor may settle its
/// accounts on disk, and that must not stall the request layer.
fn drop_unsent(settlement: Box<dyn Settlement>) {
    tokio::task::spawn_blocking(move || drop(settlement));
}

impl LibraryRequestHandler for LibraryHandler {
    /// A request is dropped, never queued, when every slot is busy. The permit covers
    /// admission and the handler; a settled reply's send runs after it is released.
    fn handle(
        &self,
        request: LibraryInboundRequest,
    ) -> Pin<Box<dyn Future<Output = Option<RequestResponse>> + Send + '_>> {
        Box::pin(async move {
            let Ok(permit) = self.permits.clone().try_acquire_owned() else {
                debug!(
                    "Dropped mesh request {} on link {}: all {MAX_CONCURRENT_INBOUND_REQUESTS} handler slots are busy",
                    request.request_id(),
                    request.link_id().to_hex_string()
                );
                return None;
            };
            let (link_id, request_id) = (request.link_id(), request.request_id());
            match self.reply(permit, request).await? {
                Reply::Value(value) => Some(RequestResponse::Value(value)),
                Reply::Code(code) => Some(RequestResponse::Value(code.to_wire())),
                Reply::Silent => None,
                Reply::Settled { value, settlement } => {
                    self.send_settled(link_id, request_id, value, settlement)
                        .await;
                    None
                }
            }
        })
    }
}
