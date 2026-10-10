//! The `docs/mesh/PROTOCOL.md` ids the interop tests in `interop` exercise, kept apart from
//! that `#[cfg(unix)]` module so the coverage report and the section 20 table count them on
//! every platform. The Python reference serves only `/status` and `/message`, so the ids
//! here cover the announce, status and message exchanges alone; the `/list`, `/fetch` and
//! `/access` paths have no reference and are covered by the Rust-only vectors of
//! `share_vectors`, `access_vectors` and `live_vectors`.

use super::{Kind, Listed};

/// The reference decodes our announce and we file its announce.
pub(super) const ANNOUNCE_IDS: &[&str] = &[
    "MESH-DEST-003",
    "MESH-DEST-004",
    "MESH-DEST-005",
    "MESH-ANN-001",
    "MESH-ANN-002",
    "MESH-ANN-004",
    "MESH-ANN-010",
    "MESH-VER-003",
];

/// The reference's well-formed requests are served: the card and the received reply.
pub(super) const REPLY_VALID_IDS: &[&str] = &[
    "MESH-STATUS-001",
    "MESH-STATUS-004",
    "MESH-STATUS-012",
    "MESH-MSG-001",
    "MESH-MSG-015",
    "MESH-MSG-016",
];

/// The reference's untrusted, nameless, wrong-version, wrong-kind and unknown-path requests
/// hear `NoAccess`, `InvalidData`, the version refusal or the `unknown_path` map.
pub(super) const REPLY_INVALID_IDS: &[&str] = &[
    "MESH-ENV-014",
    "MESH-ENV-015",
    "MESH-ENV-029",
    "MESH-ENV-030",
    "MESH-ENV-032",
    "MESH-ENV-034",
    "MESH-ENV-038",
    "MESH-VER-005",
    "MESH-VER-006",
    "MESH-VER-007",
    "MESH-VER-008",
    "MESH-VER-009",
    "MESH-VER-010",
    "MESH-VER-011",
    "MESH-STATUS-003",
    "MESH-MSG-002",
    "MESH-MSG-018",
];

/// Our `/message` and `/status` requests are decoded by the reference as sent.
pub(super) const REQUEST_IDS: &[&str] = &[
    "MESH-ENV-012",
    "MESH-ENV-018",
    "MESH-ENV-022",
    "MESH-MSG-011",
    "MESH-MSG-014",
    "MESH-MSG-022",
    "MESH-STATUS-002",
    "MESH-CANON-008",
];

/// A propagation node announcing a stamp cost above the default and under the cap is filed,
/// selected and mined for at that cost.
pub(super) const PROPAGATION_COST_IDS: &[&str] = &[
    "MESH-ANN-024",
    "MESH-ANN-027",
    "MESH-PROP-010",
    "MESH-PROP-012",
];

/// A silent peer's message falls back to the propagation node and is stored there.
pub(super) const PROPAGATION_IDS: &[&str] = &["MESH-PROP-015", "MESH-MSG-024"];

/// The reference's message hears the bare `Throttled` code before any acknowledgement
/// while the envoy queue is full and again while the reference already has a run in
/// flight, and nothing is filed or counted either time; its bulletin is acknowledged as
/// ever, and so is its message once the run has ended.
pub(super) const THROTTLED_IDS: &[&str] = &["MESH-MSG-019", "MESH-MSG-041", "MESH-MSG-042"];

pub(super) fn listed() -> Vec<Listed> {
    [
        (ANNOUNCE_IDS, Kind::Valid),
        (REPLY_VALID_IDS, Kind::Valid),
        (REPLY_INVALID_IDS, Kind::Invalid),
        (REQUEST_IDS, Kind::Valid),
        (PROPAGATION_COST_IDS, Kind::Boundary),
        (PROPAGATION_IDS, Kind::Valid),
        (THROTTLED_IDS, Kind::Invalid),
    ]
    .into_iter()
    .flat_map(|(ids, kind)| {
        ids.iter().map(move |id| Listed {
            id,
            kind,
            family: "Interop",
        })
    })
    .collect()
}
