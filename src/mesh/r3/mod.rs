//! Request/response over Reticulum links: the framing RNS `Link.request` and
//! `Link.handle_request` speak, the packet-or-resource size branches, request-id
//! correlation, a handler seam for whatever answers requests, and the dispatcher that
//! decides, from the trust list alone, who gets an answer at all.

mod client;
mod dispatch;
mod error;
mod frame;
mod receipt;
mod server;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use client::DEFAULT_REQUEST_TIMEOUT;
pub(crate) use client::{
    DEFAULT_LINK_TIMEOUT, Deadline, R3Client, RequestOptions, RequestOutcome, SizeBranch, link_to,
    open_link,
};
#[cfg(test)]
pub(crate) use dispatch::LoggingKnockSink;
pub(crate) use dispatch::{
    AdmittedRequest, DispatchError, Dispatcher, Handler, KNOCK_PATH, KnockEvent, KnockSink,
    MESSAGE_PATH, STATUS_PATH, describe_path,
};
pub(crate) use error::{R3Error, RefusalCode};
pub(crate) use frame::{Envelope, MAX_R3_PAYLOAD_BYTES, NAME_HASH_LEN, OriginName};
#[cfg(test)]
pub(crate) use frame::{EnvelopeError, PathHash, RequestFrame, RequestId, ResponseFrame};
pub(crate) use receipt::RequestReceipt;
#[cfg(test)]
pub(crate) use server::{
    Admission, DEFAULT_RESPONSE_SEND_TIMEOUT, HANDLER_TIMEOUT, InboundRequest,
    MAX_CONCURRENT_INBOUND_REQUESTS, PEER_RESOLVE_TIMEOUT, RequestHandler,
};
pub(crate) use server::{R3Server, Reply};
#[cfg(all(test, unix))]
pub(crate) use tests::network;

/// How much of a hash the logs show.
pub(crate) const LOGGED_HASH_CHARS: usize = 8;

pub(crate) fn short(hash: &str) -> &str {
    hash.get(..LOGGED_HASH_CHARS).unwrap_or(hash)
}
