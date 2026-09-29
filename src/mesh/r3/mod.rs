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
#[cfg(all(test, unix))]
pub(crate) use dispatch::LoggingKnockSink;
pub(crate) use dispatch::{
    AdmittedRequest, DispatchError, Dispatcher, Handler, KNOCK_PATH, KnockEvent, KnockSink,
    MESSAGE_PATH, STATUS_PATH, describe_path,
};
pub(crate) use error::{R3Error, RefusalCode};
pub(crate) use frame::{Envelope, MAX_R3_PAYLOAD_BYTES, NAME_HASH_LEN, OriginName};
#[cfg(test)]
pub(crate) use frame::{
    EnvelopeError, MAX_R3_NESTING_DEPTH, PathHash, RequestFrame, RequestId, ResponseFrame,
};
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

/// `text` with every maximal run of exactly 32 ASCII hex digits cut to `LOGGED_HASH_CHARS`,
/// for a log line that carries an error whose Display keeps a full hash for the user.
/// A Coyote message id is also exactly 32 hex digits and is allowed in a log line in full,
/// so a caller interpolates the id as its own placeholder and never folds it into the text
/// it hands here.
pub(crate) fn redact_hashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_hexdigit()) {
        let run_len = rest[start..]
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(rest.len() - start);
        let run = &rest[start..start + run_len];
        out.push_str(&rest[..start]);
        if run_len == 32 {
            out.push_str(short(run));
        } else {
            out.push_str(run);
        }
        rest = &rest[start + run_len..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    #[test]
    fn redact_hashes_cuts_only_runs_of_exactly_32_hex_digits() {
        let hash = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            redact_hashes(&format!("no known path to destination {hash} (2 tries)")),
            format!(
                "no known path to destination {} (2 tries)",
                &hash[..LOGGED_HASH_CHARS]
            )
        );
        assert_eq!(
            redact_hashes(&format!("{hash}:{hash}")),
            format!(
                "{}:{}",
                &hash[..LOGGED_HASH_CHARS],
                &hash[..LOGGED_HASH_CHARS]
            )
        );
        let longer = format!("{hash}00");
        assert_eq!(redact_hashes(&longer), longer);
        assert_eq!(redact_hashes(&hash[..31]), &hash[..31]);
        assert_eq!(redact_hashes("deadbeef and 42"), "deadbeef and 42");
        assert_eq!(redact_hashes(""), "");
    }

    #[test]
    fn redact_hashes_walks_multibyte_text_and_uppercase_hex() {
        let upper = "0123456789ABCDEF0123456789ABCDEF";
        assert_eq!(
            redact_hashes(&format!("p\u{e9}er {upper} \u{2192} ok")),
            format!("p\u{e9}er {} \u{2192} ok", &upper[..LOGGED_HASH_CHARS])
        );
        let lower = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            redact_hashes(&format!("\u{2014}{lower}")),
            format!("\u{2014}{}", &lower[..LOGGED_HASH_CHARS])
        );
        // A message id has the same shape, which is why ids are never folded into the text.
        let id = "fedcba9876543210fedcba9876543210";
        assert_eq!(redact_hashes(id), &id[..LOGGED_HASH_CHARS]);
    }
}
