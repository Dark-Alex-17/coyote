//! Requirement-id keyed vectors for what only shows on a live link: size branches, the inbound
//! caps, handler slots and timeouts, response correlation, version marks and the sender's
//! outcome for a refused knock or an unacknowledged message. The table is declared on every
//! platform so coverage counts it; the executor runs over the loopback fixtures of
//! `r3::tests::network`, which are `#[cfg(unix)]`.

use super::{Kind, Listed};
use crate::mesh::r3::SizeBranch;

// Read only by the unix executor below; the table itself is declared everywhere so coverage counts it.
#[cfg_attr(not(unix), allow(dead_code))]
struct Vector {
    id: &'static str,
    kind: Kind,
    case: Case,
    known_divergence: Option<&'static str>,
}

/// An encoded frame length relative to the link MDU.
#[derive(Clone, Copy, Debug)]
enum Len {
    MduMinusOne,
    Mdu,
    MduPlusOne,
}

#[derive(Clone, Copy, Debug)]
enum Direction {
    Request,
    Response,
}

/// How the peer came to be marked incompatible before the outbound request.
#[derive(Clone, Copy, Debug)]
enum Mark {
    WireRefusal,
    Announce,
}

/// A refusal the knock responder sends back instead of filing the knock.
// Its payload is read only by the unix executor below.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
enum KnockRefusal {
    /// The wire byte of a refusal code other than the one that means "filed".
    Code(u8),
    Version,
}

/// A reply to a peer message that is not the received reply for that message's id.
#[derive(Clone, Copy, Debug)]
enum OtherReply {
    Integer,
    Nil,
    NotReceived,
    OtherId,
}

// Read only by the unix executor below; the table itself is declared everywhere so coverage counts it.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
enum Case {
    /// The responder answers under the request id the requester computed.
    Correlation { len: Len },
    /// A response forged onto another link is ignored and the request times out.
    WrongLink,
    SizeBranch {
        direction: Direction,
        len: Len,
        branch: SizeBranch,
    },
    /// A frame over `MAX_R3_PAYLOAD_BYTES` is refused locally and never seen by the responder.
    OutboundCap,
    /// The responder sees the identity the requester proved on the link.
    Identified,
    /// Bytes that are not a request frame are dropped in silence.
    UndecodableFrame,
    /// A request resource over `MAX_R3_PAYLOAD_BYTES` is dropped after assembly.
    InboundCap,
    /// The request past the last handler slot is dropped in silence.
    HandlerSlots,
    /// A handler still running at `HANDLER_TIMEOUT` is abandoned and nothing is sent.
    HandlerTimeout,
    /// A requester that hears nothing ends in `Timeout` after its request timeout.
    RequestTimeout,
    /// A link that never comes up ends the request in `Timeout` at the link timeout.
    LinkTimeout,
    /// A destination the transport has no path to ends the request in `LinkFailed` at once,
    /// naming the destination truncated.
    NoKnownPath,
    /// A wire version refusal with this window marks the peer `Incompatible { found: marked }`.
    VersionMark {
        min: u16,
        max: u16,
        marked: Option<u16>,
    },
    /// A send to a peer marked incompatible is refused before any link is opened.
    IncompatibleOutbound { via: Mark },
    /// A knock refused with any other code, or a version refusal, is a direct failure that is
    /// neither stored nor retried through a propagation node.
    OtherKnockRefusal { reply: KnockRefusal },
    /// A message answered with anything but its received reply ends in `NotAcknowledged`
    /// without a fallback.
    UnacknowledgedReply { value: OtherReply },
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Self::Correlation { .. } => "Correlation",
            Self::WrongLink => "WrongLink",
            Self::SizeBranch { .. } => "SizeBranch",
            Self::OutboundCap => "OutboundCap",
            Self::Identified => "Identified",
            Self::UndecodableFrame => "UndecodableFrame",
            Self::InboundCap => "InboundCap",
            Self::HandlerSlots => "HandlerSlots",
            Self::HandlerTimeout => "HandlerTimeout",
            Self::RequestTimeout => "RequestTimeout",
            Self::LinkTimeout => "LinkTimeout",
            Self::NoKnownPath => "NoKnownPath",
            Self::VersionMark { .. } => "VersionMark",
            Self::IncompatibleOutbound { .. } => "IncompatibleOutbound",
            Self::OtherKnockRefusal { .. } => "OtherKnockRefusal",
            Self::UnacknowledgedReply { .. } => "UnacknowledgedReply",
        }
    }
}

