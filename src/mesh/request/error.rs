use crate::mesh::card::DISPLAY_NAME_MAX_CHARS;
use crate::mesh::display_text;
use crate::mesh::protocol::describe_version;
use crate::mesh::request::short;

use serde::Serialize;
use serde::ser::SerializeStruct;
use std::fmt;
use std::time::Duration;

/// Callers match on this, so it is a closed set rather than `anyhow`; `?` into an
/// `anyhow::Result` still works through `std::error::Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestError {
    /// No response arrived within `after`; the link may still be up.
    Timeout { path: String, after: Duration },
    /// The link could not be established or the packet could not be put on its interface.
    LinkFailed(String),
    /// The link closed while the request was pending, or was gone by the time we replied.
    LinkClosed,
    /// The encoded request frame exceeds `MAX_REQUEST_PAYLOAD_BYTES` and was not sent at
    /// all, or the peer's response exceeds the path's response cap and the transport
    /// refused it before assembly.
    Oversize { len: usize, max: usize },
    /// Bytes that are not a well-formed request or response frame.
    Decode(String),
    /// The transport refused to build or hand over the packet or resource.
    Send(String),
    /// The responder answered with one of the LXMF refusal sentinels instead of a body.
    Refused(RefusalCode),
    /// One side does not speak the other's protocol version. `found` is the version the
    /// refusing side saw from the other party (`None` when it saw none it could read) and
    /// `min..=max` is the refusing side's window: from the wire the refusing side is the
    /// peer, so `max` is the newest protocol it speaks; from this node's own pre-flight
    /// gate it is this node, and `found` is what the peer announced.
    UnsupportedVersion {
        found: Option<u16>,
        min: u16,
        max: u16,
    },
    /// The mesh node has been stopped.
    NotRunning,
    /// The node stopped while the request was pending.
    Shutdown,
}

impl RequestError {
    /// `true` when the peer answered `NoAccess`, the one refusal byte every refused rule
    /// shares, so the request was heard and turned away rather than lost. The wire does not
    /// say which rule: the peer's owner lifts a default-closed refusal with `.mesh trust`
    /// and a block of this instance with `.mesh unblock`; a refusal after this identity's
    /// key change is lifted the same way as the first, by trusting the new destination.
    pub(crate) fn is_access_refusal(&self) -> bool {
        matches!(self, Self::Refused(RefusalCode::NoAccess))
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { path, after } => write!(
                f,
                "No response to the mesh request {path} within {:.1}s",
                after.as_secs_f64()
            ),
            Self::LinkFailed(reason) => write!(f, "The mesh link failed: {reason}"),
            Self::LinkClosed => write!(f, "The mesh link closed before the request completed"),
            Self::Oversize { len, max } => write!(
                f,
                "The mesh request payload is {len} bytes, above the {max}-byte limit"
            ),
            Self::Decode(reason) => write!(f, "Malformed mesh request frame: {reason}"),
            Self::Send(reason) => write!(f, "The mesh transport could not send: {reason}"),
            Self::Refused(code) => write!(f, "The mesh peer refused the request: {code}"),
            Self::UnsupportedVersion { found, min, max } => write!(
                f,
                "Protocol version {} is outside the supported range {min}..={max}",
                describe_version(*found)
            ),
            Self::NotRunning => write!(
                f,
                "The mesh node has been stopped; run `.mesh on` to start it again"
            ),
            Self::Shutdown => write!(f, "The mesh node stopped while the request was pending"),
        }
    }
}

impl std::error::Error for RequestError {}

/// The refusal sentinels a Reticulum responder returns as the bare response value instead
/// of a body. They are LXMF's `LXMPeer.ERROR_*` constants (`LXMPeer.py`, lines 24-31;
/// `ERROR_NO_IDENTITY = 0xf0`, `ERROR_NO_ACCESS = 0xf1`): a propagation node returns them
/// from its request handlers (`LXMRouter.py:1428-1429`) and the requesting router compares
/// the response against them before looking for a body (`LXMRouter.py:1508,1514`). On the
/// wire each is a msgpack unsigned integer (`0xcc` + one byte, since all are above 127).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RefusalCode {
    NoIdentity = 0xf0,
    NoAccess = 0xf1,
    InvalidKey = 0xf3,
    InvalidData = 0xf4,
    InvalidStamp = 0xf5,
    Throttled = 0xf6,
    NotFound = 0xfd,
    Timeout = 0xfe,
}

impl RefusalCode {
    /// `Some` when `value` is an integer equal to one of the codes; any other value,
    /// including other integers, is a body.
    pub(crate) fn from_wire(value: &rmpv::Value) -> Option<Self> {
        let code = match value.as_u64()? {
            0xf0 => Self::NoIdentity,
            0xf1 => Self::NoAccess,
            0xf3 => Self::InvalidKey,
            0xf4 => Self::InvalidData,
            0xf5 => Self::InvalidStamp,
            0xf6 => Self::Throttled,
            0xfd => Self::NotFound,
            0xfe => Self::Timeout,
            _ => return None,
        };
        Some(code)
    }

    pub(crate) fn to_wire(self) -> rmpv::Value {
        rmpv::Value::from(self as u8)
    }

    /// `true` for the two sentinels a responder returns when it declines to serve the
    /// requester at all (`NoIdentity`, `NoAccess`), as opposed to faulting what it sent;
    /// another responder may well take the same request.
    pub(crate) fn declines_the_requester(self) -> bool {
        matches!(self, Self::NoIdentity | Self::NoAccess)
    }
}

