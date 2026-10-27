use crate::mesh::protocol::VersionRefusal;
use crate::mesh::request::dispatch::FETCH_PATH;
use crate::mesh::request::error::{RefusalCode, RequestError};
use crate::mesh::request::frame::{Envelope, MAX_FETCH_RESPONSE_BYTES, MAX_REQUEST_PAYLOAD_BYTES};
use crate::mesh::request::short;

use rmpv::Value;
use rns_transport::delivery::await_link_activation;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::link::Link;
use rns_transport::destination::request::{
    RequestError as LibraryRequestError, RequestOptions as LibraryRequestOptions,
};
use rns_transport::error::RnsError;
use rns_transport::identity::PrivateIdentity as TransportIdentity;
use rns_transport::transport::{SendPacketOutcome, Transport};
use std::future::Future;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

/// Ceiling on waiting for a response. RNS derives its default from the link RTT
/// (`rtt * 6 + 11.25s`); a fixed bound is simpler and enough for a LAN or a relay hop.
pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on link establishment, before the request itself is sent.
pub(crate) const DEFAULT_LINK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// error reports. Every wait against the transport or a link lock runs under one.
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
    ) -> Result<T, RequestError> {
        timeout_at(self.at, wait)
            .await
            .map_err(|_| self.expired(path))
    }

    pub(crate) fn expired(self, path: &str) -> RequestError {
        RequestError::Timeout {
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

/// `link_timeout` covers opening the link and identifying on it; `request_timeout` starts
/// once that is done and covers sending the request as well as waiting for the response.
///
/// Links are reused per destination for as long as upstream keeps them: its watchdog closes
/// an idle link and nothing here closes one sooner. The identity is proven on every request
/// regardless; it is one packet, and it also heals a responder that missed the first proof.
pub(crate) async fn request(
    transport: &Transport,
    identity: &TransportIdentity,
    destination: &DestinationDesc,
    path: &str,
    envelope: Envelope,
    options: RequestOptions,
) -> Result<Value, RequestError> {
    let link = link_to(transport, identity, destination, path, options.link_timeout).await?;
    request_on_link(
        transport,
        &link,
        path,
        envelope.into_value(),
        options.request_timeout,
    )
    .await
}

/// Sends `data` as the request body verbatim, with no envelope, over an already active
/// link. Coyote-to-Coyote requests wrap the envelope first. The LXMF `/get` conversation
/// does not: a propagation node reads the body as the bare `[wants, haves, limit]` arrays
/// of `LXMRouter.message_get_request` (`LXMRouter.py:1436-1475`), so wrapping it in the
/// envelope would make every round unreadable to it. Tests use the same path to put a body
/// of their own choosing, malformed ones included, in front of a responder.
///
/// The request is capped before anything touches the wire; the response cap is the
/// library's, armed per path. Sending counts against `request_timeout` as much as waiting
/// for the response does. The library runs the same timeout on the receipt, but its clock
/// pauses while a response Resource is being received, so the deadline here is the ceiling
/// a peer that trickles its response meets. Dropping the returned future once the request
/// is sent cancels nothing: the responder still serves it and the library's own timer
/// retires the entry.
pub(crate) async fn request_on_link(
    transport: &Transport,
    link: &Arc<tokio::sync::Mutex<Link>>,
    path: &str,
    data: Value,
    request_timeout: Duration,
) -> Result<Value, RequestError> {
    let packed = Link::request_payload(path, data.clone())
        .map_err(|err| RequestError::Send(format!("request frame: {err}")))?
        .packed;
    if packed.len() > MAX_REQUEST_PAYLOAD_BYTES {
        return Err(RequestError::Oversize {
            len: packed.len(),
            max: MAX_REQUEST_PAYLOAD_BYTES,
        });
    }
    let deadline = Deadline::after(request_timeout);
    let (link_id, mdu) = deadline
        .bound(path, async {
            let link = link.lock().await;
            (*link.id(), link.link_mdu())
        })
        .await?;
    let max = if path == FETCH_PATH {
        MAX_FETCH_RESPONSE_BYTES
    } else {
        MAX_REQUEST_PAYLOAD_BYTES
    };
    let opts = LibraryRequestOptions::default()
        .with_timeout(request_timeout)
        .with_max_response_size(max);
    let receipt = deadline
        .bound(path, transport.request(&link_id, path, data, opts))
        .await?
        .map_err(|err| RequestError::LinkFailed(format!("request not sent: {err}")))?;
    debug!(
        "Sending mesh request {} for {path} as a {} ({} bytes, link MDU {mdu})",
        receipt.request_id(),
        if packed.len() <= mdu {
            "packet"
        } else {
            "resource"
        },
        packed.len()
    );
    let response = match deadline.bound(path, receipt.response()).await? {
        Ok(response) => response,
        Err(LibraryRequestError::Timeout) => {
            return Err(RequestError::Timeout {
                path: path.to_string(),
                after: request_timeout,
            });
        }
        Err(LibraryRequestError::LinkClosed) => return Err(RequestError::LinkClosed),
        Err(err @ LibraryRequestError::Rejected) => {
            return Err(RequestError::Send(err.to_string()));
        }
        Err(LibraryRequestError::Oversize) => {
            // The library never reports the size of a response it refuses, whether at the
            // advertisement or after assembly, so one over the cap stands for more than
            // the cap.
            return Err(RequestError::Oversize { len: max + 1, max });
        }
        Err(LibraryRequestError::Decode) => {
            return Err(RequestError::Decode("response frame".to_string()));
        }
        Err(err @ LibraryRequestError::Send) => return Err(RequestError::Send(err.to_string())),
        Err(err) => return Err(RequestError::LinkFailed(err.to_string())),
    };
    let value = response.into_value().ok_or_else(|| {
        RequestError::Decode("response carried a file body, not a value".to_string())
    })?;
    if let Some(code) = RefusalCode::from_wire(&value) {
        return Err(RequestError::Refused(code));
    }
    if let Some(refusal) = VersionRefusal::from_value(&value) {
        return Err(RequestError::UnsupportedVersion {
            found: refusal.found,
            min: refusal.min,
            max: refusal.max,
        });
    }
    Ok(value)
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
) -> Result<Arc<tokio::sync::Mutex<Link>>, RequestError> {
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
) -> Result<Arc<tokio::sync::Mutex<Link>>, RequestError> {
    let destination_hex = destination.address_hash.to_hex_string();
    let link = match deadline.bound(path, transport.link(*destination)).await? {
        Ok(link) => link,
        Err(RnsError::NoPath) => {
            return Err(RequestError::LinkFailed(format!(
                "no known path to destination {destination_hex}"
            )));
        }
        Err(err) => {
            return Err(RequestError::LinkFailed(format!(
                "no link to destination {destination_hex}: {err}"
            )));
        }
    };
    let activation = deadline
        .bound(
            path,
            await_link_activation(transport, &link, deadline.remaining()),
        )
        .await?;
    match activation {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::TimedOut => return Err(deadline.expired(path)),
        Err(err) => return Err(RequestError::LinkFailed(err.to_string())),
    }
    let link_id = deadline
        .bound(path, async { *link.lock().await.id() })
        .await?;
    debug!(
        "Mesh link {} to destination {} is active",
        link_id.to_hex_string(),
        short(&destination_hex)
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
) -> Result<(), RequestError> {
    let (packet, link_id) = {
        let link = deadline.bound(path, link.lock()).await?;
        let payload = link.identify_payload(identity);
        let packet = link
            .identify_packet(&payload)
            .map_err(|err| RequestError::Send(format!("identify packet: {err}")))?;
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
        outcome => Err(RequestError::LinkFailed(format!(
            "identify packet not sent: {outcome:?}"
        ))),
    }
}