const VECTORS: &[Vector] = &[
    Vector {
        id: "MESH-ENV-005",
        kind: Kind::Valid,
        case: Case::Correlation { len: Len::Mdu },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-005",
        kind: Kind::Valid,
        case: Case::Correlation {
            len: Len::MduPlusOne,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-007",
        kind: Kind::Invalid,
        case: Case::WrongLink,
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-008",
        kind: Kind::Valid,
        case: Case::SizeBranch {
            direction: Direction::Request,
            len: Len::MduMinusOne,
            branch: SizeBranch::Packet,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-008",
        kind: Kind::Boundary,
        case: Case::SizeBranch {
            direction: Direction::Request,
            len: Len::Mdu,
            branch: SizeBranch::Packet,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-009",
        kind: Kind::Boundary,
        case: Case::SizeBranch {
            direction: Direction::Request,
            len: Len::MduPlusOne,
            branch: SizeBranch::Resource,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-010",
        kind: Kind::Boundary,
        case: Case::SizeBranch {
            direction: Direction::Response,
            len: Len::Mdu,
            branch: SizeBranch::Packet,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-010",
        kind: Kind::Boundary,
        case: Case::SizeBranch {
            direction: Direction::Response,
            len: Len::MduPlusOne,
            branch: SizeBranch::Resource,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-011",
        kind: Kind::Invalid,
        case: Case::OutboundCap,
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-012",
        kind: Kind::Valid,
        case: Case::Identified,
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-028",
        kind: Kind::Invalid,
        case: Case::UndecodableFrame,
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-024",
        kind: Kind::Invalid,
        case: Case::InboundCap,
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-025",
        kind: Kind::Invalid,
        case: Case::HandlerSlots,
        known_divergence: None,
    },
    Vector {
        id: "MESH-ENV-037",
        kind: Kind::Invalid,
        case: Case::HandlerTimeout,
        known_divergence: None,
    },
    Vector {
        id: "MESH-TIME-010",
        kind: Kind::Invalid,
        case: Case::HandlerTimeout,
        known_divergence: None,
    },
    Vector {
        id: "MESH-TIME-008",
        kind: Kind::Invalid,
        case: Case::RequestTimeout,
        known_divergence: None,
    },
    Vector {
        id: "MESH-TIME-009",
        kind: Kind::Invalid,
        case: Case::LinkTimeout,
        known_divergence: None,
    },
    Vector {
        id: "MESH-TIME-011",
        kind: Kind::Invalid,
        case: Case::NoKnownPath,
        known_divergence: None,
    },
    Vector {
        id: "MESH-VER-013",
        kind: Kind::Valid,
        case: Case::VersionMark {
            min: 2,
            max: 3,
            marked: Some(3),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-VER-013",
        kind: Kind::Invalid,
        case: Case::VersionMark {
            min: 1,
            max: 2,
            marked: None,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-VER-013",
        kind: Kind::Invalid,
        case: Case::VersionMark {
            min: 3,
            max: 2,
            marked: None,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-VER-014",
        kind: Kind::Invalid,
        case: Case::IncompatibleOutbound {
            via: Mark::WireRefusal,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-VER-014",
        kind: Kind::Invalid,
        case: Case::IncompatibleOutbound {
            via: Mark::Announce,
        },
        known_divergence: None,
    },
    Vector {
        // "any other refusal code": every registered code but `NoAccess` (section 8.5).
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xf0),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xf3),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xf4),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xf5),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xfd),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xfe),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Code(0xf6),
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-KNOCK-018",
        kind: Kind::Invalid,
        case: Case::OtherKnockRefusal {
            reply: KnockRefusal::Version,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-MSG-023",
        kind: Kind::Invalid,
        case: Case::UnacknowledgedReply {
            value: OtherReply::Integer,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-MSG-023",
        kind: Kind::Invalid,
        case: Case::UnacknowledgedReply {
            value: OtherReply::Nil,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-MSG-023",
        kind: Kind::Invalid,
        case: Case::UnacknowledgedReply {
            value: OtherReply::NotReceived,
        },
        known_divergence: None,
    },
    Vector {
        id: "MESH-MSG-023",
        kind: Kind::Invalid,
        case: Case::UnacknowledgedReply {
            value: OtherReply::OtherId,
        },
        known_divergence: None,
    },
];

pub(super) fn listed() -> Vec<Listed> {
    VECTORS
        .iter()
        .map(|row| Listed {
            id: row.id,
            kind: row.kind,
            family: row.case.family(),
        })
        .collect()
}

#[cfg(unix)]
mod loopback {
    use super::{Case, Direction, KnockRefusal, Len, Mark, OtherReply, VECTORS, Vector};
    use crate::mesh::announce::AnnounceAppData;
    use crate::mesh::knock::{KnockError, KnockIntro};
    use crate::mesh::message::{OutboundPeer, PeerKind, SendError, received_reply};
    use crate::mesh::node::{KnockOptions, MeshRuntime, MeshSlot};
    use crate::mesh::protocol::{
        Compatibility, MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION, VersionRefusal,
    };
    use crate::mesh::r3::network::{
        REQUEST_FRAME_OVERHEAD, Recorder, Requester, Responder, SHORT_REQUEST_TIMEOUT, Script,
        Stall, hanging_request, link_deadline, pair, request_body_of_encoded_len, request_deadline,
        request_on, response_body_of_encoded_len, response_payloads, short_options, timed_out,
        timed_out_slow_request,
    };
    use crate::mesh::r3::{
        Deadline, MAX_CONCURRENT_INBOUND_REQUESTS, MAX_R3_PAYLOAD_BYTES, R3Error, R3Server,
        RefusalCode, Reply, RequestFrame, RequestId, RequestOptions, ResponseFrame, STATUS_PATH,
        SizeBranch, open_link, short,
    };
    use crate::mesh::test_support::{
        INTEROP_TIMEOUT, LEGACY_LINK_MTU, StartedRuntime, started_runtime_on, wait_until,
    };
    use crate::mesh::trust::TrustOptions;
    use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

    use rand_core::OsRng;
    use rmpv::Value;
    use rns_transport::destination::{DestinationDesc, DestinationName, SingleInputDestination};
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use rns_transport::iface::InterfaceSharedConfig;
    use rns_transport::iface::tcp_client::TcpClient;
    use rns_transport::iface::tcp_server::TcpServer;
    use rns_transport::resource::LINK_PACKET_MDU;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant, SystemTime};
    use tokio::time::timeout;

    const ECHO_PATH: &str = "/echo";
    const SLOW_PATH: &str = "/slow";
    const BIG_PATH: &str = "/big";
    const SHORT_HANDLER_TIMEOUT: Duration = Duration::from_millis(300);
    const SHORT_LINK_TIMEOUT: Duration = Duration::from_secs(1);

    /// Which test drives a row; each group is one `#[tokio::test]`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Group {
        Sizes,
        Drops,
        Timeouts,
        Versions,
        Outcomes,
    }

    fn group(case: &Case) -> Group {
        match case {
            Case::Correlation { .. } | Case::SizeBranch { .. } | Case::Identified => Group::Sizes,
            Case::WrongLink | Case::UndecodableFrame | Case::InboundCap | Case::HandlerSlots => {
                Group::Drops
            }
            Case::OutboundCap
            | Case::HandlerTimeout
            | Case::RequestTimeout
            | Case::LinkTimeout
            | Case::NoKnownPath => Group::Timeouts,
            Case::VersionMark { .. } | Case::IncompatibleOutbound { .. } => Group::Versions,
            Case::OtherKnockRefusal { .. } | Case::UnacknowledgedReply { .. } => Group::Outcomes,
        }
    }

    impl Len {
        fn target(self, mdu: usize) -> usize {
            match self {
                Self::MduMinusOne => mdu - 1,
                Self::Mdu => mdu,
                Self::MduPlusOne => mdu + 1,
            }
        }
    }

    /// The text of a caught panic payload, for failure messages that name the vector.
    fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
        match payload.downcast::<String>() {
            Ok(text) => *text,
            Err(payload) => payload
                .downcast::<&'static str>()
                .map(|text| (*text).to_string())
                .unwrap_or_else(|_| "a panic without a message".to_string()),
        }
    }

    /// Runs every row of `wanted` on its own task and reports the failures together, each
    /// naming the row's id and family.
    async fn run(wanted: Group) {
        let rows: Vec<&Vector> = VECTORS
            .iter()
            .filter(|row| group(&row.case) == wanted)
            .collect();
        let ran = rows.len();
        assert!(ran > 0, "no {wanted:?} vectors");
        let mut failures = Vec::new();
        for row in rows {
            let case = row.case;
            let tag = format!("{} [{}]", row.id, case.family());
            let outcome = tokio::spawn(drive(case)).await.map_err(|err| {
                if err.is_panic() {
                    panic_message(err.into_panic())
                } else {
                    err.to_string()
                }
            });
            match (outcome, row.known_divergence) {
                (Ok(()), None) => {}
                (Err(detail), None) => failures.push(format!("{tag}: {detail}")),
                (Err(detail), Some(note)) => {
                    println!("{tag}: known divergence, not asserted: {note}; observed: {detail}");
                }
                (Ok(()), Some(note)) => failures.push(format!(
                    "{tag}: flagged as a known divergence but passes; drop the flag ({note})"
                )),
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {ran} {wanted:?} vectors failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    async fn drive(case: Case) {
        match case {
            Case::Correlation { len } => correlation(len).await,
            Case::WrongLink => wrong_link().await,
            Case::SizeBranch {
                direction,
                len,
                branch,
            } => size_branch(direction, len, branch).await,
            Case::OutboundCap => outbound_cap().await,
            Case::Identified => identified().await,
            Case::UndecodableFrame => undecodable_frame().await,
            Case::InboundCap => inbound_cap().await,
            Case::HandlerSlots => handler_slots().await,
            Case::HandlerTimeout => handler_timeout().await,
            Case::RequestTimeout => request_timeout().await,
            Case::LinkTimeout => link_timeout().await,
            Case::NoKnownPath => no_known_path().await,
            Case::VersionMark { min, max, marked } => version_mark(min, max, marked).await,
            Case::IncompatibleOutbound { via } => incompatible_outbound(via).await,
            Case::OtherKnockRefusal { reply } => other_knock_refusal(reply).await,
            Case::UnacknowledgedReply { value } => unacknowledged_reply(value).await,
        }
    }

    struct EchoPair {
        recorder: Arc<Recorder>,
        responder: Responder,
        requester: Requester,
        desc: DestinationDesc,
        mdu: usize,
    }

    /// A recorder-backed pair over the legacy MTU, with the link MDU the spec's reference
    /// value of 431 bytes.
    async fn echo_pair() -> EchoPair {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let link = open_link(&requester.transport, &desc, ECHO_PATH, link_deadline())
            .await
            .unwrap();
        let mdu = link.lock().await.link_mdu();
        assert_eq!(mdu, LINK_PACKET_MDU);
        assert_eq!(mdu, 431);
        drop(link);
        EchoPair {
            recorder,
            responder,
            requester,
            desc,
            mdu,
        }
    }

    impl EchoPair {
        async fn stop(self) {
            self.requester.stop().await;
            self.responder.stop().await;
        }
    }

    async fn correlation(len: Len) {
        let pair = echo_pair().await;
        let body = request_body_of_encoded_len(pair.requester.origin, len.target(pair.mdu));

        let outcome = pair
            .requester
            .request(&pair.desc, ECHO_PATH, body.clone())
            .await
            .unwrap();

        let seen = pair.recorder.last();
        assert_eq!(seen.request_id, outcome.request_id, "{len:?}");
        assert_eq!(seen.branch, outcome.request_branch, "{len:?}");
        assert_eq!(outcome.value, body, "{len:?}");
        pair.stop().await;
    }

    async fn size_branch(direction: Direction, len: Len, expected: SizeBranch) {
        let pair = echo_pair().await;
        let target = len.target(pair.mdu);
        match direction {
            Direction::Request => {
                let body = request_body_of_encoded_len(pair.requester.origin, target);
                let outcome = pair
                    .requester
                    .request(&pair.desc, ECHO_PATH, body.clone())
                    .await
                    .unwrap();
                assert_eq!(
                    outcome.request_branch, expected,
                    "a {target}-byte request frame"
                );
                assert_eq!(outcome.value, body);
                assert_eq!(pair.recorder.last().branch, expected);
            }
            Direction::Response => {
                let body = response_body_of_encoded_len(target);
                pair.recorder
                    .queue(Script::Reply(Reply::Value(body.clone())));
                let outcome = pair
                    .requester
                    .request(&pair.desc, ECHO_PATH, Value::Nil)
                    .await
                    .unwrap();
                assert_eq!(outcome.request_branch, SizeBranch::Packet);
                assert_eq!(
                    outcome.response_branch, expected,
                    "a {target}-byte response frame"
                );
                assert_eq!(outcome.value, body);
            }
        }
        pair.stop().await;
    }

    async fn identified() {
        let pair = echo_pair().await;

        let outcome = pair
            .requester
            .request(&pair.desc, ECHO_PATH, Value::from("who"))
            .await
            .unwrap();

        assert_eq!(outcome.value, Value::from("who"));
        assert_eq!(
            pair.recorder.last().identity,
            Some(pair.requester.identity.as_identity().address_hash)
        );
        assert_eq!(pair.responder.server.identified_peer_count(), 1);
        pair.stop().await;
    }

    async fn outbound_cap() {
        let pair = echo_pair().await;
        let body = Value::Binary(vec![0xab; MAX_R3_PAYLOAD_BYTES]);
        let len = RequestFrame::new(BIG_PATH, pair.requester.envelope(body.clone()).into_value())
            .encode()
            .len();
        assert!(len > MAX_R3_PAYLOAD_BYTES);

        let err = pair
            .requester
            .request(&pair.desc, BIG_PATH, body)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            R3Error::Oversize {
                len,
                max: MAX_R3_PAYLOAD_BYTES,
            }
        );
        assert_eq!(pair.recorder.seen_count(), 0);
        assert_eq!(pair.responder.server.decoded_count(), 0);
        pair.stop().await;
    }

    async fn undecodable_frame() {
        let pair = echo_pair().await;
        let transport = &pair.requester.transport;
        let link = open_link(transport, &pair.desc, ECHO_PATH, link_deadline())
            .await
            .unwrap();
        let mut events = transport.out_link_events();
        let mut garbage = Vec::new();
        rmpv::encode::write_value(
            &mut garbage,
            &Value::Array(vec![Value::F64(1.0), Value::Binary(vec![0; 16])]),
        )
        .unwrap();

        let packet = link.lock().await.request_packet(&garbage).unwrap();
        transport
            .send_link_packet_on_bound_iface(&link, packet)
            .await;
        wait_until("the responder to reach the frame decoder", || {
            pair.responder.server.decoded_count() == 1
        })
        .await;

        assert_eq!(pair.recorder.seen_count(), 0);
        let outcome = request_on(
            &pair.requester,
            &link,
            ECHO_PATH,
            Value::from("after"),
            request_deadline(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.value, Value::from("after"));
        assert_eq!(pair.responder.server.decoded_count(), 2);
        assert_eq!(pair.recorder.seen_count(), 1);
        assert_eq!(
            response_payloads(&mut events).len(),
            1,
            "only the well-formed request was answered"
        );
        pair.stop().await;
    }

    async fn wrong_link() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, mut requester, desc) = pair(recorder.clone()).await;
        // The requester's interface is new to its transport, whose announce ingress control
        // holds a second announce arriving within a second of the first for a minute.
        requester
            .transport
            .iface_manager()
            .lock()
            .await
            .set_shared_config(
                requester.iface,
                InterfaceSharedConfig {
                    ingress_control: Some(false),
                    ..InterfaceSharedConfig::default()
                },
            );
        let decoy = responder
            .transport
            .add_destination(
                TransportIdentity::new_from_rand(OsRng),
                DestinationName::new("coyote", "mesh.decoy"),
            )
            .await;
        let decoy_hash = decoy.lock().await.desc.address_hash;
        let packet = decoy.lock().await.announce(OsRng, None).unwrap();
        responder.transport.send_packet(packet).await;
        let decoy_desc = requester.learn(&decoy_hash).await;
        let decoy_link = open_link(
            &requester.transport,
            &decoy_desc,
            SLOW_PATH,
            link_deadline(),
        )
        .await
        .unwrap();
        let decoy_link_id = *decoy_link.lock().await.id();
        let (in_flight, seen) = hanging_request(&requester, &recorder, &desc).await;
        assert_ne!(seen.link_id, decoy_link_id);

        let decoy_in_link = responder
            .transport
            .find_in_link(&decoy_link_id)
            .await
            .expect("the responder holds the decoy in-link");
        let forged = ResponseFrame {
            request_id: seen.request_id,
            data: Value::from("forged"),
        }
        .encode();
        let packet = decoy_in_link.lock().await.response_packet(&forged).unwrap();
        responder
            .transport
            .send_link_packet_on_bound_iface(&decoy_in_link, packet)
            .await;

        let ignored = format!(
            "Ignored a mesh response for {} that arrived on link {} instead of {}",
            seen.request_id.to_hex_string(),
            decoy_link_id.to_hex_string(),
            seen.link_id.to_hex_string()
        );
        wait_until("the requester to ignore the forged response", || {
            debug_snapshot().contains(&ignored)
        })
        .await;
        assert_eq!(requester.client.pending_len(), 1);
        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), timed_out_slow_request());
        requester.stop().await;
        responder.stop().await;
    }

    async fn inbound_cap() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let responder = Responder::listen(recorder.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let mut requester = Requester::connect(responder.port, TcpClient::DEFAULT_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;
        let transport = &requester.transport;
        let link = open_link(transport, &desc, BIG_PATH, link_deadline())
            .await
            .unwrap();
        let link_id = *link.lock().await.id();
        let body = Value::Binary(vec![
            0xab;
            MAX_R3_PAYLOAD_BYTES + 1 - REQUEST_FRAME_OVERHEAD
        ]);
        let packed = RequestFrame::new(BIG_PATH, body).encode();
        assert_eq!(packed.len(), MAX_R3_PAYLOAD_BYTES + 1);

        transport
            .send_request_resource(
                &link_id,
                RequestId::of_packed(&packed).to_vec(),
                packed,
                None,
            )
            .await
            .unwrap();

        let dropped = format!(
            "Dropped an oversize mesh request on link {} ({} bytes, max {MAX_R3_PAYLOAD_BYTES})",
            link_id.to_hex_string(),
            MAX_R3_PAYLOAD_BYTES + 1
        );
        wait_until("the responder to drop the oversize request", || {
            debug_snapshot().contains(&dropped)
        })
        .await;
        assert_eq!(recorder.seen_count(), 0);
        assert_eq!(responder.server.decoded_count(), 0);
        let outcome = request_on(
            &requester,
            &link,
            ECHO_PATH,
            Value::from("after"),
            request_deadline(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.value, Value::from("after"));
        assert_eq!(recorder.seen_count(), 1);
        requester.stop().await;
        responder.stop().await;
    }

    async fn handler_slots() {
        let stall = Arc::new(Stall::default());
        let (responder, requester, desc) = pair(stall.clone()).await;
        let link = open_link(&requester.transport, &desc, SLOW_PATH, link_deadline())
            .await
            .unwrap();
        let in_flight: Vec<_> = (0..MAX_CONCURRENT_INBOUND_REQUESTS)
            .map(|_| {
                let client = requester.client.clone();
                let transport = requester.transport.clone();
                let link = link.clone();
                tokio::spawn(async move {
                    client
                        .request_on_link(
                            &transport,
                            &link,
                            SLOW_PATH,
                            Value::Nil,
                            request_deadline(),
                        )
                        .await
                })
            })
            .collect();
        wait_until("every handler slot to be taken", || {
            stall.entered.load(Ordering::SeqCst) == MAX_CONCURRENT_INBOUND_REQUESTS
        })
        .await;

        let err = request_on(
            &requester,
            &link,
            SLOW_PATH,
            Value::Nil,
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();

        assert_eq!(err, timed_out(SLOW_PATH));
        assert_eq!(
            stall.entered.load(Ordering::SeqCst),
            MAX_CONCURRENT_INBOUND_REQUESTS,
            "the seventeenth request never reached a handler"
        );
        for task in in_flight {
            task.abort();
        }
        requester.stop().await;
        responder.stop().await;
    }

    async fn handler_timeout() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Hang);
        let server = Arc::new(R3Server::with_handler_timeout_for_test(
            SHORT_HANDLER_TIMEOUT,
        ));
        let responder = Responder::listen_on(server, recorder.clone(), LEGACY_LINK_MTU).await;
        let mut requester = Requester::connect(responder.port, LEGACY_LINK_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;

        let err = requester
            .client
            .request(
                &requester.transport,
                &requester.identity,
                &desc,
                SLOW_PATH,
                requester.envelope(Value::Nil),
                short_options(),
            )
            .await
            .unwrap_err();

        assert_eq!(err, timed_out(SLOW_PATH));
        let seen = recorder.last();
        let warned = format!(
            "Mesh request {} on link {} was not handled within {}ms; sent nothing",
            seen.request_id.to_hex_string(),
            seen.link_id.to_hex_string(),
            SHORT_HANDLER_TIMEOUT.as_millis()
        );
        assert!(
            warn_snapshot().iter().any(|message| message == &warned),
            "expected {warned:?}"
        );
        wait_until("the hung handler to be dropped", || {
            recorder.abandoned_count() == 1
        })
        .await;
        requester.stop().await;
        responder.stop().await;
    }

    async fn request_timeout() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let started = Instant::now();

        let (in_flight, _seen) = hanging_request(&requester, &recorder, &desc).await;
        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();

        assert_eq!(result.unwrap_err(), timed_out_slow_request());
        assert!(started.elapsed() >= SHORT_REQUEST_TIMEOUT);
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    async fn link_timeout() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        responder.stop().await;

        let err = requester
            .client
            .request(
                &requester.transport,
                &requester.identity,
                &desc,
                ECHO_PATH,
                requester.envelope(Value::Nil),
                RequestOptions {
                    link_timeout: SHORT_LINK_TIMEOUT,
                    request_timeout: SHORT_REQUEST_TIMEOUT,
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(err, R3Error::Timeout { .. }), "{err:?}");
        assert_eq!(recorder.seen_count(), 0);
        requester.stop().await;
    }

    /// A destination nobody serves and the requester has never heard announced, so the
    /// transport holds no path to it and the request fails before a link is attempted.
    async fn no_known_path() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, _desc) = pair(recorder.clone()).await;
        let ghost = SingleInputDestination::new(
            TransportIdentity::new_from_rand(OsRng),
            DestinationName::new("coyote", "mesh.ghost"),
        )
        .desc;
        let ghost_hex = ghost.address_hash.to_hex_string();

        let err = requester
            .client
            .request(
                &requester.transport,
                &requester.identity,
                &ghost,
                ECHO_PATH,
                requester.envelope(Value::Nil),
                RequestOptions {
                    link_timeout: SHORT_LINK_TIMEOUT,
                    request_timeout: SHORT_REQUEST_TIMEOUT,
                },
            )
            .await
            .unwrap_err();

        let R3Error::LinkFailed(reason) = err else {
            panic!("expected LinkFailed for a destination without a path, got {err:?}");
        };
        assert!(
            reason.contains(&ghost_hex),
            "the error value keeps the full destination for the human; redaction belongs to the log sinks: {reason}"
        );
        assert_eq!(recorder.seen_count(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    /// A runtime whose only peer is a recorder-backed responder that announced `version`.
    struct PeerRuntime {
        recorder: Arc<Recorder>,
        responder: Responder,
        runtime: Arc<MeshRuntime>,
        slot: Arc<MeshSlot>,
        destination_hex: String,
        _started: StartedRuntime,
    }

    async fn peer_runtime(tag: &str, version: u16) -> PeerRuntime {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let responder = Responder::listen(recorder.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on(tag, responder.port).await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        let app_data = AnnounceAppData {
            version,
            display_name: Some("Peer".to_string()),
        }
        .encode()
        .unwrap();
        responder.announce(Some(&app_data)).await;
        let destination_hex = responder.desc.address_hash.to_hex_string();
        let peers = runtime.peers();
        wait_until("the runtime to file the responder as a peer", || {
            peers.get(&destination_hex).is_some()
        })
        .await;
        PeerRuntime {
            recorder,
            responder,
            runtime,
            slot,
            destination_hex,
            _started: started,
        }
    }

    impl PeerRuntime {
        fn compatibility(&self) -> Compatibility {
            self.runtime
                .peers()
                .get(&self.destination_hex)
                .unwrap()
                .compatibility
        }

        /// One `/status` request the responder answers with a version refusal of `min..=max`.
        async fn refused_request(&self, min: u16, max: u16) {
            let desc = self
                .runtime
                .resolve_destination(&self.destination_hex)
                .await
                .unwrap();
            let refusal = VersionRefusal {
                found: Some(MESH_PROTOCOL_VERSION),
                min,
                max,
            };
            self.recorder
                .queue(Script::Reply(Reply::Value(refusal.to_value())));

            let err = self
                .runtime
                .request(&desc, STATUS_PATH, Value::Nil, RequestOptions::default())
                .await
                .unwrap_err();

            assert_eq!(
                err,
                R3Error::UnsupportedVersion {
                    found: Some(MESH_PROTOCOL_VERSION),
                    min,
                    max,
                }
            );
        }

        /// The debug lines since `mark` that report a link to the responder coming up.
        fn links_opened_since(&self, mark: usize) -> Vec<String> {
            let needle = format!("to destination {} is active", short(&self.destination_hex));
            debug_snapshot()
                .into_iter()
                .skip(mark)
                .filter(|line| line.contains(&needle))
                .collect()
        }

        async fn stop(self) {
            assert!(self.slot.stop().await.unwrap());
            self.responder.stop().await;
        }
    }

    async fn version_mark(min: u16, max: u16, marked: Option<u16>) {
        let peer = peer_runtime("conformance-version-mark", MESH_PROTOCOL_VERSION).await;
        assert_eq!(peer.compatibility(), Compatibility::Compatible);

        peer.refused_request(min, max).await;

        let expected = marked.map_or(Compatibility::Compatible, |found| {
            Compatibility::Incompatible { found }
        });
        assert_eq!(peer.compatibility(), expected, "window {min}..={max}");
        peer.stop().await;
    }

    async fn incompatible_outbound(via: Mark) {
        let newer = MESH_PROTOCOL_VERSION + 1;
        let peer = match via {
            Mark::WireRefusal => {
                let peer = peer_runtime("conformance-marked-by-wire", MESH_PROTOCOL_VERSION).await;
                peer.refused_request(newer, newer).await;
                peer
            }
            Mark::Announce => peer_runtime("conformance-marked-by-announce", newer).await,
        };
        assert_eq!(
            peer.compatibility(),
            Compatibility::Incompatible { found: newer }
        );
        peer.runtime
            .trust()
            .trust_destination(
                peer.slot.as_ref(),
                &peer.destination_hex,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let seen_before = peer.recorder.seen_count();
        let mark = debug_snapshot().len();
        let message = OutboundPeer::new(PeerKind::Message, "hello", None, None, None).unwrap();

        let err = peer
            .runtime
            .send_peer(&peer.destination_hex, &message)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            SendError::IncompatibleVersion {
                destination: peer.destination_hex.clone(),
                found: Some(newer),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            }
        );
        let links = peer.links_opened_since(mark);
        assert!(
            links.is_empty(),
            "a marked peer is never linked to: {links:?}"
        );
        assert_eq!(peer.recorder.seen_count(), seen_before);
        peer.stop().await;
    }

    /// The runtime has no propagation node, so a fallback attempt would surface as
    /// `NoPropagationNode` rather than the direct failure the spec mandates.
    async fn other_knock_refusal(reply: KnockRefusal) {
        let peer = peer_runtime("conformance-knock-refusal", MESH_PROTOCOL_VERSION).await;
        let desc = peer
            .runtime
            .resolve_destination(&peer.destination_hex)
            .await
            .unwrap();
        let expected = match reply {
            KnockRefusal::Code(byte) => {
                let code = RefusalCode::from_wire(&Value::from(byte))
                    .unwrap_or_else(|| panic!("{byte:#x} is not a refusal code"));
                peer.recorder.queue(Script::Reply(Reply::Code(code)));
                R3Error::Refused(code)
            }
            KnockRefusal::Version => {
                let newer = MESH_PROTOCOL_VERSION + 1;
                let refusal = VersionRefusal {
                    found: Some(MESH_PROTOCOL_VERSION),
                    min: newer,
                    max: newer,
                };
                peer.recorder
                    .queue(Script::Reply(Reply::Value(refusal.to_value())));
                R3Error::UnsupportedVersion {
                    found: Some(MESH_PROTOCOL_VERSION),
                    min: newer,
                    max: newer,
                }
            }
        };
        let intro = KnockIntro::new("hi").unwrap();

        let err = peer
            .runtime
            .knock_with(&desc, &intro, KnockOptions::default())
            .await
            .unwrap_err();

        assert_eq!(err, KnockError::Direct(expected), "{reply:?}");
        assert_eq!(
            peer.recorder.seen_count(),
            1,
            "{reply:?}: one request, no retry"
        );
        peer.stop().await;
    }

    /// Discriminated the same way as `other_knock_refusal`: without a propagation node a
    /// fallback would end in `NoPropagationNode`, not `NotAcknowledged`.
    async fn unacknowledged_reply(value: OtherReply) {
        let peer = peer_runtime("conformance-unacknowledged-reply", MESH_PROTOCOL_VERSION).await;
        peer.runtime
            .trust()
            .trust_destination(
                peer.slot.as_ref(),
                &peer.destination_hex,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let message = OutboundPeer::new(PeerKind::Message, "hello", None, None, None).unwrap();
        let reply = match value {
            OtherReply::Integer => Value::from(7),
            OtherReply::Nil => Value::Nil,
            OtherReply::NotReceived => {
                Value::Map(vec![(Value::from("received"), Value::from(false))])
            }
            OtherReply::OtherId => received_reply(&format!("not-{}", message.id)),
        };
        peer.recorder.queue(Script::Reply(Reply::Value(reply)));

        let err = peer
            .runtime
            .send_peer(&peer.destination_hex, &message)
            .await
            .unwrap_err();

        assert_eq!(err, SendError::NotAcknowledged, "{value:?}");
        assert_eq!(
            peer.recorder.seen_count(),
            1,
            "{value:?}: one request, no retry"
        );
        peer.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn size_branches_and_correlation_hold_on_a_live_link() {
        run(Group::Sizes).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_responder_drops_what_section_6_6_says_it_drops() {
        run(Group::Drops).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_timeouts_and_the_outbound_cap_end_requests_as_specified() {
        run(Group::Timeouts).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_refusals_mark_peers_and_marked_peers_are_refused_outbound() {
        run(Group::Versions).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sender_outcomes_end_as_sections_8_5_and_10_4_mandate() {
        run(Group::Outcomes).await;
    }

    /// Every group a row lands in has one of the `#[tokio::test]`s above driving it, and every
    /// registered refusal code but `NoAccess` has a MESH-KNOCK-018 row: a row that counts for
    /// coverage but is never driven, or an "any other code" row that only tries one code, would
    /// let the table over-report what it asserts.
    #[test]
    fn every_row_is_driven_and_every_other_refusal_code_has_a_knock_row() {
        const DRIVEN: &[Group] = &[
            Group::Sizes,
            Group::Drops,
            Group::Timeouts,
            Group::Versions,
            Group::Outcomes,
        ];
        for row in VECTORS {
            let group = group(&row.case);
            assert!(
                DRIVEN.contains(&group),
                "{} [{group:?}] has no test",
                row.id
            );
        }
        let knock_codes: Vec<u8> = VECTORS
            .iter()
            .filter_map(|row| match row.case {
                Case::OtherKnockRefusal {
                    reply: KnockRefusal::Code(byte),
                } if row.id == "MESH-KNOCK-018" => Some(byte),
                _ => None,
            })
            .collect();
        for code in [
            RefusalCode::NoIdentity,
            RefusalCode::InvalidKey,
            RefusalCode::InvalidData,
            RefusalCode::InvalidStamp,
            RefusalCode::Throttled,
            RefusalCode::NotFound,
            RefusalCode::Timeout,
        ] {
            assert!(
                knock_codes.contains(&(code as u8)),
                "MESH-KNOCK-018 has no row for {code:?}"
            );
        }
        let no_access = RefusalCode::from_wire(&Value::from(0xf1u8)).unwrap();
        assert!(
            !knock_codes.contains(&(no_access as u8)),
            "NoAccess lands the knock directly (MESH-KNOCK-016), it is not an `other` code"
        );
    }
}
