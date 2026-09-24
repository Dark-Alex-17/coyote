//! Request/response over Reticulum links: the framing RNS `Link.request` and
//! `Link.handle_request` speak, the packet-or-resource size branches, request-id
//! correlation, and a handler seam for whatever answers requests. Nothing here decides who
//! may ask what.

mod client;
mod error;
mod frame;
mod server;
#[cfg(test)]
mod tests;

pub(crate) use client::{R3Client, RequestOptions, RequestOutcome};
pub(crate) use error::R3Error;
pub(crate) use server::{R3Server, RequestHandler};
