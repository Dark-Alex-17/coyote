use std::fmt;
use std::time::Duration;

/// Why a request could not be completed. Callers match on this, so it is a closed set
/// rather than `anyhow`; `?` into an `anyhow::Result` still works through `std::error::Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum R3Error {
    /// No response arrived within `after`; the link may still be up.
    Timeout { path: String, after: Duration },
    /// The link could not be established or the packet could not be put on its interface.
    LinkFailed(String),
    /// The link closed while the request was pending, or was gone by the time we replied.
    LinkClosed,
    /// The encoded frame exceeds `MAX_R3_PAYLOAD_BYTES` and was not sent at all.
    Oversize { len: usize, max: usize },
    /// Bytes that are not a well-formed request or response frame.
    Decode(String),
    /// The transport refused to build or hand over the packet or resource.
    Send(String),
    /// The responder answered with one of the LXMF refusal sentinels instead of a body.
    Refused(RefusalCode),
    /// The mesh node has been stopped.
    NotRunning,
    /// The node stopped while the request was pending.
    Shutdown,
}

impl fmt::Display for R3Error {
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
            Self::NotRunning => write!(
                f,
                "The mesh node has been stopped; run `.mesh on` to start it again"
            ),
            Self::Shutdown => write!(f, "The mesh node stopped while the request was pending"),
        }
    }
}

impl std::error::Error for R3Error {}

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
