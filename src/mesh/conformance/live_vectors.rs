//! Requirement-id keyed vectors for the file-sharing surface as two nodes see it: the
//! list, access, decision and fetch cycle, the admission of an access request, the
//! requester's reading of a fetch reply, the share set's handling of symlinks and the
//! grant a reference attachment lends. The table is declared on every platform so coverage
//! counts it; the executor runs over the loopback node pair of `r3::tests::network`, which
//! is `#[cfg(unix)]`.

use super::{Kind, Listed};

// Read only by the unix executor below; the table itself is declared everywhere so coverage counts it.
#[cfg_attr(not(unix), allow(dead_code))]
struct Vector {
    id: &'static str,
    kind: Kind,
    case: Case,
}

/// How one key of a scripted fetch reply is written.
#[derive(Clone, Copy, Debug)]
enum Field {
    Right,
    Missing,
    /// The key is present with a value of another msgpack type.
    WrongType,
    /// A `bin` one byte shorter than the grammar fixes.
    Short,
    /// The right type with a value that does not fit: a `size` that is not the length of
    /// `bytes`, a `sha256` that is not their digest, a `rule` that is not one of the names.
    Other,
}

/// A status whose reply is complete and carries every other status' keys as well.
#[derive(Clone, Copy, Debug)]
enum Status {
    NotShared,
    TooLarge,
    NotModified,
    InvalidPath,
}

// Read only by the unix executor below; the table itself is declared everywhere so coverage counts it.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
enum Answer {
    NotAMap,
    /// `v` as given, or absent.
    Version(Option<u64>),
    NoStatus,
    StatusNotText,
    /// A `status` word the table does not spell.
    OtherStatus,
    Ok {
        size: Field,
        sha256: Field,
        bytes: Field,
    },
    NotModified {
        sha256: Field,
    },
    TooLarge {
        limit: Field,
    },
    InvalidPath {
        rule: Field,
    },
    Extras(Status),
}

