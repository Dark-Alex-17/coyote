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

pub(crate) use client::{R3Client, RequestOptions, RequestOutcome};
pub(crate) use dispatch::{Dispatcher, LoggingKnockSink};
pub(crate) use error::R3Error;
pub(crate) use frame::{Envelope, NAME_HASH_LEN, OriginName};
pub(crate) use receipt::RequestReceipt;
pub(crate) use server::R3Server;
#[cfg(test)]
pub(crate) use server::RequestHandler;
