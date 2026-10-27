use crate::mesh::request::error::RequestError;
use crate::mesh::request::short;

use rns_transport::delivery::await_link_activation;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::link::Link;
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