// Read only by the unix executor below; the table itself is declared everywhere so coverage counts it.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
enum Expect {
    Malformed(&'static str),
    Corrupt,
    UnknownStatus,
    NotShared,
    TooLarge,
    NotModified,
    InvalidPath(&'static str),
}

// Read only by the unix executor below; the table itself is declared everywhere so coverage counts it.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
enum Case {
    /// `/list` answers the share set as it stands for the requester, never the tree.
    Listing,
    /// `/fetch` of a path an allow serves is `ok` with the bytes and their digest.
    ServedByAllow,
    /// `/access` for a path nothing serves is filed and answered `pending`; the envoy is
    /// never consulted.
    FiledPending,
    /// The one-off grant is in the store once the decision reply has been sent directly.
    GrantBeforeReply,
    /// The decision travels as an answered reply with one data part and no path in it.
    DecisionReply,
    /// The granted path is served once on the grant; `spent` asks for it a second time.
    GrantServes { spent: bool },
    /// A grant is not a share: `/list` after the grant still names only the allowed file.
    GrantHiddenFromListing,
    /// No hook environment raised by the request or the decision names a path or the reason.
    NoSideChannel,
    /// Paths the share set already serves are granted on the wire at once with nothing filed.
    GrantedAtOnce,
    /// An identity the trust list does not admit hears silence on `/access`.
    UnknownRequester,
    /// A one-off decision whose send fails leaves no grant and the request pending.
    DecisionSendFails,
    /// The requester reads a scripted fetch reply.
    FetchReply { answer: Answer, expect: Expect },
    /// A symlink under the root that resolves outside it is `not_shared`, as a missing file is.
    EscapingLink,
    /// A user deny on the resolved file holds when the peer asks through an alias symlink.
    DenyThroughAlias,
    /// A mutation whose target share file is a planted symlink is refused and the target untouched.
    PlantedShareFile,
    /// A reference attachment lends a one-off grant under the message id; `acknowledged`
    /// says whether the peer heard the message or the grant was taken back.
    Lend { acknowledged: bool },
}

impl Case {
    fn family(&self) -> &'static str {
        match self {
            Self::Listing
            | Self::ServedByAllow
            | Self::FiledPending
            | Self::GrantBeforeReply
            | Self::DecisionReply
            | Self::GrantServes { .. }
            | Self::GrantHiddenFromListing
            | Self::NoSideChannel => "Cycle",
            Self::GrantedAtOnce | Self::UnknownRequester | Self::DecisionSendFails => "Access",
            Self::FetchReply { .. } => "Requester",
            Self::EscapingLink | Self::DenyThroughAlias | Self::PlantedShareFile => "Symlinks",
            Self::Lend { .. } => "Lending",
        }
    }
}

const fn ok(size: Field, sha256: Field, bytes: Field) -> Answer {
    Answer::Ok {
        size,
        sha256,
        bytes,
    }
}

const fn reply(id: &'static str, kind: Kind, answer: Answer, expect: Expect) -> Vector {
    Vector {
        id,
        kind,
        case: Case::FetchReply { answer, expect },
    }
}

const VECTORS: &[Vector] = &[
    Vector {
        id: "MESH-LIST-016",
        kind: Kind::Valid,
        case: Case::Listing,
    },
    Vector {
        id: "MESH-FETCH-024",
        kind: Kind::Valid,
        case: Case::ServedByAllow,
    },
    Vector {
        id: "MESH-ACCESS-016",
        kind: Kind::Valid,
        case: Case::FiledPending,
    },
    Vector {
        id: "MESH-ACCESS-019",
        kind: Kind::Valid,
        case: Case::GrantBeforeReply,
    },
    Vector {
        id: "MESH-ACCESS-018",
        kind: Kind::Valid,
        case: Case::DecisionReply,
    },
    Vector {
        id: "MESH-SHARE-014",
        kind: Kind::Valid,
        case: Case::GrantServes { spent: false },
    },
    Vector {
        id: "MESH-SHARE-014",
        kind: Kind::Boundary,
        case: Case::GrantServes { spent: true },
    },
    Vector {
        id: "MESH-LIST-016",
        kind: Kind::Boundary,
        case: Case::GrantHiddenFromListing,
    },
    Vector {
        id: "MESH-ACCESS-029",
        kind: Kind::Valid,
        case: Case::NoSideChannel,
    },
    Vector {
        id: "MESH-ACCESS-015",
        kind: Kind::Valid,
        case: Case::GrantedAtOnce,
    },
    Vector {
        id: "MESH-ACCESS-014",
        kind: Kind::Invalid,
        case: Case::UnknownRequester,
    },
    Vector {
        id: "MESH-ACCESS-019",
        kind: Kind::Invalid,
        case: Case::DecisionSendFails,
    },
    reply(
        "MESH-FETCH-013",
        Kind::Invalid,
        Answer::NotAMap,
        Expect::Malformed("map"),
    ),
    reply(
        "MESH-FETCH-013",
        Kind::Invalid,
        Answer::Version(None),
        Expect::Malformed("v"),
    ),
    reply(
        "MESH-FETCH-013",
        Kind::Invalid,
        Answer::Version(Some(2)),
        Expect::Malformed("v"),
    ),
    reply(
        "MESH-FETCH-014",
        Kind::Invalid,
        Answer::NoStatus,
        Expect::Malformed("status"),
    ),
    reply(
        "MESH-FETCH-014",
        Kind::Invalid,
        Answer::StatusNotText,
        Expect::Malformed("status"),
    ),
    reply(
        "MESH-FETCH-015",
        Kind::Invalid,
        Answer::OtherStatus,
        Expect::UnknownStatus,
    ),
    reply(
        "MESH-FETCH-016",
        Kind::Invalid,
        ok(Field::Missing, Field::Right, Field::Right),
        Expect::Malformed("size"),
    ),
    reply(
        "MESH-FETCH-016",
        Kind::Invalid,
        ok(Field::WrongType, Field::Right, Field::Right),
        Expect::Malformed("size"),
    ),
    reply(
        "MESH-FETCH-016",
        Kind::Invalid,
        ok(Field::Other, Field::Right, Field::Right),
        Expect::Malformed("size"),
    ),
    reply(
        "MESH-FETCH-017",
        Kind::Invalid,
        ok(Field::Right, Field::Missing, Field::Right),
        Expect::Malformed("sha256"),
    ),
    reply(
        "MESH-FETCH-017",
        Kind::Invalid,
        ok(Field::Right, Field::WrongType, Field::Right),
        Expect::Malformed("sha256"),
    ),
    reply(
        "MESH-FETCH-017",
        Kind::Invalid,
        ok(Field::Right, Field::Short, Field::Right),
        Expect::Malformed("sha256"),
    ),
    reply(
        "MESH-FETCH-017",
        Kind::Invalid,
        Answer::NotModified {
            sha256: Field::Missing,
        },
        Expect::Malformed("sha256"),
    ),
    reply(
        "MESH-FETCH-017",
        Kind::Invalid,
        Answer::NotModified {
            sha256: Field::WrongType,
        },
        Expect::Malformed("sha256"),
    ),
    reply(
        "MESH-FETCH-017",
        Kind::Invalid,
        Answer::NotModified {
            sha256: Field::Short,
        },
        Expect::Malformed("sha256"),
    ),
    reply(
        "MESH-FETCH-018",
        Kind::Invalid,
        ok(Field::Right, Field::Other, Field::Right),
        Expect::Corrupt,
    ),
    reply(
        "MESH-FETCH-019",
        Kind::Invalid,
        ok(Field::Right, Field::Right, Field::Missing),
        Expect::Malformed("bytes"),
    ),
    reply(
        "MESH-FETCH-019",
        Kind::Invalid,
        ok(Field::Right, Field::Right, Field::WrongType),
        Expect::Malformed("bytes"),
    ),
    reply(
        "MESH-FETCH-007",
        Kind::Boundary,
        Answer::InvalidPath { rule: Field::Other },
        Expect::InvalidPath("unknown"),
    ),
    reply(
        "MESH-FETCH-007",
        Kind::Invalid,
        Answer::InvalidPath {
            rule: Field::Missing,
        },
        Expect::Malformed("rule"),
    ),
    reply(
        "MESH-FETCH-007",
        Kind::Invalid,
        Answer::InvalidPath {
            rule: Field::WrongType,
        },
        Expect::Malformed("rule"),
    ),
    reply(
        "MESH-FETCH-022",
        Kind::Invalid,
        Answer::TooLarge {
            limit: Field::Missing,
        },
        Expect::Malformed("limit"),
    ),
    reply(
        "MESH-FETCH-022",
        Kind::Invalid,
        Answer::TooLarge {
            limit: Field::WrongType,
        },
        Expect::Malformed("limit"),
    ),
    reply(
        "MESH-FETCH-023",
        Kind::Valid,
        Answer::Extras(Status::NotShared),
        Expect::NotShared,
    ),
    reply(
        "MESH-FETCH-023",
        Kind::Valid,
        Answer::Extras(Status::TooLarge),
        Expect::TooLarge,
    ),
    reply(
        "MESH-FETCH-023",
        Kind::Valid,
        Answer::Extras(Status::NotModified),
        Expect::NotModified,
    ),
    reply(
        "MESH-FETCH-023",
        Kind::Valid,
        Answer::Extras(Status::InvalidPath),
        Expect::InvalidPath("segment"),
    ),
    Vector {
        id: "MESH-FETCH-006",
        kind: Kind::Invalid,
        case: Case::EscapingLink,
    },
    Vector {
        id: "MESH-SHARE-006",
        kind: Kind::Valid,
        case: Case::DenyThroughAlias,
    },
    Vector {
        id: "MESH-SHARE-012",
        kind: Kind::Invalid,
        case: Case::PlantedShareFile,
    },
    Vector {
        id: "MESH-SHARE-019",
        kind: Kind::Valid,
        case: Case::Lend { acknowledged: true },
    },
    Vector {
        id: "MESH-SHARE-019",
        kind: Kind::Invalid,
        case: Case::Lend {
            acknowledged: false,
        },
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
    use super::{Answer, Case, Expect, Field, Status, VECTORS, Vector};
    use crate::config::WORKSPACE_COYOTE_DIR_NAME;
    use crate::config::mesh_config::MAX_FETCH_FILE_BYTES;
    use crate::hooks::HookEvent;
    use crate::mesh::access::{AccessDecisionReport, GrantKind};
    use crate::mesh::envoy::EnvoySink;
    use crate::mesh::events::{RecordedFire, RecordingHookSink, env_value};
    use crate::mesh::fetch::{FetchError, Fetched};
    use crate::mesh::grants::GrantRecord;
    use crate::mesh::hex_lower;
    use crate::mesh::message::{
        Disposition, OutboundPeer, PEER_WIRE_VERSION, PeerBody, PeerKind, PeerVia, RawPart,
        SendError, from_r3_body,
    };
    use crate::mesh::node::MeshSlot;
    use crate::mesh::r3::network::timed_out;
    use crate::mesh::r3::{ACCESS_PATH, FETCH_PATH, LIST_PATH, Reply};
    use crate::mesh::shares::{Mutation, PeerRef, Served, ShareLocations, ShareSet, WriteScope};
    use crate::mesh::test_support::{
        NodePair, RecordingEnvoy, ResponderScript, TempDir, access_body, fetch_body, hook_sink_for,
        installed_slot, share_docs_from_a, short_options, trusting_b, wait_until, wire_field,
        wire_status,
    };

    use futures_util::FutureExt;
    use rand_core::OsRng;
    use rmpv::Value;
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::panic::AssertUnwindSafe;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const DEFAULT_GRANT_TTL_SECS: f64 = 900.0;
    const PEER_LIMIT: u64 = 4_194_304;
    const UNSERVED_PATH: &str = "src/x.rs";
    const REASON: &str = "need the entry point";

    /// Which test drives a row; each group is one `#[tokio::test]`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Group {
        Cycle,
        Access,
        Requester,
        Symlinks,
        Lending,
    }

    fn group(case: &Case) -> Group {
        match case {
            Case::Listing
            | Case::ServedByAllow
            | Case::FiledPending
            | Case::GrantBeforeReply
            | Case::DecisionReply
            | Case::GrantServes { .. }
            | Case::GrantHiddenFromListing
            | Case::NoSideChannel => Group::Cycle,
            Case::GrantedAtOnce | Case::UnknownRequester | Case::DecisionSendFails => Group::Access,
            Case::FetchReply { .. } => Group::Requester,
            Case::EscapingLink | Case::DenyThroughAlias | Case::PlantedShareFile => Group::Symlinks,
            Case::Lend { .. } => Group::Lending,
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

    fn wire_map(entries: Vec<(&str, Value)>) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    fn listing_body() -> Value {
        wire_map(vec![("v", Value::from(PEER_WIRE_VERSION))])
    }

    fn entries_of(listing: &Value) -> Vec<&str> {
        wire_field(listing, "entries")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(|entry| wire_field(entry, "path").and_then(Value::as_str).unwrap())
            .collect()
    }

    fn fires_of(fired: &[RecordedFire], event: HookEvent) -> Vec<&Vec<(&'static str, String)>> {
        fired
            .iter()
            .filter(|(fired, _)| *fired == event)
            .map(|(_, envs)| envs)
            .collect()
    }

    fn unix_now() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    /// Node A installed into a slot, with B introduced as a trusted peer, a hook sink on
    /// both and A's global share list allowing `docs/**` over a fresh workspace.
    struct Live {
        pair: NodePair,
        slot: Arc<MeshSlot>,
        sink: Arc<RecordingHookSink>,
        workspace: TempDir,
        b_hex: String,
    }

    impl Live {
        async fn start(tag: &str) -> Self {
            let pair = NodePair::start_with(tag, |_| {}, trusting_b).await;
            pair.introduce_b_to_a().await;
            let (slot, _) = installed_slot(&pair);
            let sink = hook_sink_for(&pair, &slot);
            let workspace = share_docs_from_a(&pair, &slot, &format!("{tag}-root"));
            let b_hex = pair
                .responder
                .desc
                .address_hash
                .to_hex_string()
                .to_lowercase();
            Self {
                pair,
                slot,
                sink,
                workspace,
                b_hex,
            }
        }

        fn grants(&self) -> Vec<GrantRecord> {
            self.pair.node_a.serving().grants().list().unwrap()
        }

        fn inbound_ids(&self) -> Vec<String> {
            self.slot
                .inbound_store()
                .unwrap()
                .list(SystemTime::now())
                .unwrap()
                .into_iter()
                .map(|record| record.id)
                .collect()
        }

        async fn b_asks_a(&self, path: &str, body: Value) -> Value {
            self.pair.b_asks_a(path, body, short_options()).await.value
        }
    }

    /// Everything B and A's stores showed over one list, fetch, access, grant and fetch
    /// cycle; the Cycle rows each read their part of it.
    struct Cycle {
        docs: Vec<u8>,
        digest: [u8; 32],
        unserved: Vec<u8>,
        b_hex: String,
        listed: Value,
        served: Value,
        asked: Value,
        report: AccessDecisionReport,
        grants_after_decision: Vec<GrantRecord>,
        decision: PeerBody,
        on_grant: Value,
        spent: Value,
        listed_after_grant: Value,
        fired: Vec<RecordedFire>,
        envoy_jobs: usize,
    }

    async fn cycle(live: &Live) -> Cycle {
        let envoy = RecordingEnvoy::new(true, false);
        live.slot.set_envoy(envoy.clone() as Arc<dyn EnvoySink>);
        let docs = b"# a\n".to_vec();
        fs::write(live.workspace.path.join("docs").join("a.md"), &docs).unwrap();
        let digest: [u8; 32] = Sha256::digest(&docs).into();
        let unserved = fs::read(live.workspace.path.join(UNSERVED_PATH)).unwrap();

        let listed = live.b_asks_a(LIST_PATH, listing_body()).await;
        let served = live
            .b_asks_a(FETCH_PATH, fetch_body("docs/a.md", None))
            .await;
        let asked = live
            .b_asks_a(ACCESS_PATH, access_body("acc-1", &[UNSERVED_PATH], REASON))
            .await;

        live.pair.recorder_b.queue(ResponderScript::Acknowledge);
        let report = live
            .slot
            .access()
            .grant("acc-1", GrantKind::OneOff { ttl: None })
            .await
            .unwrap();
        let grants_after_decision = live.grants();
        let decision = from_r3_body(&live.pair.recorder_b.last_body()).unwrap();

        let on_grant = live
            .b_asks_a(FETCH_PATH, fetch_body(UNSERVED_PATH, None))
            .await;
        let spent = live
            .b_asks_a(FETCH_PATH, fetch_body(UNSERVED_PATH, None))
            .await;
        let listed_after_grant = live.b_asks_a(LIST_PATH, listing_body()).await;

        wait_until("both served fetches to fire mesh.fetch.served", || {
            live.sink
                .snapshot()
                .iter()
                .filter(|(event, _)| *event == HookEvent::MeshFetchServed)
                .count()
                >= 2
        })
        .await;
        Cycle {
            docs,
            digest,
            unserved,
            b_hex: live.b_hex.clone(),
            listed,
            served,
            asked,
            report,
            grants_after_decision,
            decision,
            on_grant,
            spent,
            listed_after_grant,
            fired: live.sink.drain(),
            envoy_jobs: envoy.jobs.lock().len(),
        }
    }

    fn check_cycle(case: Case, run: &Cycle) {
        match case {
            Case::Listing => assert_eq!(entries_of(&run.listed), ["docs/a.md"]),
            Case::ServedByAllow => {
                assert_eq!(wire_status(&run.served), "ok");
                assert_eq!(
                    wire_field(&run.served, "size").and_then(Value::as_u64),
                    Some(run.docs.len() as u64)
                );
                assert_eq!(
                    wire_field(&run.served, "sha256"),
                    Some(&Value::Binary(run.digest.to_vec()))
                );
                assert_eq!(
                    wire_field(&run.served, "bytes"),
                    Some(&Value::Binary(run.docs.clone()))
                );
            }
            Case::FiledPending => {
                assert_eq!(wire_status(&run.asked), "pending");
                assert_eq!(
                    wire_field(&run.asked, "id").and_then(Value::as_str),
                    Some("acc-1")
                );
                assert_eq!(run.envoy_jobs, 0, "the envoy was handed the request");
            }
            Case::GrantBeforeReply => {
                assert_eq!(run.report.via, PeerVia::Direct);
                let grants: Vec<&GrantRecord> = run
                    .grants_after_decision
                    .iter()
                    .filter(|grant| grant.id == "acc-1")
                    .collect();
                assert_eq!(grants.len(), 1, "{:?}", run.grants_after_decision);
                assert_eq!(grants[0].peer, run.b_hex);
                assert_eq!(grants[0].paths.len(), 1, "{:?}", grants[0].paths);
                assert_eq!(grants[0].paths[0].path, UNSERVED_PATH);
                assert_eq!(grants[0].paths[0].uses_left, 1);
                let RawPart::Data { data } = &run.decision.parts[0] else {
                    panic!("not a data part: {:?}", run.decision.parts);
                };
                assert_eq!(data["access"]["status"], serde_json::json!("granted"));
            }
            Case::DecisionReply => {
                let decision = &run.decision;
                assert_eq!(decision.kind, PeerKind::Reply);
                assert_eq!(decision.in_reply_to.as_deref(), Some("acc-1"));
                assert_eq!(decision.thread.as_deref(), Some("acc-1"));
                assert_eq!(decision.disposition, Some(Disposition::Answered));
                assert_eq!(decision.fields, None);
                assert_eq!(decision.parts.len(), 1, "{:?}", decision.parts);
                let RawPart::Data { data } = &decision.parts[0] else {
                    panic!("not a data part: {:?}", decision.parts);
                };
                let expires = data["access"]["expires"]
                    .as_f64()
                    .unwrap_or_else(|| panic!("no expires in {data}"));
                assert_eq!(
                    *data,
                    serde_json::json!({ "access": { "status": "granted", "expires": expires } })
                );
                assert!(
                    decision
                        .content
                        .starts_with("access granted: 1 path until "),
                    "{}",
                    decision.content
                );
                assert!(!decision.content.contains("x.rs"), "{}", decision.content);
            }
            Case::GrantServes { spent: false } => {
                assert_eq!(wire_status(&run.on_grant), "ok");
                assert_eq!(
                    wire_field(&run.on_grant, "bytes"),
                    Some(&Value::Binary(run.unserved.clone()))
                );
            }
            Case::GrantServes { spent: true } => {
                assert_eq!(wire_status(&run.spent), "not_shared");
            }
            Case::GrantHiddenFromListing => {
                assert_eq!(entries_of(&run.listed_after_grant), ["docs/a.md"]);
            }
            Case::NoSideChannel => {
                let served = fires_of(&run.fired, HookEvent::MeshFetchServed);
                assert_eq!(served.len(), 2, "{:?}", run.fired);
                let requested = fires_of(&run.fired, HookEvent::MeshAccessRequested);
                assert_eq!(requested.len(), 1, "{:?}", run.fired);
                assert_eq!(
                    env_value(requested[0], "COYOTE_MESH_ACCESS_ID"),
                    Some("acc-1")
                );
                assert_eq!(env_value(requested[0], "COYOTE_MESH_PATH_COUNT"), Some("1"));
                let decided = fires_of(&run.fired, HookEvent::MeshAccessDecided);
                assert_eq!(decided.len(), 1, "{:?}", run.fired);
                assert_eq!(
                    env_value(decided[0], "COYOTE_MESH_DECISION"),
                    Some("granted")
                );
                for (event, envs) in &run.fired {
                    for (key, value) in envs {
                        assert!(
                            !value.contains("x.rs") && !value.contains(REASON),
                            "{event:?} {key}={value}"
                        );
                    }
                }
            }
            other => panic!("not a Cycle case: {other:?}"),
        }
    }

    async fn drive_access(live: &Live, case: Case) {
        match case {
            Case::GrantedAtOnce => {
                fs::write(live.workspace.path.join("docs").join("a.md"), b"# a\n").unwrap();
                let before = unix_now();
                let asked = live
                    .b_asks_a(ACCESS_PATH, access_body("acc-1", &["docs/a.md"], ""))
                    .await;
                assert_eq!(wire_status(&asked), "granted");
                let expires = wire_field(&asked, "expires")
                    .and_then(Value::as_f64)
                    .unwrap();
                assert!(
                    expires >= before + DEFAULT_GRANT_TTL_SECS - 1.0
                        && expires < before + DEFAULT_GRANT_TTL_SECS + 60.0,
                    "{expires}"
                );
                assert!(!live.inbound_ids().contains(&"acc-1".to_string()));
                assert!(live.grants().iter().all(|grant| grant.id != "acc-1"));
            }
            Case::UnknownRequester => {
                let stranger = TransportIdentity::new_from_rand(OsRng);
                let err = live
                    .pair
                    .client_b
                    .request(
                        &live.pair.responder.transport,
                        &stranger,
                        &live.pair.a_desc,
                        ACCESS_PATH,
                        live.pair.responder.envelope(access_body(
                            "acc-stranger",
                            &["docs/a.md"],
                            "",
                        )),
                        short_options(),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(err, timed_out(ACCESS_PATH));
                assert!(!live.inbound_ids().contains(&"acc-stranger".to_string()));
            }
            Case::DecisionSendFails => {
                let asked = live
                    .b_asks_a(ACCESS_PATH, access_body("acc-2", &[UNSERVED_PATH], REASON))
                    .await;
                assert_eq!(wire_status(&asked), "pending");
                live.pair
                    .recorder_b
                    .queue(ResponderScript::Reply(Reply::Value(Value::Nil)));

                live.slot
                    .access()
                    .grant("acc-2", GrantKind::OneOff { ttl: None })
                    .await
                    .unwrap_err();

                assert!(live.grants().iter().all(|grant| grant.id != "acc-2"));
                assert!(live.inbound_ids().contains(&"acc-2".to_string()));
                assert_eq!(live.pair.recorder_b.seen_count(), 1, "one send, no retry");
            }
            other => panic!("not an Access case: {other:?}"),
        }
    }

    /// The scripted reply for `answer` about a file of `bytes`; a key written `None` is
    /// left out.
    fn fetch_reply(answer: Answer, bytes: &[u8], digest: [u8; 32]) -> Value {
        let size = |field: Field| match field {
            Field::Right => Some(Value::from(bytes.len() as u64)),
            Field::Missing => None,
            Field::WrongType => Some(Value::from(bytes.len().to_string())),
            Field::Short | Field::Other => Some(Value::from(bytes.len() as u64 + 1)),
        };
        let sha256 = |field: Field| match field {
            Field::Right => Some(Value::Binary(digest.to_vec())),
            Field::Missing => None,
            Field::WrongType => Some(Value::from(hex_lower(&digest))),
            Field::Short => Some(Value::Binary(digest[..31].to_vec())),
            Field::Other => Some(Value::Binary(vec![0x11; 32])),
        };
        let body = |field: Field| match field {
            Field::Right => Some(Value::Binary(bytes.to_vec())),
            Field::Missing => None,
            Field::WrongType | Field::Short | Field::Other => {
                Some(Value::from(String::from_utf8_lossy(bytes).into_owned()))
            }
        };
        let limit = |field: Field| match field {
            Field::Right => Some(Value::from(PEER_LIMIT)),
            Field::Missing => None,
            Field::WrongType | Field::Short | Field::Other => {
                Some(Value::from(PEER_LIMIT.to_string()))
            }
        };
        let rule = |field: Field| match field {
            Field::Right => Some(Value::from("segment")),
            Field::Missing => None,
            Field::WrongType => Some(Value::from(7u64)),
            Field::Short | Field::Other => Some(Value::from("made-up")),
        };
        let v = Some(Value::from(PEER_WIRE_VERSION));
        let status = |word: &str| Some(Value::from(word));
        let entries: Vec<(&str, Option<Value>)> = match answer {
            Answer::NotAMap => return Value::from(7u64),
            Answer::Version(version) => vec![
                ("v", version.map(Value::from)),
                ("status", status("not_shared")),
            ],
            Answer::NoStatus => vec![("v", v)],
            Answer::StatusNotText => vec![("v", v), ("status", Some(Value::from(1u64)))],
            Answer::OtherStatus => vec![("v", v), ("status", status("served"))],
            Answer::Ok {
                size: size_field,
                sha256: sha_field,
                bytes: bytes_field,
            } => vec![
                ("v", v),
                ("status", status("ok")),
                ("size", size(size_field)),
                ("sha256", sha256(sha_field)),
                ("bytes", body(bytes_field)),
            ],
            Answer::NotModified { sha256: field } => vec![
                ("v", v),
                ("status", status("not_modified")),
                ("sha256", sha256(field)),
            ],
            Answer::TooLarge { limit: field } => vec![
                ("v", v),
                ("status", status("too_large")),
                ("limit", limit(field)),
            ],
            Answer::InvalidPath { rule: field } => vec![
                ("v", v),
                ("status", status("invalid_path")),
                ("rule", rule(field)),
            ],
            // Every key of every status, each well formed, so the status' own key is
            // read and the rest are the other statuses' keys to ignore.
            Answer::Extras(which) => vec![
                ("v", v),
                (
                    "status",
                    status(match which {
                        Status::NotShared => "not_shared",
                        Status::TooLarge => "too_large",
                        Status::NotModified => "not_modified",
                        Status::InvalidPath => "invalid_path",
                    }),
                ),
                ("size", size(Field::Right)),
                ("sha256", sha256(Field::Right)),
                ("bytes", body(Field::Right)),
                ("limit", limit(Field::Right)),
                ("rule", rule(Field::Right)),
                ("next", Some(Value::Binary(vec![0xaa; 8]))),
                ("entries", Some(Value::Array(Vec::new()))),
                ("retry_after", Some(Value::from(1u64))),
            ],
        };
        wire_map(
            entries
                .into_iter()
                .filter_map(|(key, value)| Some((key, value?)))
                .collect(),
        )
    }

    async fn drive_requester(pair: &NodePair, answer: Answer, expect: Expect) {
        let bytes = b"# c\n".to_vec();
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let reply = fetch_reply(answer, &bytes, digest);
        pair.recorder_b
            .queue(ResponderScript::Reply(Reply::Value(reply.clone())));

        let fetched = pair
            .node_a
            .fetch_file(&pair.responder.desc, "docs/c.md", None)
            .await;

        match (expect, fetched) {
            (Expect::Malformed(key), Err(FetchError::Malformed(read))) => {
                assert_eq!(read, key, "{reply:?}");
            }
            (Expect::Corrupt, Err(FetchError::Corrupt))
            | (Expect::UnknownStatus, Err(FetchError::UnknownStatus)) => {}
            (Expect::NotShared, Ok(Fetched::NotShared)) => {}
            (Expect::TooLarge, Ok(Fetched::TooLarge { limit })) => {
                assert_eq!(limit, PEER_LIMIT);
            }
            (Expect::NotModified, Ok(Fetched::NotModified { sha256 })) => {
                assert_eq!(sha256, digest);
            }
            (Expect::InvalidPath(rule), Ok(Fetched::InvalidPath { rule: read })) => {
                assert_eq!(read, rule);
            }
            (expect, fetched) => panic!("expected {expect:?}, read {fetched:?} from {reply:?}"),
        }
    }

    /// A share root with its config directory beside it, for the rows that judge the
    /// share set in-process.
    struct Root {
        _tmp: TempDir,
        config_dir: PathBuf,
        root: PathBuf,
    }

    impl Root {
        fn new(tag: &str) -> Self {
            let tmp = TempDir::new(tag);
            let config_dir = tmp.path.join("config");
            let root = tmp.path.join("workspace");
            fs::create_dir_all(&root).unwrap();
            Self {
                _tmp: tmp,
                config_dir,
                root,
            }
        }

        fn locations(&self) -> ShareLocations {
            ShareLocations::with_dir_name(
                &self.config_dir,
                &self.root,
                WORKSPACE_COYOTE_DIR_NAME.to_string(),
            )
        }

        fn load(&self) -> ShareSet {
            ShareSet::load(self.locations())
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, relative).unwrap();
            path
        }

        fn link(&self, relative: &str, target: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, path).unwrap();
        }
    }

    fn allow(pattern: &str) -> Mutation {
        Mutation::Allow {
            pattern: pattern.to_string(),
            peer: None,
        }
    }

    fn served(set: &ShareSet, wire_text: &str) -> Served {
        let (identity, destination) = ("1a".repeat(16), "2b".repeat(16));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        set.is_served(&peer, wire_text, false, MAX_FETCH_FILE_BYTES, None)
    }

    fn drive_symlinks(case: Case) {
        match case {
            Case::EscapingLink => {
                let fx = Root::new("live-escaping-link");
                fs::write(fx._tmp.path.join("outside.md"), "outside").unwrap();
                fx.link("docs/escape", "../../outside.md");
                let mut set = fx.load();
                set.apply(allow("**"), WriteScope::Global).unwrap();

                let escaped = served(&set, "docs/escape");
                let missing = served(&set, "docs/missing.md");

                assert!(matches!(escaped, Served::NotShared), "{escaped:?}");
                assert!(matches!(missing, Served::NotShared), "{missing:?}");
            }
            Case::DenyThroughAlias => {
                let fx = Root::new("live-alias-deny");
                fx.file("src/secret.md");
                fx.file("src/other.md");
                fx.link("pub/s", "../src/secret.md");
                let mut set = fx.load();
                set.apply(allow("**"), WriteScope::Global).unwrap();
                set.apply(
                    Mutation::Deny {
                        pattern: "src/secret.md".to_string(),
                    },
                    WriteScope::Global,
                )
                .unwrap();

                let aliased = served(&set, "pub/s");
                let other = served(&set, "src/other.md");

                assert!(matches!(aliased, Served::NotShared), "{aliased:?}");
                assert!(matches!(other, Served::File(_)), "{other:?}");
            }
            Case::PlantedShareFile => {
                let fx = Root::new("live-planted-share-file");
                let sentinel = fx._tmp.path.join("sentinel.yaml");
                fs::write(&sentinel, "version: 1\n").unwrap();
                let global = fx.locations().global;
                fs::create_dir_all(global.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(&sentinel, &global).unwrap();
                let mut set = fx.load();

                let err = set
                    .apply(allow("docs/**"), WriteScope::Global)
                    .unwrap_err()
                    .to_string();

                assert!(err.contains("is a symlink"), "{err}");
                assert_eq!(fs::read_to_string(&sentinel).unwrap(), "version: 1\n");
                assert!(
                    fs::symlink_metadata(&global)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(
                    fs::read_dir(global.parent().unwrap()).unwrap().count(),
                    1,
                    "no temp is left beside the planted link"
                );
            }
            other => panic!("not a Symlinks case: {other:?}"),
        }
    }

    fn lending(id: &str) -> OutboundPeer {
        OutboundPeer {
            kind: PeerKind::Message,
            id: id.to_string(),
            in_reply_to: None,
            title: None,
            content: "see the attached".to_string(),
            fields: None,
            parts: vec![RawPart::File {
                name: "docs/big.md".to_string(),
                size: 4_096,
                sha256: [7u8; 32],
                bytes: None,
                reference: Some("docs/big.md".to_string()),
            }],
            thread: None,
            disposition: None,
            retry_after: None,
        }
    }

    async fn drive_lending(live: &Live, acknowledged: bool) {
        if acknowledged {
            live.pair.recorder_b.queue(ResponderScript::Acknowledge);
            let sent = live
                .pair
                .node_a
                .send_peer_lending_reference(&live.b_hex, &lending("m-1"))
                .await
                .unwrap();

            assert_eq!(sent.via, PeerVia::Direct);
            let grants = live.grants();
            let lent: Vec<&GrantRecord> = grants.iter().filter(|grant| grant.id == "m-1").collect();
            assert_eq!(lent.len(), 1, "{grants:?}");
            assert_eq!(lent[0].peer, live.b_hex);
            assert_eq!(lent[0].paths.len(), 1, "{:?}", lent[0].paths);
            assert_eq!(lent[0].paths[0].path, "docs/big.md");
            assert_eq!(lent[0].paths[0].uses_left, lent[0].paths[0].uses);
            let heard = from_r3_body(&live.pair.recorder_b.last_body()).unwrap();
            assert_eq!(heard.id, "m-1");
            assert!(
                matches!(
                    &heard.parts[..],
                    [RawPart::File {
                        bytes: None,
                        reference: Some(reference),
                        ..
                    }] if reference == "docs/big.md"
                ),
                "{:?}",
                heard.parts
            );
        } else {
            live.pair
                .recorder_b
                .queue(ResponderScript::Reply(Reply::Value(Value::Nil)));
            let err = live
                .pair
                .node_a
                .send_peer_lending_reference(&live.b_hex, &lending("m-2"))
                .await
                .unwrap_err()
                .to_string();

            assert_eq!(err, SendError::NotAcknowledged.to_string());
            let grants = live.grants();
            assert!(grants.iter().all(|grant| grant.id != "m-2"), "{grants:?}");
        }
    }

    /// Runs every row of `wanted` over one fixture for the group and reports the failures
    /// together, each naming the row's id and family.
    async fn run(wanted: Group) {
        let rows: Vec<&Vector> = VECTORS
            .iter()
            .filter(|row| group(&row.case) == wanted)
            .collect();
        let ran = rows.len();
        assert!(ran > 0, "no {wanted:?} vectors");
        let mut failures = Vec::new();
        let mut record = |row: &Vector, outcome: Result<(), Box<dyn std::any::Any + Send>>| {
            if let Err(payload) = outcome {
                failures.push(format!(
                    "{} [{}]: {}",
                    row.id,
                    row.case.family(),
                    panic_message(payload)
                ));
            }
        };
        match wanted {
            Group::Cycle => {
                let live = Live::start("live-cycle").await;
                let observed = cycle(&live).await;
                for row in rows {
                    record(
                        row,
                        std::panic::catch_unwind(AssertUnwindSafe(|| {
                            check_cycle(row.case, &observed)
                        })),
                    );
                }
                live.pair.stop_node_a().await;
            }
            Group::Access => {
                let live = Live::start("live-access").await;
                for row in rows {
                    let outcome = AssertUnwindSafe(drive_access(&live, row.case))
                        .catch_unwind()
                        .await;
                    record(row, outcome);
                }
                live.pair.stop_node_a().await;
            }
            Group::Requester => {
                let pair = NodePair::start_with("live-requester", |_| {}, trusting_b).await;
                pair.introduce_b_to_a().await;
                for row in rows {
                    let Case::FetchReply { answer, expect } = row.case else {
                        panic!("not a Requester case: {:?}", row.case);
                    };
                    let outcome = AssertUnwindSafe(drive_requester(&pair, answer, expect))
                        .catch_unwind()
                        .await;
                    record(row, outcome);
                }
                pair.stop_node_a().await;
            }
            Group::Symlinks => {
                for row in rows {
                    record(row, std::panic::catch_unwind(|| drive_symlinks(row.case)));
                }
            }
            Group::Lending => {
                let live = Live::start("live-lending").await;
                for row in rows {
                    let Case::Lend { acknowledged } = row.case else {
                        panic!("not a Lending case: {:?}", row.case);
                    };
                    let outcome = AssertUnwindSafe(drive_lending(&live, acknowledged))
                        .catch_unwind()
                        .await;
                    record(row, outcome);
                }
                live.pair.stop_node_a().await;
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {ran} {wanted:?} vectors failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_list_access_grant_and_fetch_cycle_holds_over_a_live_pair() {
        run(Group::Cycle).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn access_admission_and_decisions_hold_over_a_live_pair() {
        run(Group::Access).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_requester_reads_fetch_replies_as_section_10_15_mandates() {
        run(Group::Requester).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn symlinks_never_widen_what_is_served_or_written() {
        run(Group::Symlinks).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reference_attachment_lends_a_grant_only_for_a_message_the_peer_heard() {
        run(Group::Lending).await;
    }

    /// Every group a row lands in has one of the `#[tokio::test]`s above driving it: a row
    /// that counts for coverage but is never driven would let the table over-report what
    /// it asserts.
    #[test]
    fn every_row_is_driven() {
        const DRIVEN: &[Group] = &[
            Group::Cycle,
            Group::Access,
            Group::Requester,
            Group::Symlinks,
            Group::Lending,
        ];
        for row in VECTORS {
            let group = group(&row.case);
            assert!(
                DRIVEN.contains(&group),
                "{} [{group:?}] has no test",
                row.id
            );
        }
    }
}