/// The next step after a request failed over the link, for the `Direct` arms of the
/// send, knock and access errors. A link that failed, timed out or closed is the peer
/// being out of reach for the moment, so a retry is the step; a frame over the limit
/// wants a smaller message; a reply this Coyote could not read, or a version neither
/// side speaks, names the other Coyote; a transport fault is this build's to report.
/// The callers route most of these elsewhere before wrapping (the link failures to a
/// propagation fallback, `NotRunning` and `Shutdown` to their own arm), so those rows
/// serve only a caller that wraps without the remap.
pub(crate) fn request_remedy(err: &RequestError) -> &'static str {
    match err {
        RequestError::Timeout { .. } | RequestError::LinkFailed(_) | RequestError::LinkClosed => {
            "Try again in a minute; `.mesh peers` shows when the peer was last heard"
        }
        RequestError::Oversize { .. } => {
            "Shorten the message or send fewer parts so the frame fits the limit, then send it again"
        }
        RequestError::Decode(_) => {
            "The peer answered in a form this Coyote could not read; it may run a different Coyote"
        }
        RequestError::Send(_) => "Try again; if it repeats, report it",
        RequestError::Refused(code) => refusal_remedy(*code),
        RequestError::UnsupportedVersion { .. } => {
            "One of the two needs upgrading before they can talk; `.mesh peers` names which"
        }
        RequestError::NotRunning | RequestError::Shutdown => "Run `.mesh on`, then try again",
    }
}

/// The next step after a peer refused a request with `code`. A throttled or timed-out
/// peer takes the same request later; a peer that could not read what was sent meets a
/// retry with the same answer; a name it does not know wants checking. The two codes
/// that decline the requester are lifted only on the peer's side; no caller wraps
/// `NoAccess` (the send and access paths turn it into a `RemoteRefusal`, the knock
/// counts it as heard), so that row serves only a code that slipped past.
pub(crate) fn refusal_remedy(code: RefusalCode) -> &'static str {
    match code {
        RefusalCode::Throttled => "Wait a minute and try again",
        RefusalCode::Timeout => "Try again in a minute",
        RefusalCode::NoIdentity | RefusalCode::NoAccess => {
            "The peer has not trusted this instance; nothing on this side lifts that"
        }
        RefusalCode::InvalidKey | RefusalCode::InvalidData | RefusalCode::InvalidStamp => {
            "The peer could not read what was sent, so sending it again meets the same answer; report it"
        }
        RefusalCode::NotFound => "The peer has nothing under that name; check it and try again",
    }
}

impl fmt::Display for RefusalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::NoIdentity => "no identity",
            Self::NoAccess => "no access",
            Self::InvalidKey => "invalid key",
            Self::InvalidData => "invalid data",
            Self::InvalidStamp => "invalid stamp",
            Self::Throttled => "throttled",
            Self::NotFound => "not found",
            Self::Timeout => "timeout",
        };
        write!(f, "{name} (0x{:02x})", *self as u8)
    }
}

/// A peer's `NoAccess` answer to a request this node's own trust gate let through. The
/// peer knows this identity but has not trusted the instance that asked, so the remedy is
/// on its side, and every surface (REPL, hook, tool result) says so with the same words.
/// The display name is the only peer-written text; `new` cleans it, and the fields stay
/// private so nothing else can file a raw one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteRefusal {
    destination: String,
    display_name: Option<String>,
    own_destination: String,
}

impl RemoteRefusal {
    /// `display_name` is the peer's announced text as the table filed it.
    pub(crate) fn new(
        destination: &str,
        display_name: Option<&str>,
        own_destination: String,
    ) -> Self {
        Self {
            destination: destination.to_string(),
            display_name: display_name.and_then(|name| display_text(name, DISPLAY_NAME_MAX_CHARS)),
            own_destination,
        }
    }

    /// What the model can still do about it, for a tool result's `next_action`. A
    /// default-closed refusal already filed the request as a knock on the peer, so nothing
    /// here is left to send; a peer that blocked this instance, or one that saw this
    /// identity's key change, filed none, and the wire does not say which it was.
    pub(crate) fn next_action(&self) -> String {
        format!(
            "Nothing on this node unblocks it: ask the person at {} to run .mesh trust {}; unless that peer blocked this instance or saw this identity's key change, the request already reached them as a knock (.mesh knocks on their side).",
            short(&self.destination),
            self.own_destination
        )
    }
}

/// As a tool result carries it: the `kind` a tool gives any refusal, the sentence, and
/// what the model can still do.
impl Serialize for RemoteRefusal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RemoteRefusal", 3)?;
        state.serialize_field("kind", "refused")?;
        state.serialize_field("message", &self.to_string())?;
        state.serialize_field("next_action", &self.next_action())?;
        state.end()
    }
}

/// The name is quoted and any quote inside it becomes an apostrophe, as `knock_text`
/// does, so peer text cannot close its own quotes and pose as the sentences after it,
/// which are this node's own.
impl fmt::Display for RemoteRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dest8 = short(&self.destination);
        match &self.display_name {
            Some(name) => write!(f, "\"{}\" ({dest8})", name.replace('"', "'"))?,
            None => f.write_str(dest8)?,
        }
        write!(
            f,
            " refused the request: it has not trusted this instance ({own}). Its owner can run .mesh trust {own} (or .mesh unblock {own} if it blocked this instance). (This node does trust {dest8}.)",
            own = self.own_destination
        )
    }
}
