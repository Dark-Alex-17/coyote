use super::dispatch::DispatchError;
use super::error::{R3Error, RefusalCode};
use super::frame::{
    Envelope, NAME_HASH_LEN, OriginName, PathHash, RequestFrame, RequestId, ResponseFrame,
};

use rmpv::Value;
use rns_transport::destination::link::{Link, unpack_response_envelope};

fn sixteen(byte: u8) -> Value {
    Value::Binary(vec![byte; 16])
}

fn packed(value: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &value).unwrap();
    bytes
}

#[test]
fn request_frame_matches_upstream_link_request_byte_for_byte() {
    let data = Value::Map(vec![
        (Value::from("name"), Value::from("Alex")),
        (Value::from("n"), Value::from(3)),
    ]);
    let upstream = Link::request_payload("/status", data.clone()).unwrap();

    let frame = RequestFrame::decode(&upstream.packed).unwrap();

    assert_eq!(frame.path_hash, PathHash::of("/status"));
    assert_eq!(frame.path_hash, PathHash::from(upstream.path_hash));
    assert_eq!(frame.data, data);
    assert_eq!(frame.encode(), upstream.packed);
    assert_eq!(
        RequestId::of_packed(&upstream.packed),
        RequestId::from(upstream.resource_request_id)
    );
}

#[test]
fn response_frame_is_accepted_by_upstream_envelope_unpacker() {
    let request_id = RequestId::from([7u8; 16]);
    let frame = ResponseFrame {
        request_id,
        data: Value::Array(vec![Value::from(1), Value::Nil, Value::from("ok")]),
    };
    let bytes = frame.encode();

    let (upstream_id, upstream_value) = unpack_response_envelope(&bytes).unwrap();

    assert_eq!(RequestId::from(upstream_id), request_id);
    assert_eq!(upstream_value, frame.data);
    assert_eq!(ResponseFrame::decode(&bytes).unwrap(), frame);
}

#[test]
fn request_frame_layout_is_fixed_width_apart_from_the_body() {
    let bytes = RequestFrame::new("/p", Value::Nil).encode();

    // array(3) + float64 + bin8(16) + nil
    assert_eq!(bytes.len(), 1 + 9 + 18 + 1);
    assert_eq!(bytes[0], 0x93);
    assert_eq!(bytes[1], 0xcb);
    assert_eq!(&bytes[10..12], &[0xc4, 0x10]);
    assert_eq!(bytes[28], 0xc0);
    assert_eq!(RequestFrame::decode(&bytes).unwrap().data, Value::Nil);
}

#[test]
fn decode_refuses_malformed_frames() {
    let mut trailing = RequestFrame::new("/p", Value::Nil).encode();
    trailing.push(0xc0);
    assert!(matches!(
        RequestFrame::decode(&trailing),
        Err(R3Error::Decode(reason)) if reason.contains("trailing")
    ));

    let two_element_request = packed(Value::Array(vec![Value::F64(1.0), sixteen(1)]));
    assert!(matches!(
        RequestFrame::decode(&two_element_request),
        Err(R3Error::Decode(reason)) if reason.contains("2 elements")
    ));

    let short_hash_request = packed(Value::Array(vec![
        Value::F64(1.0),
        Value::Binary(vec![1; 4]),
        Value::Nil,
    ]));
    assert!(matches!(
        RequestFrame::decode(&short_hash_request),
        Err(R3Error::Decode(reason)) if reason.contains("4 bytes")
    ));

    let integer_time = packed(Value::Array(vec![Value::from(1), sixteen(1), Value::Nil]));
    assert!(matches!(
        RequestFrame::decode(&integer_time),
        Err(R3Error::Decode(reason)) if reason.contains("float64")
    ));

    let three_element_response = packed(Value::Array(vec![sixteen(2), Value::Nil, Value::Nil]));
    assert!(matches!(
        ResponseFrame::decode(&three_element_response),
        Err(R3Error::Decode(reason)) if reason.contains("3 elements")
    ));

    let short_hash_response = packed(Value::Array(vec![Value::Binary(vec![2; 4]), Value::Nil]));
    assert!(matches!(
        ResponseFrame::decode(&short_hash_response),
        Err(R3Error::Decode(reason)) if reason.contains("4 bytes")
    ));

    let string_hash_response = packed(Value::Array(vec![
        Value::from("0123456789abcdef"),
        Value::Nil,
    ]));
    assert!(matches!(
        ResponseFrame::decode(&string_hash_response),
        Err(R3Error::Decode(reason)) if reason.contains("not a bin")
    ));

    assert!(matches!(
        RequestFrame::decode(&[0x93, 0xcb]),
        Err(R3Error::Decode(_))
    ));
}

#[test]
fn refusal_codes_round_trip_the_wire_and_reject_other_values() {
    let codes = [
        RefusalCode::NoIdentity,
        RefusalCode::NoAccess,
        RefusalCode::InvalidKey,
        RefusalCode::InvalidData,
        RefusalCode::InvalidStamp,
        RefusalCode::Throttled,
        RefusalCode::NotFound,
        RefusalCode::Timeout,
    ];
    for code in codes {
        assert_eq!(RefusalCode::from_wire(&code.to_wire()), Some(code));
    }
    assert_eq!(
        packed(RefusalCode::NoIdentity.to_wire()),
        vec![0xcc, 0xf0],
        "LXMPeer.ERROR_NO_IDENTITY packs as msgpack uint8"
    );
    assert_eq!(packed(RefusalCode::NoAccess.to_wire()), vec![0xcc, 0xf1]);
    assert_eq!(RefusalCode::from_wire(&Value::from(0xf2)), None);
    assert_eq!(RefusalCode::from_wire(&Value::from(1)), None);
    assert_eq!(RefusalCode::from_wire(&Value::from(-0xf0)), None);
    assert_eq!(RefusalCode::from_wire(&Value::Binary(vec![0xf0])), None);
    assert_eq!(RefusalCode::from_wire(&Value::Nil), None);
    assert_ne!(
        R3Error::Refused(RefusalCode::NoIdentity),
        R3Error::Refused(RefusalCode::NoAccess)
    );
}

#[test]
fn dispatch_errors_round_trip_as_maps_and_never_read_as_refusal_codes() {
    let errors = [
        DispatchError::UnknownPath {
            path_hash: PathHash::of("/nope").to_hex_string(),
        },
        DispatchError::NoProvider {
            path: "/status".to_string(),
        },
    ];
    for error in errors {
        let value = error.to_value();
        assert_eq!(DispatchError::from_value(&value), Some(error));
        assert_eq!(RefusalCode::from_wire(&value), None);
    }
    assert_eq!(
        DispatchError::UnknownPath {
            path_hash: "ab".repeat(16)
        }
        .to_value(),
        Value::Map(vec![
            (Value::from("error"), Value::from("unknown_path")),
            (Value::from("path_hash"), Value::from("ab".repeat(16))),
        ])
    );
    assert_eq!(DispatchError::from_value(&Value::Nil), None);
    assert_eq!(
        DispatchError::from_value(&RefusalCode::NoAccess.to_wire()),
        None
    );
    assert_eq!(
        DispatchError::from_value(&Value::Map(vec![(
            Value::from("error"),
            Value::from("unknown_path")
        )])),
        None,
        "an error without its detail is not one of ours"
    );
}

/// Every refusal must be built at one site so the bytes cannot drift between rules. The
/// needle and the test-module markers are assembled at runtime so this test's own text
/// never matches them. Only the trailing test module is stripped: a `#[cfg(test)]` on an
/// import or a helper method must not end the scan early.
///
/// The propagation fetch is the client half: it decodes a refusal a node sent us, so it may
/// name the code only inside an `R3Error::Refused(..)` pattern and must never build one.
#[test]
fn no_access_is_named_at_exactly_one_site_outside_the_error_module() {
    let needle = ["RefusalCode::", "NoAccess"].concat();
    let consumed = ["R3Error::Refused(", &needle, ") =>"].concat();
    let markers = [
        ["#[cfg(test)]", "\nmod tests"].concat(),
        ["#[cfg(test)]", "\npub(crate) mod test_support"].concat(),
    ];
    let mut sites = Vec::new();
    for path in crate::mesh::test_support::rust_sources() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name == "error.rs" || path.ends_with("r3/tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let end = markers
            .iter()
            .filter_map(|marker| source.find(marker))
            .min()
            .unwrap_or(source.len());
        let production = &source[..end];
        if name == "server.rs" {
            assert!(production.contains("code.to_wire()"));
        }
        if name == "node.rs" {
            assert!(production.contains("impl MeshRuntime"));
        }
        let count = production.matches(&needle).count();
        if name == "propagation_fetch.rs" {
            assert!(
                count > 0,
                "the fetch must map the node's access refusal to a remedy"
            );
            assert_eq!(
                count,
                production.matches(&consumed).count(),
                "the fetch may only decode a received refusal, never name the code elsewhere"
            );
            assert!(!production.contains("to_wire"));
            assert!(!production.contains("Reply::Code"));
            continue;
        }
        if count > 0 {
            sites.push((name, count));
        }
    }
    assert_eq!(
        sites,
        vec![("dispatch.rs".to_string(), 1)],
        "the refusal code must be named once, in the dispatcher"
    );
}

#[test]
fn envelope_round_trips_and_rejects_anything_that_names_no_origin() {
    let origin = OriginName([7; NAME_HASH_LEN]);
    let body = Value::from("hello");
    let value = Envelope {
        origin,
        body: body.clone(),
    }
    .into_value();
    assert_eq!(
        value,
        Value::Map(vec![
            (Value::from("name_hash"), Value::Binary(vec![7; 10])),
            (Value::from("body"), body.clone()),
        ])
    );
    let decoded = Envelope::from_value(value).unwrap();
    assert_eq!(decoded.origin, origin);
    assert_eq!(decoded.body, body);

    let with_extra = Envelope::from_value(Value::Map(vec![
        (Value::from("later"), Value::from(1)),
        (Value::from("body"), Value::Nil),
        (Value::from("name_hash"), Value::Binary(vec![7; 10])),
    ]))
    .expect("unknown keys are ignored and order does not matter");
    assert_eq!(with_extra.origin, origin);
    assert_eq!(with_extra.body, Value::Nil);

    let malformed = [
        Value::Nil,
        Value::from("hello"),
        Value::Array(vec![Value::Binary(vec![7; 10]), Value::Nil]),
        Value::Map(vec![]),
        Value::Map(vec![(Value::from("body"), Value::Nil)]),
        Value::Map(vec![(Value::from("name_hash"), Value::Binary(vec![7; 10]))]),
        Value::Map(vec![
            (Value::from("name_hash"), Value::Binary(vec![7; 9])),
            (Value::from("body"), Value::Nil),
        ]),
        Value::Map(vec![
            (Value::from("name_hash"), Value::Binary(vec![7; 11])),
            (Value::from("body"), Value::Nil),
        ]),
        Value::Map(vec![
            (Value::from("name_hash"), Value::from("0707070707")),
            (Value::from("body"), Value::Nil),
        ]),
    ];
    for value in malformed {
        assert!(
            Envelope::from_value(value.clone()).is_none(),
            "{value:?} must not read as an envelope"
        );
    }
}

// Two real nodes over loopback TCP. Unix-only like the node tests: the `MeshRuntime` test
// mints an owner-only identity file, which only unix implements.
#[cfg(unix)]
mod network {
    use super::super::client::{
        DEFAULT_LINK_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, Deadline, R3Client, RequestOptions,
        RequestOutcome, SizeBranch, identify, open_link,
    };
    use super::super::dispatch::{
        AdmittedRequest, DispatchError, Dispatcher, Handler, KNOCK_PATH, KnockEvent, KnockSink,
        LoggingKnockSink, MESSAGE_PATH, ReservedPath, STATUS_PATH,
    };
    use super::super::error::{R3Error, RefusalCode};
    use super::super::frame::{
        Envelope, MAX_R3_PAYLOAD_BYTES, OriginName, PathHash, RequestFrame, RequestId,
        ResponseFrame,
    };
    use super::super::receipt::{ReceiptState, RequestReceipt};
    use super::super::server::{
        Admission, InboundRequest, MAX_CONCURRENT_INBOUND_REQUESTS, R3Server, Reply, RequestHandler,
    };
    use crate::config::{ForkRekey, Session};
    use crate::mesh::announce::AnnounceAppData;
    use crate::mesh::node::{MeshRuntime, MeshSlot, NodeOptions, REKEY_GRACE, SHUTDOWN_GRACE};
    use crate::mesh::test_support::{
        Connector, INTEROP_TIMEOUT, LEGACY_LINK_MTU, Listener, TempDir, TrustList, loopback_relay,
        mesh_paths, private_config, wait_until,
    };
    use crate::mesh::trust::{IdentityStanding, Rule, TrustChange, TrustOptions};
    use crate::mesh::{destination_address, mesh_config_dir};
    use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use rand_core::OsRng;
    use rmpv::Value;
    use rns_transport::PacketContext;
    use rns_transport::destination::link::{Link, LinkEvent, LinkEventData, LinkId};
    use rns_transport::destination::{DestinationDesc, DestinationName, SingleInputDestination};
    use rns_transport::hash::{AddressHash, Hash};
    use rns_transport::identity::Identity;
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use rns_transport::iface::InterfaceSharedConfig;
    use rns_transport::iface::tcp_client::TcpClient;
    use rns_transport::iface::tcp_server::TcpServer;
    use rns_transport::resource::{LINK_PACKET_MDU, ResourceEvent, ResourceEventKind};
    use rns_transport::transport::{AnnounceEvent, Transport};
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant, SystemTime};
    use tokio::sync::broadcast;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    /// How long a dropped `Script::Hang` handler takes to go away; see `Abandoned`.
    const ABANDON_DELAY: Duration = Duration::from_millis(250);
    /// How long a request these tests never answer waits before it gives up.
    const SHORT_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
    /// Bytes of array header, request id and bin32 header around a response frame's body.
    const RESPONSE_FRAME_OVERHEAD: usize = 24;
    /// Bytes of array header, time, path hash and bin32 header around a request frame's body.
    const REQUEST_FRAME_OVERHEAD: usize = 33;

    fn link_deadline() -> Deadline {
        Deadline::after(DEFAULT_LINK_TIMEOUT)
    }

    fn request_deadline() -> Deadline {
        Deadline::after(DEFAULT_REQUEST_TIMEOUT)
    }

    fn fresh_destination_name() -> DestinationName {
        let instance_id = Session::default().ensure_mesh_instance_id().to_string();
        DestinationName::new("coyote", &format!("mesh.{instance_id}"))
    }

    /// What the handler observed about one request, kept in plain data for assertions.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Seen {
        link_id: LinkId,
        request_id: RequestId,
        identity: Option<AddressHash>,
        /// The requester's instance as the dispatcher derived it; `None` when the recorder
        /// stands in for the dispatcher and nothing derived one.
        destination: Option<AddressHash>,
        path_hash: PathHash,
        branch: SizeBranch,
    }

    enum Script {
        Reply(Reply),
        Hang,
    }

    /// Counts hung handlers the server dropped, which is how it cancels them. The drop blocks
    /// for `ABANDON_DELAY` first so a stop that returns without waiting for the drop is
    /// caught: without the delay the drop is quicker than the rest of the teardown.
    struct Abandoned<'a>(&'a AtomicUsize);

    impl Drop for Abandoned<'_> {
        fn drop(&mut self) {
            std::thread::sleep(ABANDON_DELAY);
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Echoes the request body unless a scripted reply is queued; records every request and
    /// every hung handler that was dropped.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<Seen>>,
        script: Mutex<VecDeque<Script>>,
        abandoned: AtomicUsize,
    }

    impl Recorder {
        fn queue(&self, script: Script) {
            self.script.lock().push_back(script);
        }

        fn seen_count(&self) -> usize {
            self.seen.lock().len()
        }

        fn abandoned_count(&self) -> usize {
            self.abandoned.load(Ordering::SeqCst)
        }

        fn last(&self) -> Seen {
            self.seen
                .lock()
                .last()
                .cloned()
                .expect("a request was seen")
        }

        async fn record(&self, seen: Seen, body: Value) -> Reply {
            self.seen.lock().push(seen);
            let next = self.script.lock().pop_front();
            match next {
                Some(Script::Reply(reply)) => reply,
                Some(Script::Hang) => {
                    let _abandoned = Abandoned(&self.abandoned);
                    std::future::pending().await
                }
                None => Reply::Value(body),
            }
        }
    }

    /// In place of the dispatcher, the recorder sees raw bodies: it echoes the body out of
    /// an envelope when the requester sent one and the bytes as they came otherwise.
    #[async_trait]
    impl RequestHandler for Recorder {
        fn admit(&self, _link_id: LinkId, _identity: Option<&Identity>) -> Admission {
            Admission::Admit
        }

        async fn handle(&self, request: InboundRequest) -> Reply {
            let seen = Seen {
                link_id: request.link_id,
                request_id: request.request_id,
                identity: request.identity.map(|identity| identity.address_hash),
                destination: None,
                path_hash: request.path_hash,
                branch: request.branch,
            };
            let body = match Envelope::from_value(request.data.clone()) {
                Some(envelope) => envelope.body,
                None => request.data,
            };
            self.record(seen, body).await
        }
    }

    /// The same recorder behind the dispatcher's seam, on whichever test path it is given.
    #[async_trait]
    impl Handler for Recorder {
        async fn handle(&self, request: AdmittedRequest) -> Reply {
            let seen = Seen {
                link_id: request.link_id,
                request_id: request.request_id,
                identity: Some(request.identity.address_hash),
                destination: Some(request.destination_hash),
                path_hash: request.path_hash,
                branch: request.branch,
            };
            self.record(seen, request.body).await
        }
    }

    /// Never answers and never finishes, without the drop delay `Recorder` adds, for tests
    /// that park many handlers at once.
    #[derive(Default)]
    struct Stall {
        entered: AtomicUsize,
    }

    #[async_trait]
    impl RequestHandler for Stall {
        fn admit(&self, _link_id: LinkId, _identity: Option<&Identity>) -> Admission {
            Admission::Admit
        }

        async fn handle(&self, _request: InboundRequest) -> Reply {
            self.entered.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[derive(Default)]
    struct SpySink {
        knocks: Mutex<Vec<KnockEvent>>,
    }

    impl SpySink {
        fn count(&self) -> usize {
            self.knocks.lock().len()
        }

        fn only(&self) -> (String, String, PathHash, Option<Value>) {
            let knocks = self.knocks.lock();
            assert_eq!(knocks.len(), 1, "exactly one knock");
            let knock = &knocks[0];
            (
                knock.identity_hash.clone(),
                knock.destination_hash.clone(),
                knock.path_hash,
                knock.data.clone(),
            )
        }
    }

    impl KnockSink for SpySink {
        fn knock(&self, knock: KnockEvent) {
            self.knocks.lock().push(knock);
        }
    }

    const TEST_PATH: &str = "/test";

    /// Puts the real dispatcher over `list` in front of `responder`, serving `TEST_PATH`
    /// with `recorder` and knocking into the returned spy.
    fn gate(responder: &Responder, recorder: Arc<Recorder>, list: &TrustList, tag: &str) -> Gate {
        let (trust, tmp) = list.open(tag);
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());
        assert!(dispatcher.register(TEST_PATH, recorder).unwrap().is_none());
        responder.server.set_handler(Arc::new(dispatcher));
        Gate { sink, _tmp: tmp }
    }

    struct Gate {
        sink: Arc<SpySink>,
        _tmp: TempDir,
    }

    /// A `Listener` serving a fresh Coyote destination.
    struct Responder {
        transport: Arc<Transport>,
        server: Arc<R3Server>,
        dest: Arc<tokio::sync::Mutex<SingleInputDestination>>,
        desc: DestinationDesc,
        iface: AddressHash,
        cancel: CancellationToken,
        port: u16,
    }

    impl Responder {
        async fn listen(handler: Arc<dyn RequestHandler>, client_mtu: usize) -> Self {
            Self::listen_on(Arc::new(R3Server::new()), handler, client_mtu).await
        }

        async fn listen_on(
            server: Arc<R3Server>,
            handler: Arc<dyn RequestHandler>,
            client_mtu: usize,
        ) -> Self {
            let Listener {
                transport,
                server,
                dest,
                desc,
                iface,
                cancel,
                port,
            } = Listener::listen(
                server,
                handler,
                client_mtu,
                TransportIdentity::new_from_rand(OsRng),
                fresh_destination_name(),
            )
            .await;
            Self {
                transport,
                server,
                dest,
                desc,
                iface,
                cancel,
                port,
            }
        }

        async fn announce(&self, app_data: Option<&[u8]>) {
            let packet = self.dest.lock().await.announce(OsRng, app_data).unwrap();
            self.transport.send_packet(packet).await;
        }

        fn origin(&self) -> OriginName {
            OriginName::of(&self.desc.name)
        }

        /// `body` as this node sends it when it is the one requesting.
        fn envelope(&self, body: Value) -> Envelope {
            Envelope {
                origin: self.origin(),
                body,
            }
        }

        async fn stop(self) {
            self.cancel.cancel();
            self.transport
                .iface_manager()
                .lock()
                .await
                .stop_interface(self.iface);
        }
    }

    /// A `Connector` with no destination of its own; `origin` is the instance it claims
    /// in every request.
    struct Requester {
        transport: Arc<Transport>,
        identity: TransportIdentity,
        origin: OriginName,
        client: Arc<R3Client>,
        client_task: JoinHandle<()>,
        announces: broadcast::Receiver<AnnounceEvent>,
        iface: AddressHash,
        iface_task: JoinHandle<()>,
        cancel: CancellationToken,
    }

    impl Requester {
        async fn connect(port: u16, mtu: usize) -> Self {
            let Connector {
                transport,
                identity,
                client,
                client_task,
                announces,
                iface,
                iface_task,
                cancel,
            } = Connector::connect(port, mtu).await;
            Self {
                transport,
                identity,
                origin: OriginName::of(&fresh_destination_name()),
                client,
                client_task,
                announces,
                iface,
                iface_task,
                cancel,
            }
        }

        /// Consumes announces until the one for `hash` arrives and returns its description,
        /// which is what a requester links to.
        async fn learn(&mut self, hash: &AddressHash) -> DestinationDesc {
            let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
            loop {
                let event = tokio::time::timeout_at(deadline, self.announces.recv())
                    .await
                    .expect("the requester must hear the responder's announce")
                    .unwrap();
                let desc = event.destination.lock().await.desc;
                if desc.address_hash == *hash {
                    return desc;
                }
            }
        }

        fn envelope(&self, body: Value) -> Envelope {
            Envelope {
                origin: self.origin,
                body,
            }
        }

        async fn request(
            &self,
            desc: &DestinationDesc,
            path: &str,
            data: Value,
        ) -> Result<RequestOutcome, R3Error> {
            self.client
                .request(
                    &self.transport,
                    &self.identity,
                    desc,
                    path,
                    self.envelope(data),
                    RequestOptions::default(),
                )
                .await
        }

        async fn stop(self) {
            self.cancel.cancel();
            // Bounded because a requester the test has wedged with a response-size limit has
            // its transport's handler lock held for good.
            let _ = timeout(SHUTDOWN_GRACE, self.transport.stop_interface(self.iface)).await;
            self.iface_task.abort();
        }
    }

    /// A bare responder and requester joined over a legacy-MTU loopback link.
    async fn pair(handler: Arc<dyn RequestHandler>) -> (Responder, Requester, DestinationDesc) {
        let responder = Responder::listen(handler, LEGACY_LINK_MTU).await;
        let mut requester = Requester::connect(responder.port, LEGACY_LINK_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;
        (responder, requester, desc)
    }

    fn identity_hex(requester: &Requester) -> String {
        requester
            .identity
            .as_identity()
            .address_hash
            .to_hex_string()
    }

    /// The requester's own instance, as the dispatcher derives it from the origin the
    /// requester names and the identity it proves. This is what a trust list keys on.
    fn requester_destination(requester: &Requester) -> AddressHash {
        destination_address(
            &requester.origin.0,
            &requester.identity.as_identity().address_hash,
        )
    }

    fn requester_destination_hex(requester: &Requester) -> String {
        requester_destination(requester).to_hex_string()
    }

    fn short_options() -> RequestOptions {
        RequestOptions {
            request_timeout: SHORT_REQUEST_TIMEOUT,
            ..RequestOptions::default()
        }
    }

    fn timed_out(path: &str) -> R3Error {
        R3Error::Timeout {
            path: path.to_string(),
            after: SHORT_REQUEST_TIMEOUT,
        }
    }

    /// A link the requester has proven its identity on, as the responder sees it.
    async fn identified_link(
        requester: &Requester,
        responder: &Responder,
        desc: &DestinationDesc,
    ) -> Arc<tokio::sync::Mutex<Link>> {
        let transport = &requester.transport;
        let link = open_link(transport, desc, TEST_PATH, link_deadline())
            .await
            .unwrap();
        identify(
            transport,
            &link,
            &requester.identity,
            TEST_PATH,
            link_deadline(),
        )
        .await
        .unwrap();
        wait_until("the responder to record the identity", || {
            responder.server.identified_peer_count() == 1
        })
        .await;
        link
    }

    /// One request over `link` from the requester's own instance, as `Requester::request`
    /// sends it but without opening or identifying a link first.
    async fn request_on(
        requester: &Requester,
        link: &Arc<tokio::sync::Mutex<Link>>,
        path: &str,
        data: Value,
        deadline: Deadline,
    ) -> Result<RequestOutcome, R3Error> {
        requester
            .client
            .request_on_link_with(
                &requester.transport,
                link,
                path,
                requester.envelope(data),
                deadline,
                None,
            )
            .await
    }

    /// The response payloads `events` has carried so far, as the requester's transport
    /// received them.
    fn response_payloads(events: &mut broadcast::Receiver<LinkEventData>) -> Vec<Vec<u8>> {
        let mut payloads = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let LinkEvent::Data(payload) = event.event
                && payload.context() == PacketContext::Response
            {
                payloads.push(payload.as_slice().to_vec());
            }
        }
        payloads
    }

    fn assert_debug_logged(needle: &str) {
        let debugs = debug_snapshot();
        assert!(
            debugs.iter().any(|message| message.contains(needle)),
            "no debug message contains {needle:?}"
        );
    }

    /// A body whose request frame, envelope included, encodes to exactly `target` bytes;
    /// the bin header grows at 256 bytes so the search is over lengths rather than
    /// arithmetic.
    fn request_body_of_encoded_len(origin: OriginName, target: usize) -> Value {
        (0..target)
            .map(|n| Value::Binary(vec![0xab; n]))
            .find(|body| {
                let enveloped = Envelope {
                    origin,
                    body: body.clone(),
                }
                .into_value();
                RequestFrame::new("/echo", enveloped).encode().len() == target
            })
            .unwrap_or_else(|| panic!("no body encodes a {target}-byte request frame"))
    }

    fn response_body_of_encoded_len(target: usize) -> Value {
        (0..target)
            .map(|n| Value::Binary(vec![0xcd; n]))
            .find(|body| {
                ResponseFrame {
                    request_id: RequestId::from([0; 16]),
                    data: body.clone(),
                }
                .encode()
                .len()
                    == target
            })
            .unwrap_or_else(|| panic!("no body encodes a {target}-byte response frame"))
    }

    /// Roughly what a peer card carries: enough to need a resource in either direction.
    fn card() -> Value {
        Value::Map(vec![
            (Value::from("display_name"), Value::from("Alex Clarke")),
            (
                Value::from("session"),
                Value::from("mesh-transport-pairing-session"),
            ),
            (Value::from("objective"), Value::from("o".repeat(320))),
            (
                Value::from("state"),
                Value::Map(vec![
                    (Value::from("phase"), Value::from("implementing")),
                    (Value::from("tasks_done"), Value::from(7)),
                    (Value::from("notes"), Value::from("n".repeat(160))),
                ]),
            ),
        ])
    }

    /// Sends a request the responder is scripted to hang on, with `SHORT_REQUEST_TIMEOUT`,
    /// and returns the in-flight request with what the responder saw of it.
    async fn hanging_request(
        requester: &Requester,
        recorder: &Recorder,
        desc: &DestinationDesc,
    ) -> (JoinHandle<Result<RequestOutcome, R3Error>>, Seen) {
        recorder.queue(Script::Hang);
        let before = recorder.seen_count();
        let client = requester.client.clone();
        let transport = requester.transport.clone();
        let identity = requester.identity.clone();
        let origin = requester.origin;
        let desc = *desc;
        let in_flight = tokio::spawn(async move {
            client
                .request(
                    &transport,
                    &identity,
                    &desc,
                    "/slow",
                    Envelope {
                        origin,
                        body: Value::Nil,
                    },
                    RequestOptions {
                        request_timeout: SHORT_REQUEST_TIMEOUT,
                        ..RequestOptions::default()
                    },
                )
                .await
        });
        wait_until("the responder to receive the hanging request", || {
            recorder.seen_count() > before
        })
        .await;
        (in_flight, recorder.last())
    }

    fn timed_out_slow_request() -> R3Error {
        R3Error::Timeout {
            path: "/slow".to_string(),
            after: SHORT_REQUEST_TIMEOUT,
        }
    }

    /// A response frame for `request_id` that encodes to one byte over the cap.
    fn oversize_response(request_id: RequestId, body: Vec<u8>) -> Vec<u8> {
        let bytes = ResponseFrame {
            request_id,
            data: Value::Binary(body),
        }
        .encode();
        assert_eq!(bytes.len(), MAX_R3_PAYLOAD_BYTES + 1);
        bytes
    }

    /// A response body that encodes one byte over the cap and does not compress: the
    /// transport's advertisement check compares the bz2-compressed transfer size, so only a
    /// random body reaches a limit set at that layer.
    fn incompressible_response_body() -> Vec<u8> {
        let mut random = vec![0u8; MAX_R3_PAYLOAD_BYTES + 1 - RESPONSE_FRAME_OVERHEAD];
        rand_core::RngCore::fill_bytes(&mut OsRng, &mut random);
        random
    }

    /// A request frame for `/big` that encodes one byte over the cap, with a random body for
    /// the same reason as `incompressible_response_body`.
    fn oversize_request() -> Vec<u8> {
        let mut random = vec![0u8; MAX_R3_PAYLOAD_BYTES + 1 - REQUEST_FRAME_OVERHEAD];
        rand_core::RngCore::fill_bytes(&mut OsRng, &mut random);
        let packed = RequestFrame::new("/big", Value::Binary(random)).encode();
        assert_eq!(packed.len(), MAX_R3_PAYLOAD_BYTES + 1);
        packed
    }

    /// Waits for the receiver to answer the advertisement of `resource_hash` with a cancel,
    /// which the sender reports as `OutboundCancelled`.
    async fn await_outbound_cancelled(
        events: &mut broadcast::Receiver<ResourceEvent>,
        resource_hash: Hash,
    ) {
        let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv())
                .await
                .expect("the receiver must refuse the oversize advertisement")
                .unwrap();
            if event.hash == resource_hash
                && matches!(event.kind, ResourceEventKind::OutboundCancelled)
            {
                return;
            }
        }
    }

    /// A started `MeshRuntime` (node A) connected to a bare responder (node B) whose
    /// transport also runs an `R3Client`, so either side can request from the other.
    struct NodePair {
        responder: Responder,
        recorder_b: Arc<Recorder>,
        client_b: Arc<R3Client>,
        cancel_b: CancellationToken,
        node_a: Arc<MeshRuntime>,
        recorder_a: Arc<Recorder>,
        a_desc: DestinationDesc,
        _tmp: TempDir,
    }

    impl NodePair {
        /// Node A with a recorder in place of the dispatcher `start` installs.
        async fn start(tag: &str) -> Self {
            let pair = Self::start_as_started(tag).await;
            pair.node_a.set_request_handler(pair.recorder_a.clone());
            pair
        }

        /// Node A exactly as `start` leaves it, serving through its own dispatcher.
        async fn start_as_started(tag: &str) -> Self {
            let recorder_b = Arc::new(Recorder::default());
            // A `MeshRuntime` joins with `TcpClient`'s default MTU, so the responder matches it.
            let responder =
                Responder::listen(recorder_b.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
            let mut b_announces = responder.transport.recv_announces().await;
            let client_b = Arc::new(R3Client::new());
            let cancel_b = CancellationToken::new();
            tokio::spawn(client_b.clone().run(
                responder.transport.out_link_events(),
                responder.transport.resource_events(),
                cancel_b.clone(),
            ));

            let tmp = TempDir::new(tag);
            let mut session = Session::default();
            let node_a = MeshRuntime::start(
                &private_config(responder.port),
                true,
                &mut session,
                mesh_paths(&tmp),
                NodeOptions::default(),
            )
            .await
            .unwrap();
            let recorder_a = Arc::new(Recorder::default());
            let a_hash = node_a.destination_hash().await;
            let a_desc = loop {
                let event = timeout(INTEROP_TIMEOUT, b_announces.recv())
                    .await
                    .expect("node B must hear node A's start announce")
                    .unwrap();
                let desc = event.destination.lock().await.desc;
                if desc.address_hash.to_hex_string() == a_hash {
                    break desc;
                }
            };
            Self {
                responder,
                recorder_b,
                client_b,
                cancel_b,
                node_a,
                recorder_a,
                a_desc,
                _tmp: tmp,
            }
        }

        /// Announces node B so node A has a path to it, waiting until A files B as a peer.
        async fn introduce_b_to_a(&self) {
            let b_app_data = AnnounceAppData {
                version: 1,
                display_name: Some("Bea".to_string()),
            }
            .encode()
            .unwrap();
            self.responder.announce(Some(&b_app_data)).await;
            let peers = self.node_a.peers();
            let b_hash = self.responder.desc.address_hash.to_hex_string();
            wait_until("node A to file node B as a peer", || {
                peers
                    .snapshot()
                    .iter()
                    .any(|peer| peer.destination_hash == b_hash)
            })
            .await;
        }

        /// Arms node A's advertisement-time request cap, which production code leaves off,
        /// and trips it with an oversize request from B. The reject deadlocks A's transport
        /// (upstream rev 3ed5932), which is what the bounded-wait tests need.
        async fn wedge_node_a(&self) {
            self.node_a
                .arm_request_cap_for_test(MAX_R3_PAYLOAD_BYTES)
                .await;
            let transport_b = &self.responder.transport;
            let link = open_link(transport_b, &self.a_desc, "/big", link_deadline())
                .await
                .unwrap();
            let link_id = *link.lock().await.id();
            let mut resource_events = transport_b.resource_events();
            let packed = oversize_request();
            let resource_hash = transport_b
                .send_request_resource(
                    &link_id,
                    RequestId::of_packed(&packed).to_vec(),
                    packed,
                    None,
                )
                .await
                .unwrap();
            await_outbound_cancelled(&mut resource_events, resource_hash).await;
            assert_eq!(
                self.recorder_a.seen_count(),
                0,
                "nothing reached the handler"
            );
        }

        /// Stops node A through a slot, the way the REPL does, and returns how long it took.
        async fn stop_node_a(self) -> Duration {
            let slot = MeshSlot::default();
            slot.install(self.node_a).unwrap();
            let stopping = Instant::now();
            assert!(slot.stop().await.unwrap());
            let stopped_in = stopping.elapsed();
            self.cancel_b.cancel();
            self.responder.stop().await;
            stopped_in
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundary_sizes_pick_packet_or_resource_on_both_halves() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let link = open_link(&requester.transport, &desc, "/echo", link_deadline())
            .await
            .unwrap();
        // `rns_transport::resource::LINK_PACKET_MDU` is the link MDU at the legacy 500-byte
        // MTU both interfaces were given.
        let mdu = link.lock().await.link_mdu();
        assert_eq!(mdu, LINK_PACKET_MDU);
        assert_eq!(mdu, 431);
        drop(link);

        let matrix = [
            (mdu - 1, SizeBranch::Packet),
            (mdu, SizeBranch::Packet),
            (mdu + 1, SizeBranch::Resource),
        ];
        for (target, expected) in matrix {
            let body = request_body_of_encoded_len(requester.origin, target);
            let outcome = requester
                .request(&desc, "/echo", body.clone())
                .await
                .unwrap();
            assert_eq!(
                outcome.request_branch, expected,
                "a {target}-byte request frame"
            );
            assert_eq!(outcome.value, body, "a {target}-byte request frame echoes");
            let seen = recorder.last();
            assert_eq!(seen.branch, expected);
            assert_eq!(seen.path_hash, PathHash::of("/echo"));
            assert_eq!(
                seen.request_id, outcome.request_id,
                "the responder's id for a {expected:?} request must match the requester's"
            );
        }
        for (target, expected) in matrix {
            let body = response_body_of_encoded_len(target);
            recorder.queue(Script::Reply(Reply::Value(body.clone())));
            let outcome = requester.request(&desc, "/echo", Value::Nil).await.unwrap();
            assert_eq!(outcome.request_branch, SizeBranch::Packet);
            assert_eq!(
                outcome.response_branch, expected,
                "a {target}-byte response frame"
            );
            assert_eq!(outcome.value, body, "a {target}-byte response frame");
        }

        let card = card();
        let card_len = RequestFrame::new("/card", card.clone()).encode().len();
        assert!(card_len > mdu, "the card frame is {card_len} bytes");
        let outcome = requester
            .request(&desc, "/card", card.clone())
            .await
            .unwrap();
        assert_eq!(outcome.request_branch, SizeBranch::Resource);
        assert_eq!(outcome.response_branch, SizeBranch::Resource);
        assert_eq!(outcome.value, card);
        let seen = recorder.last();
        assert_eq!(seen.request_id, outcome.request_id);

        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refusal_codes_arrive_as_distinct_errors() {
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Reply(Reply::Code(RefusalCode::NoIdentity)));
        recorder.queue(Script::Reply(Reply::Code(RefusalCode::NoAccess)));
        recorder.queue(Script::Reply(Reply::Value(Value::from(0xf0))));
        let (responder, requester, desc) = pair(recorder.clone()).await;

        let first = requester.request(&desc, "/sync", Value::Nil).await;
        let second = requester.request(&desc, "/sync", Value::Nil).await;
        let third = requester.request(&desc, "/sync", Value::Nil).await;

        assert_eq!(
            first.unwrap_err(),
            R3Error::Refused(RefusalCode::NoIdentity)
        );
        assert_eq!(second.unwrap_err(), R3Error::Refused(RefusalCode::NoAccess));
        assert_eq!(
            third.unwrap_err(),
            R3Error::Refused(RefusalCode::NoIdentity),
            "a bare integer body equal to a code is the code, as LXMRouter reads it"
        );
        assert_eq!(recorder.seen_count(), 3);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unanswered_request_times_out_and_clears_the_pending_table() {
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Hang);
        recorder.queue(Script::Reply(Reply::Silent));
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let options = RequestOptions {
            request_timeout: Duration::from_millis(500),
            ..RequestOptions::default()
        };

        for _ in 0..2 {
            let started = Instant::now();
            let err = requester
                .client
                .request(
                    &requester.transport,
                    &requester.identity,
                    &desc,
                    "/slow",
                    requester.envelope(Value::Nil),
                    options,
                )
                .await
                .unwrap_err();
            assert_eq!(
                err,
                R3Error::Timeout {
                    path: "/slow".to_string(),
                    after: Duration::from_millis(500),
                }
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(requester.client.pending_len(), 0);
        }
        assert_eq!(recorder.seen_count(), 2);

        let outcome = requester
            .request(&desc, "/echo", Value::from("after"))
            .await
            .unwrap();
        assert_eq!(
            outcome.value,
            Value::from("after"),
            "a hung handler must not stall the responder's event loop"
        );
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_the_client_fails_in_flight_requests_with_shutdown() {
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Hang);
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let client = requester.client.clone();
        let transport = requester.transport.clone();
        let identity = requester.identity.clone();
        let origin = requester.origin;
        let in_flight = tokio::spawn(async move {
            client
                .request(
                    &transport,
                    &identity,
                    &desc,
                    "/slow",
                    Envelope {
                        origin,
                        body: Value::Nil,
                    },
                    RequestOptions::default(),
                )
                .await
        });
        wait_until("the request to be pending", || {
            requester.client.pending_len() == 1
        })
        .await;

        requester.cancel.cancel();

        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), R3Error::Shutdown);
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_after_the_client_loop_exits_fails_with_shutdown_at_once() {
        let recorder = Arc::new(Recorder::default());
        let (responder, mut requester, desc) = pair(recorder.clone()).await;
        let link = identified_link(&requester, &responder, &desc).await;
        requester.cancel.cancel();
        timeout(INTEROP_TIMEOUT, &mut requester.client_task)
            .await
            .unwrap()
            .unwrap();

        let result = timeout(
            SHORT_REQUEST_TIMEOUT,
            request_on(&requester, &link, TEST_PATH, Value::Nil, request_deadline()),
        )
        .await
        .expect("a closed client must fail the request without waiting out the deadline");

        assert_eq!(result.unwrap_err(), R3Error::Shutdown);
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_is_tracked_only_after_proof_and_forgotten_on_close() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let transport = &requester.transport;
        let link = open_link(transport, &desc, "/who", link_deadline())
            .await
            .unwrap();

        requester
            .client
            .request_on_link(transport, &link, "/who", Value::Nil, request_deadline())
            .await
            .unwrap();
        let unproven = recorder.last();
        assert_eq!(unproven.identity, None);
        assert_eq!(responder.server.identified_peer_count(), 0);

        identify(
            transport,
            &link,
            &requester.identity,
            "/who",
            link_deadline(),
        )
        .await
        .unwrap();
        wait_until("the responder to record the identity", || {
            responder.server.identified_peer_count() == 1
        })
        .await;
        requester
            .client
            .request_on_link(transport, &link, "/who", Value::Nil, request_deadline())
            .await
            .unwrap();
        let proven = recorder.last();
        assert_eq!(
            proven.identity,
            Some(requester.identity.as_identity().address_hash)
        );
        assert_eq!(responder.server.identified_peer_count(), 1);

        let teardown = link
            .lock()
            .await
            .teardown()
            .expect("an active link builds a teardown");
        transport
            .send_link_packet_on_bound_iface(&link, teardown)
            .await;
        wait_until("the responder to forget the closed link", || {
            responder.server.identified_peer_count() == 0
        })
        .await;
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_request_resource_is_dropped_before_the_handler_runs() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        // The default TCP MTU so the transfer is a couple of segments rather than thousands.
        let responder = Responder::listen(recorder.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let mut requester = Requester::connect(responder.port, TcpClient::DEFAULT_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;
        let transport = &requester.transport;
        let link = open_link(transport, &desc, "/big", link_deadline())
            .await
            .unwrap();
        let link_id = *link.lock().await.id();

        let body = Value::Binary(vec![
            0xab;
            MAX_R3_PAYLOAD_BYTES + 1 - REQUEST_FRAME_OVERHEAD
        ]);
        let packed = RequestFrame::new("/big", body).encode();
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

        let outcome = requester
            .client
            .request_on_link(
                transport,
                &link,
                "/echo",
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_response_resource_is_dropped_after_assembly() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let responder = Responder::listen(recorder.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let mut requester = Requester::connect(responder.port, TcpClient::DEFAULT_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;
        let (in_flight, seen) = hanging_request(&requester, &recorder, &desc).await;

        // With no response-size limit registered the advertisement is accepted whatever its
        // size, and the assembled bytes are dropped by the client instead.
        responder
            .transport
            .send_response_resource(
                &seen.link_id,
                seen.request_id.to_vec(),
                oversize_response(seen.request_id, incompressible_response_body()),
                None,
            )
            .await
            .unwrap();

        let dropped = format!(
            "Dropped an oversize mesh response on link {} ({} bytes, max {MAX_R3_PAYLOAD_BYTES})",
            seen.link_id.to_hex_string(),
            MAX_R3_PAYLOAD_BYTES + 1
        );
        wait_until("the requester to drop the oversize response", || {
            debug_snapshot().contains(&dropped)
        })
        .await;
        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), timed_out_slow_request());
        assert_eq!(requester.client.pending_len(), 0);

        let outcome = requester
            .request(&desc, "/echo", Value::from("after"))
            .await
            .unwrap();
        assert_eq!(
            outcome.value,
            Value::from("after"),
            "an oversize response must not wedge the requester's transport"
        );
        requester.stop().await;
        responder.stop().await;
    }

    /// Refusing an oversize response advertisement leaves the requester's transport with its
    /// handler lock held by a parked upstream task, so every transport call after it blocks.
    /// A request made then must fail on its own deadlines rather than hang. The limit that
    /// triggers the refusal is set by the test only; production code no longer arms that
    /// path, precisely because of this deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_gives_up_on_a_wedged_transport() {
        let recorder = Arc::new(Recorder::default());
        let responder = Responder::listen(recorder.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let mut requester = Requester::connect(responder.port, TcpClient::DEFAULT_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;
        let (in_flight, seen) = hanging_request(&requester, &recorder, &desc).await;
        let out_link = requester
            .transport
            .find_out_link(&seen.link_id)
            .await
            .expect("the requester holds the out-link the request went on");
        out_link
            .lock()
            .await
            .set_response_size_limit(seen.request_id.as_bytes(), MAX_R3_PAYLOAD_BYTES);

        let mut resource_events = responder.transport.resource_events();
        let resource_hash = responder
            .transport
            .send_response_resource(
                &seen.link_id,
                seen.request_id.to_vec(),
                oversize_response(seen.request_id, incompressible_response_body()),
                None,
            )
            .await
            .unwrap();
        await_outbound_cancelled(&mut resource_events, resource_hash).await;
        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), timed_out_slow_request());

        let options = RequestOptions {
            link_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(2),
        };
        let started = Instant::now();
        let result = timeout(
            Duration::from_secs(8),
            requester.client.request(
                &requester.transport,
                &requester.identity,
                &desc,
                "/after",
                requester.envelope(Value::Nil),
                options,
            ),
        )
        .await
        .expect("a request on a wedged transport must give up on its own deadlines");
        assert!(
            matches!(result, Err(R3Error::Timeout { .. })),
            "{result:?} after {:?}",
            started.elapsed()
        );
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_response_is_dropped_before_decode() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let responder = Responder::listen(recorder.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let mut requester = Requester::connect(responder.port, TcpClient::DEFAULT_MTU).await;
        responder.announce(None).await;
        let desc = requester.learn(&responder.desc.address_hash).await;
        let (in_flight, seen) = hanging_request(&requester, &recorder, &desc).await;

        // A repeating body compresses far below the cap, so the advertisement passes the
        // transport's check and the assembled bytes reach the client.
        let body = vec![0xcd; MAX_R3_PAYLOAD_BYTES + 1 - RESPONSE_FRAME_OVERHEAD];
        responder
            .transport
            .send_response_resource(
                &seen.link_id,
                seen.request_id.to_vec(),
                oversize_response(seen.request_id, body),
                None,
            )
            .await
            .unwrap();

        let dropped = format!(
            "Dropped an oversize mesh response on link {} ({} bytes, max {MAX_R3_PAYLOAD_BYTES})",
            seen.link_id.to_hex_string(),
            MAX_R3_PAYLOAD_BYTES + 1
        );
        wait_until("the requester to drop the oversize response", || {
            debug_snapshot().contains(&dropped)
        })
        .await;
        assert_eq!(requester.client.pending_len(), 1);
        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), timed_out_slow_request());
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn response_on_the_wrong_link_is_ignored() {
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
                fresh_destination_name(),
            )
            .await;
        let decoy_hash = decoy.lock().await.desc.address_hash;
        let packet = decoy.lock().await.announce(OsRng, None).unwrap();
        responder.transport.send_packet(packet).await;
        let decoy_desc = requester.learn(&decoy_hash).await;
        let decoy_link = open_link(&requester.transport, &decoy_desc, "/slow", link_deadline())
            .await
            .unwrap();
        let decoy_link_id = *decoy_link.lock().await.id();
        let (in_flight, seen) = hanging_request(&requester, &recorder, &desc).await;
        assert_ne!(seen.link_id, decoy_link_id);

        // Both ends derive the link id from the link request packet, so the responder's
        // in-link for the decoy carries the id the requester saw.
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_runtime_does_not_arm_the_inbound_request_cap() {
        install_log_collector();
        let pair = NodePair::start("r3-runtime-uncapped").await;
        assert_eq!(
            pair.node_a.max_request_size().await,
            None,
            "the destination must not carry a max_request_size: rejecting an advertisement on it deadlocks the upstream transport (rev 3ed5932)"
        );

        let transport_b = &pair.responder.transport;
        let link = open_link(transport_b, &pair.a_desc, "/big", link_deadline())
            .await
            .unwrap();
        let link_id = *link.lock().await.id();
        let packed = oversize_request();
        transport_b
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
        wait_until("node A to drop the oversize request", || {
            debug_snapshot().contains(&dropped)
        })
        .await;
        assert_eq!(
            pair.recorder_a.seen_count(),
            0,
            "nothing reached the handler"
        );

        let outcome = pair
            .client_b
            .request(
                transport_b,
                &TransportIdentity::new_from_rand(OsRng),
                &pair.a_desc,
                "/echo",
                pair.responder.envelope(Value::from("after")),
                RequestOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.value,
            Value::from("after"),
            "an oversize request must not wedge the node's transport"
        );
        assert_eq!(pair.recorder_a.seen_count(), 1);
        pair.stop_node_a().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_runtime_stop_is_bounded_when_the_transport_is_wedged() {
        let pair = NodePair::start("r3-runtime-wedged-stop").await;
        pair.wedge_node_a().await;

        // The interface stop and the deregistration in `shutdown` both wait on the held
        // handler lock, so only the deadlines bring `stop` back.
        let stopped_in = pair.stop_node_a().await;
        assert!(
            stopped_in < SHUTDOWN_GRACE * 2,
            "stop must stay bounded even with the transport wedged; took {stopped_in:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rekey_gives_up_within_grace_when_the_transport_is_wedged() {
        let pair = NodePair::start("r3-runtime-wedged-rekey").await;
        let original_id = pair.node_a.instance_id().await;
        let original_hash = pair.node_a.destination_hash().await;
        pair.wedge_node_a().await;

        let rekeying = Instant::now();
        let err = timeout(
            REKEY_GRACE * 2,
            pair.node_a.rekey(ForkRekey {
                original_instance_id: Some(original_id.clone()),
                fork_instance_id: Session::default().ensure_mesh_instance_id().to_string(),
            }),
        )
        .await
        .expect("rekey must give up on a wedged transport within its grace")
        .unwrap_err()
        .to_string();
        let gave_up_in = rekeying.elapsed();

        assert!(
            err.contains(&original_hash),
            "the error must name the destination the node still serves: {err}"
        );
        assert_eq!(pair.node_a.instance_id().await, original_id);
        assert_eq!(pair.node_a.destination_hash().await, original_hash);
        let stopped_in = pair.stop_node_a().await;
        assert!(
            gave_up_in < REKEY_GRACE * 2 && stopped_in < SHUTDOWN_GRACE * 2,
            "rekey took {gave_up_in:?} and stop took {stopped_in:?} on a wedged transport"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_runtime_requests_and_serves_and_stop_drains_in_flight_requests() {
        let recorder_b = Arc::new(Recorder::default());
        // A `MeshRuntime` joins with `TcpClient`'s default MTU, so the responder matches it.
        let responder = Responder::listen(recorder_b.clone(), TcpServer::DEFAULT_CLIENT_MTU).await;
        let mut b_announces = responder.transport.recv_announces().await;
        let client_b = Arc::new(R3Client::new());
        let cancel_b = CancellationToken::new();
        tokio::spawn(client_b.clone().run(
            responder.transport.out_link_events(),
            responder.transport.resource_events(),
            cancel_b.clone(),
        ));

        let tmp = TempDir::new("r3-runtime");
        let mut session = Session::default();
        let node_a = MeshRuntime::start(
            &private_config(responder.port),
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
        let recorder_a = Arc::new(Recorder::default());
        node_a.set_request_handler(recorder_a.clone());
        let a_hash = node_a.destination_hash().await;
        let a_desc = loop {
            let event = timeout(INTEROP_TIMEOUT, b_announces.recv())
                .await
                .expect("node B must hear node A's start announce")
                .unwrap();
            let desc = event.destination.lock().await.desc;
            if desc.address_hash.to_hex_string() == a_hash {
                break desc;
            }
        };
        let b_app_data = AnnounceAppData {
            version: 1,
            display_name: Some("Bea".to_string()),
        }
        .encode()
        .unwrap();
        responder.announce(Some(&b_app_data)).await;
        let peers = node_a.peers();
        let b_hash = responder.desc.address_hash.to_hex_string();
        wait_until("node A to file node B as a peer", || {
            peers
                .snapshot()
                .iter()
                .any(|peer| peer.destination_hash == b_hash)
        })
        .await;

        let outcome = node_a
            .request(
                &responder.desc,
                "/echo",
                Value::from("from a"),
                RequestOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.value, Value::from("from a"));
        assert!(recorder_b.last().identity.is_some());

        let b_identity = TransportIdentity::new_from_rand(OsRng);
        let outcome = client_b
            .request(
                &responder.transport,
                &b_identity,
                &a_desc,
                "/echo",
                responder.envelope(card()),
                RequestOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.value, card());
        // At the 262144-byte TCP MTU a card fits one link packet in each direction.
        assert_eq!(outcome.request_branch, SizeBranch::Packet);
        assert_eq!(outcome.response_branch, SizeBranch::Packet);
        assert_eq!(
            recorder_a.last().identity,
            Some(b_identity.as_identity().address_hash)
        );

        recorder_a.queue(Script::Hang);
        let client = client_b.clone();
        let transport_b = responder.transport.clone();
        let identity_b = b_identity.clone();
        let origin_b = responder.origin();
        let b_in_flight = tokio::spawn(async move {
            client
                .request(
                    &transport_b,
                    &identity_b,
                    &a_desc,
                    "/slow",
                    Envelope {
                        origin: origin_b,
                        body: Value::Nil,
                    },
                    RequestOptions::default(),
                )
                .await
        });
        wait_until("node A to receive the hanging request", || {
            recorder_a.seen_count() == 2
        })
        .await;

        recorder_b.queue(Script::Hang);
        let node = node_a.clone();
        let desc = responder.desc;
        let in_flight = tokio::spawn(async move {
            node.request(&desc, "/slow", Value::Nil, RequestOptions::default())
                .await
        });
        wait_until("node B to receive the hanging request", || {
            recorder_b.seen_count() == 2
        })
        .await;
        let slot = MeshSlot::default();
        slot.install(node_a).unwrap();

        let stopping = Instant::now();
        assert!(slot.stop().await.unwrap());
        let stopped_in = stopping.elapsed();

        assert!(stopped_in < SHUTDOWN_GRACE, "stop took {stopped_in:?}");
        assert_eq!(
            recorder_a.abandoned_count(),
            1,
            "stop must wait for the hung handler to be dropped, not only cancel it"
        );
        let result = timeout(INTEROP_TIMEOUT, in_flight).await.unwrap().unwrap();
        assert_eq!(result.unwrap_err(), R3Error::Shutdown);
        cancel_b.cancel();
        let result = timeout(INTEROP_TIMEOUT, b_in_flight)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), R3Error::Shutdown);
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_trust_list_admits_nobody_and_never_decodes() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let gate = gate(
            &responder,
            recorder.clone(),
            &TrustList::default(),
            "r3-gate-empty",
        );
        let link = identified_link(&requester, &responder, &desc).await;
        let link_id = *link.lock().await.id();

        let err = request_on(
            &requester,
            &link,
            STATUS_PATH,
            Value::Nil,
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();

        assert_eq!(err, timed_out(STATUS_PATH));
        assert_eq!(recorder.seen_count(), 0, "the handler was never entered");
        assert_eq!(
            responder.server.decoded_count(),
            0,
            "the payload was never decoded"
        );
        assert_eq!(gate.sink.count(), 0);
        assert_debug_logged(&format!(
            "from {} on link {}: dropped: unknown identity",
            &identity_hex(&requester)[..8],
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_path_is_silent_to_strangers_and_a_typed_error_to_peers() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default().identity(&identity_hex(&requester), true);
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-unknown-path");
        let transport = &requester.transport;
        let link = open_link(transport, &desc, "/nope", link_deadline())
            .await
            .unwrap();
        let link_id = *link.lock().await.id();

        let err = request_on(
            &requester,
            &link,
            "/nope",
            Value::Nil,
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();
        assert_eq!(err, timed_out("/nope"));
        assert_eq!(responder.server.decoded_count(), 0);
        assert_debug_logged(&format!(
            "from anonymous on link {}: dropped: unauthenticated",
            link_id.to_hex_string()
        ));

        identify(
            transport,
            &link,
            &requester.identity,
            "/nope",
            link_deadline(),
        )
        .await
        .unwrap();
        wait_until("the responder to record the identity", || {
            responder.server.identified_peer_count() == 1
        })
        .await;
        let outcome = request_on(&requester, &link, "/nope", Value::Nil, request_deadline())
            .await
            .unwrap();

        assert_eq!(
            DispatchError::from_value(&outcome.value),
            Some(DispatchError::UnknownPath {
                path_hash: PathHash::of("/nope").to_hex_string(),
            })
        );
        assert_eq!(responder.server.decoded_count(), 1);
        assert_eq!(recorder.seen_count(), 0);
        assert_eq!(gate.sink.count(), 0);
        assert_debug_logged(&format!(
            "Mesh request {} for hash {} from {} on link {}: unknown path: IdentityTrusted",
            outcome.request_id.to_hex_string(),
            PathHash::of("/nope").to_hex_string(),
            &identity_hex(&requester)[..8],
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_identity_is_dropped_before_decode_without_a_knock() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default().block(&identity_hex(&requester));
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-blocked");
        let link = identified_link(&requester, &responder, &desc).await;
        let link_id = *link.lock().await.id();

        let err = request_on(
            &requester,
            &link,
            TEST_PATH,
            Value::Nil,
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();

        assert_eq!(err, timed_out(TEST_PATH));
        assert_eq!(responder.server.decoded_count(), 0);
        assert_eq!(recorder.seen_count(), 0);
        assert_eq!(gate.sink.count(), 0, "a blocked identity never knocks");
        assert_debug_logged(&format!(
            "on link {}: dropped: blocked identity",
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    /// A `/knock` request from `identity` as `handle` receives it once `admit` has let the
    /// identity through, naming the instance under `origin`.
    fn admitted_knock(identity: Identity, origin: OriginName) -> InboundRequest {
        InboundRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: Some(identity),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(KNOCK_PATH),
            requested_at: 0.0,
            data: Envelope {
                origin,
                body: Value::Nil,
            }
            .into_value(),
            branch: SizeBranch::Packet,
        }
    }

    /// An identity blocked after `admit` let it through still gets nothing: `handle` reads
    /// its standing itself and stays silent rather than refusing.
    #[tokio::test]
    async fn a_blocked_identity_reaching_handle_is_answered_silently() {
        install_log_collector();
        let identity = *TransportIdentity::new_from_rand(OsRng).as_identity();
        let identity_hex = identity.address_hash.to_hex_string();
        let (trust, _tmp) = TrustList::default()
            .block(&identity_hex)
            .open("r3-dispatch-blocked-late");
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());
        let request = admitted_knock(identity, OriginName::of(&fresh_destination_name()));
        let link_id = request.link_id;

        let reply = RequestHandler::handle(&dispatcher, request).await;

        assert!(matches!(reply, Reply::Silent));
        assert_eq!(sink.count(), 0, "a blocked identity never knocks");
        assert_debug_logged(&format!(
            "for /knock from {} on link {}: dropped: blocked identity",
            &identity_hex[..8],
            link_id.to_hex_string()
        ));
    }

    /// Hands `rule` to the dispatcher's refusal seam as `handle` would after the store
    /// refused, recording every `(id8, outcome)` it logs into `logged`.
    fn refusal_under(
        dispatcher: &Dispatcher,
        rule: Rule,
        identity: &Identity,
        logged: &Mutex<Vec<(String, String)>>,
    ) -> Reply {
        let knock = KnockEvent {
            identity_hash: identity.address_hash.to_hex_string(),
            destination_hash: destination_address(
                &OriginName::of(&fresh_destination_name()).0,
                &identity.address_hash,
            )
            .to_hex_string(),
            link_id: LinkId::new_from_rand(OsRng),
            path_hash: PathHash::of(KNOCK_PATH),
            data: Some(Value::Nil),
        };
        dispatcher.refusal(rule, knock, &|id8, outcome| {
            logged.lock().push((id8.to_string(), outcome.to_string()))
        })
    }

    /// The store may block an identity between `handle` reading its standing and asking
    /// for a verdict; that verdict refuses under `IdentityBlocked`. The blocked peer still
    /// hears nothing, exactly as if the standing check had caught it: no refusal bytes and
    /// no knock.
    #[test]
    fn a_verdict_blocked_after_the_standing_check_is_answered_silently() {
        let identity = *TransportIdentity::new_from_rand(OsRng).as_identity();
        let identity_hex = identity.address_hash.to_hex_string();
        let (trust, _tmp) = TrustList::default().open("r3-dispatch-blocked-verdict");
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());
        let logged = Mutex::new(Vec::new());

        let reply = refusal_under(&dispatcher, Rule::IdentityBlocked, &identity, &logged);

        assert!(matches!(reply, Reply::Silent));
        assert_eq!(sink.count(), 0, "a blocked identity never knocks");
        assert_eq!(
            *logged.lock(),
            vec![(
                identity_hex[..8].to_string(),
                "dropped: blocked identity".to_string()
            )]
        );
    }

    /// The other refusals keep their bytes: default-closed knocks and refuses, a denied
    /// destination refuses without a knock.
    #[test]
    fn refusals_other_than_a_blocked_identity_still_answer_no_access() {
        let identity = *TransportIdentity::new_from_rand(OsRng).as_identity();
        let identity_hex = identity.address_hash.to_hex_string();
        let (trust, _tmp) = TrustList::default().open("r3-dispatch-refusal-bytes");
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());
        let logged = Mutex::new(Vec::new());

        let closed = refusal_under(&dispatcher, Rule::DefaultClosed, &identity, &logged);
        assert!(matches!(closed, Reply::Code(RefusalCode::NoAccess)));
        assert_eq!(sink.count(), 1, "default-closed knocks");

        let denied = refusal_under(&dispatcher, Rule::DestinationDenied, &identity, &logged);
        assert!(matches!(denied, Reply::Code(RefusalCode::NoAccess)));
        assert_eq!(sink.count(), 1, "a denied destination does not knock");

        let id8 = identity_hex[..8].to_string();
        assert_eq!(
            *logged.lock(),
            vec![
                (id8.clone(), "refused: DefaultClosed (knocked)".to_string()),
                (id8, "refused: DestinationDenied".to_string()),
            ]
        );
    }

    /// `/knock` is the dispatcher's own. Registering over it is refused, and the built-in
    /// handler goes on serving it.
    #[tokio::test]
    async fn registering_over_knock_is_refused_and_displaces_nothing() {
        let identity = *TransportIdentity::new_from_rand(OsRng).as_identity();
        let identity_hex = identity.address_hash.to_hex_string();
        let (trust, _tmp) = TrustList::default()
            .identity(&identity_hex, true)
            .open("r3-dispatch-knock-reserved");
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());
        let usurper = Arc::new(Recorder::default());

        assert!(matches!(
            dispatcher.register(KNOCK_PATH, usurper.clone()),
            Err(ReservedPath(path)) if path == KNOCK_PATH
        ));

        let request = admitted_knock(identity, OriginName::of(&fresh_destination_name()));
        let reply = RequestHandler::handle(&dispatcher, request).await;

        assert!(matches!(reply, Reply::Value(Value::Nil)));
        assert_eq!(
            sink.count(),
            1,
            "the built-in knock handler still serves /knock"
        );
        assert_eq!(usurper.seen_count(), 0);
    }

    /// An identity untrusted between `admit` and `handle` is no longer in the list at all,
    /// which under the destination rules alone would be default-closed: a knock and a
    /// refusal. `handle` drops it instead, like `admit` would have.
    #[tokio::test]
    async fn an_identity_untrusted_before_handle_is_answered_silently() {
        let identity = *TransportIdentity::new_from_rand(OsRng).as_identity();
        let (trust, _tmp) = TrustList::default().open("r3-dispatch-untrusted-late");
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());
        let request = admitted_knock(identity, OriginName::of(&fresh_destination_name()));

        let reply = RequestHandler::handle(&dispatcher, request).await;

        assert!(matches!(reply, Reply::Silent));
        assert_eq!(sink.count(), 0, "an unknown identity never knocks");
    }

    /// The store ranks a denied destination above a blocked identity, which would answer a
    /// blocked peer asking from a denied instance with refusal bytes. `handle` settles the
    /// identity before the destination is ever judged, so the peer still hears nothing.
    #[tokio::test]
    async fn a_blocked_identity_on_a_denied_destination_is_answered_silently() {
        let identity = *TransportIdentity::new_from_rand(OsRng).as_identity();
        let origin = OriginName::of(&fresh_destination_name());
        let destination = destination_address(&origin.0, &identity.address_hash);
        let (trust, _tmp) = TrustList::default()
            .block(&identity.address_hash.to_hex_string())
            .deny(&destination.to_hex_string())
            .open("r3-dispatch-blocked-denied");
        let sink = Arc::new(SpySink::default());
        let dispatcher = Dispatcher::new(trust, sink.clone());

        let reply = RequestHandler::handle(&dispatcher, admitted_knock(identity, origin)).await;

        assert!(
            matches!(reply, Reply::Silent),
            "not a refusal a peer can read"
        );
        assert_eq!(sink.count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn untrusted_destination_knocks_and_then_refuses() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default().identity(&identity_hex(&requester), false);
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-knock");
        let link = identified_link(&requester, &responder, &desc).await;
        let link_id = *link.lock().await.id();

        let err = request_on(
            &requester,
            &link,
            TEST_PATH,
            Value::from("intro"),
            request_deadline(),
        )
        .await
        .unwrap_err();

        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
        let (identity, destination, path_hash, data) = gate.sink.only();
        assert_eq!(identity, identity_hex(&requester));
        assert_eq!(
            destination,
            requester_destination_hex(&requester),
            "the knock names the requester's own instance"
        );
        assert_eq!(path_hash, PathHash::of(TEST_PATH));
        assert_eq!(data, None, "only /knock carries the body to the sink");
        assert_eq!(recorder.seen_count(), 0);
        assert_debug_logged(&format!(
            "from {} on link {}: refused: DefaultClosed (knocked)",
            &identity_hex(&requester)[..8],
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn denied_destination_refuses_without_a_knock() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default()
            .identity(&identity_hex(&requester), true)
            .deny(&requester_destination_hex(&requester));
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-denied");
        let link = identified_link(&requester, &responder, &desc).await;
        let link_id = *link.lock().await.id();

        let err = request_on(&requester, &link, TEST_PATH, Value::Nil, request_deadline())
            .await
            .unwrap_err();

        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
        assert_eq!(gate.sink.count(), 0, "a deny is final, not a knock");
        assert_eq!(recorder.seen_count(), 0);
        assert_debug_logged(&format!(
            "from {} on link {}: refused: DestinationDenied",
            &identity_hex(&requester)[..8],
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    /// Default-closed, a denied instance and a body that names no instance are all refused
    /// with the same bytes, so a refusal never tells the peer which of the three it hit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_refusal_is_the_same_bytes_on_the_wire() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let identity = identity_hex(&requester);
        let destination = requester_destination_hex(&requester);
        let states = [
            (TrustList::default().identity(&identity, false), false),
            (
                TrustList::default()
                    .identity(&identity, true)
                    .deny(&destination),
                false,
            ),
            (TrustList::default().identity(&identity, true), true),
        ];

        let mut tails = Vec::new();
        for (n, (list, raw_body)) in states.iter().enumerate() {
            let _gate = gate(&responder, recorder.clone(), list, &format!("r3-bytes-{n}"));
            let mut events = requester.transport.out_link_events();
            let result = if *raw_body {
                let link = identified_link(&requester, &responder, &desc).await;
                requester
                    .client
                    .request_on_link(
                        &requester.transport,
                        &link,
                        TEST_PATH,
                        Value::from(n),
                        request_deadline(),
                    )
                    .await
            } else {
                requester.request(&desc, TEST_PATH, Value::from(n)).await
            };
            let err = result.unwrap_err();
            assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
            let payloads = response_payloads(&mut events);
            assert_eq!(payloads.len(), 1, "one response per request");
            let payload = &payloads[0];
            assert_eq!(&payload[..3], &[0x92, 0xc4, 0x10], "array(2), bin8(16)");
            tails.push((payload.len(), payload[19..].to_vec()));
        }

        assert_eq!(tails.len(), 3);
        assert!(
            tails.iter().all(|tail| tail == &tails[0]),
            "every refusal must be the same bytes: {tails:?}"
        );
        assert_eq!(tails[0].1, vec![0xcc, 0xf1]);
        assert_eq!(recorder.seen_count(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[test]
    fn destination_address_matches_upstream_derivation() {
        let identity = TransportIdentity::new_from_rand(OsRng);
        let name = fresh_destination_name();
        let upstream = SingleInputDestination::new(identity.clone(), name)
            .desc
            .address_hash;
        let identity_hash = identity.as_identity().address_hash;

        assert_eq!(
            destination_address(
                name.as_name_hash_slice().try_into().unwrap(),
                &identity_hash
            ),
            upstream
        );
        assert_eq!(
            destination_address(&OriginName::of(&name).0, &identity_hash),
            upstream,
            "the origin carries exactly the bytes the derivation reads"
        );
    }

    /// A body that names no instance leaves nothing for the destination tier to judge, so
    /// a known identity sending one is refused outright, even one trusted everywhere: no
    /// knock, since there is no instance to knock for, and no handler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_envelope_from_a_known_identity_is_refused_without_a_knock() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default().identity(&identity_hex(&requester), true);
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-malformed");
        let link = identified_link(&requester, &responder, &desc).await;
        let link_id = *link.lock().await.id();

        let bodies = [
            Value::Nil,
            Value::Map(vec![(
                Value::from("name_hash"),
                Value::Binary(requester.origin.0.to_vec()),
            )]),
        ];
        for body in bodies {
            let err = requester
                .client
                .request_on_link(
                    &requester.transport,
                    &link,
                    TEST_PATH,
                    body.clone(),
                    request_deadline(),
                )
                .await
                .unwrap_err();
            assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess), "{body:?}");
        }

        assert_eq!(recorder.seen_count(), 0, "the handler was never entered");
        assert_eq!(
            gate.sink.count(),
            0,
            "nothing to knock for without an instance"
        );
        assert_debug_logged(&format!(
            "from {} on link {}: refused: unverifiable origin",
            &identity_hex(&requester)[..8],
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    /// The instance a request names is bound to the identity proven on the link. A peer
    /// that names the origin of an instance trusted under another identity is judged as
    /// its own instance of that name, which the list does not know, so the trust the user
    /// gave identity B cannot be borrowed by identity A.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_claimed_instance_is_bound_to_the_proven_identity() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let identity_b = TransportIdentity::new_from_rand(OsRng);
        let identity_b = identity_b.as_identity().address_hash;
        let borrowed = destination_address(&requester.origin.0, &identity_b);
        let list = TrustList::default()
            .identity(&identity_hex(&requester), false)
            .destination(&borrowed.to_hex_string(), &identity_b.to_hex_string());
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-bound");

        let err = requester
            .request(&desc, TEST_PATH, Value::from("mine?"))
            .await
            .unwrap_err();

        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
        let (identity, destination, path_hash, data) = gate.sink.only();
        assert_eq!(identity, identity_hex(&requester));
        assert_eq!(
            destination,
            requester_destination_hex(&requester),
            "the knock names the instance under the requester's own identity"
        );
        assert_ne!(destination, borrowed.to_hex_string());
        assert_eq!(path_hash, PathHash::of(TEST_PATH));
        assert_eq!(data, None);
        assert_eq!(recorder.seen_count(), 0, "the handler was never entered");
        let link_id = gate.sink.knocks.lock()[0].link_id;
        assert_debug_logged(&format!(
            "from {} on link {}: refused: DefaultClosed (knocked)",
            &identity_hex(&requester)[..8],
            link_id.to_hex_string()
        ));
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trusted_destination_is_served_and_placeholders_answer_typed_errors() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default().destination(
            &requester_destination_hex(&requester),
            &identity_hex(&requester),
        );
        let gate = gate(&responder, recorder.clone(), &list, "r3-gate-served");

        let outcome = requester
            .request(&desc, TEST_PATH, Value::from("ping"))
            .await
            .unwrap();
        assert_eq!(outcome.value, Value::from("ping"));
        let seen = recorder.last();
        assert_eq!(
            seen.identity,
            Some(requester.identity.as_identity().address_hash)
        );
        assert_eq!(
            seen.destination,
            Some(requester_destination(&requester)),
            "the handler sees the requester's own instance"
        );
        assert_eq!(seen.path_hash, PathHash::of(TEST_PATH));
        assert_debug_logged(&format!(
            "Mesh request {} for hash {} from {} on link {}: served: DestinationTrusted",
            outcome.request_id.to_hex_string(),
            PathHash::of(TEST_PATH).to_hex_string(),
            &identity_hex(&requester)[..8],
            seen.link_id.to_hex_string()
        ));

        let outcome = requester
            .request(&desc, STATUS_PATH, Value::Nil)
            .await
            .unwrap();
        assert_eq!(
            DispatchError::from_value(&outcome.value),
            Some(DispatchError::NoProvider {
                path: STATUS_PATH.to_string(),
            })
        );
        assert_debug_logged(&format!(
            "Mesh request {} for /status from {} on link {}: no provider: DestinationTrusted",
            outcome.request_id.to_hex_string(),
            &identity_hex(&requester)[..8],
            seen.link_id.to_hex_string()
        ));

        let outcome = requester
            .request(&desc, MESSAGE_PATH, Value::Nil)
            .await
            .unwrap();
        assert_eq!(
            DispatchError::from_value(&outcome.value),
            Some(DispatchError::NoProvider {
                path: MESSAGE_PATH.to_string(),
            })
        );

        let outcome = requester
            .request(&desc, KNOCK_PATH, Value::from("hello"))
            .await
            .unwrap();
        assert_eq!(outcome.value, Value::Nil);
        let (identity, destination, path_hash, data) = gate.sink.only();
        assert_eq!(identity, identity_hex(&requester));
        assert_eq!(destination, requester_destination_hex(&requester));
        assert_eq!(path_hash, PathHash::of(KNOCK_PATH));
        assert_eq!(data, Some(Value::from("hello")));
        assert_eq!(recorder.seen_count(), 1);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_is_read_from_the_link_when_the_table_has_lost_it() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let link = identified_link(&requester, &responder, &desc).await;

        responder.server.forget_identities_for_test();
        assert_eq!(responder.server.identified_peer_count(), 0);
        request_on(&requester, &link, "/who", Value::Nil, request_deadline())
            .await
            .unwrap();

        let seen = recorder.last();
        assert_eq!(
            seen.identity,
            Some(requester.identity.as_identity().address_hash),
            "the link itself knows who proved themselves on it"
        );
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_beyond_the_handler_slots_are_dropped_silently() {
        install_log_collector();
        let stall = Arc::new(Stall::default());
        let (responder, requester, desc) = pair(stall.clone()).await;
        let link = open_link(&requester.transport, &desc, "/slow", link_deadline())
            .await
            .unwrap();
        let link_id = *link.lock().await.id();

        let in_flight: Vec<_> = (0..MAX_CONCURRENT_INBOUND_REQUESTS)
            .map(|_| {
                let client = requester.client.clone();
                let transport = requester.transport.clone();
                let link = link.clone();
                tokio::spawn(async move {
                    client
                        .request_on_link(&transport, &link, "/slow", Value::Nil, request_deadline())
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
            "/slow",
            Value::Nil,
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();

        assert_eq!(err, timed_out("/slow"));
        assert_eq!(
            stall.entered.load(Ordering::SeqCst),
            MAX_CONCURRENT_INBOUND_REQUESTS
        );
        assert_debug_logged(&format!(
            "on link {}: all {MAX_CONCURRENT_INBOUND_REQUESTS} handler slots are busy",
            link_id.to_hex_string()
        ));
        for task in in_flight {
            task.abort();
        }
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_handler_past_its_timeout_answers_nothing_and_frees_its_slot() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Hang);
        let server = Arc::new(R3Server::with_handler_timeout_for_test(
            Duration::from_millis(300),
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
                "/slow",
                requester.envelope(Value::Nil),
                short_options(),
            )
            .await
            .unwrap_err();

        assert_eq!(err, timed_out("/slow"));
        let seen = recorder.last();
        let warned = format!(
            "Mesh request {} on link {} was not handled within 300ms; sent nothing",
            seen.request_id.to_hex_string(),
            seen.link_id.to_hex_string()
        );
        assert!(
            warn_snapshot().iter().any(|message| message == &warned),
            "expected {warned:?}"
        );
        wait_until("the hung handler to be dropped", || {
            recorder.abandoned_count() == 1
        })
        .await;
        wait_until("the handler slot to be freed", || {
            responder.server.available_permits() == MAX_CONCURRENT_INBOUND_REQUESTS
        })
        .await;
        let outcome = requester
            .request(&desc, "/echo", Value::from("after"))
            .await
            .unwrap();
        assert_eq!(outcome.value, Value::from("after"));
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_request_future_clears_its_pending_entry() {
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Hang);
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let link = open_link(&requester.transport, &desc, "/slow", link_deadline())
            .await
            .unwrap();

        let abandoned = timeout(
            Duration::from_secs(1),
            request_on(&requester, &link, "/slow", Value::Nil, request_deadline()),
        )
        .await;

        assert!(
            abandoned.is_err(),
            "the request future was dropped mid-wait"
        );
        wait_until("the responder to have received the request", || {
            recorder.seen_count() == 1
        })
        .await;
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sequential_requests_reuse_the_link_and_stay_one_identified_peer() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;

        requester.request(&desc, "/echo", Value::Nil).await.unwrap();
        let first = recorder.last();
        requester.request(&desc, "/echo", Value::Nil).await.unwrap();
        let second = recorder.last();

        assert_eq!(first.link_id, second.link_id);
        assert_eq!(responder.server.identified_peer_count(), 1);
        assert_eq!(recorder.seen_count(), 2);
        requester.stop().await;
        responder.stop().await;
    }

    /// Every state the receipt goes through after `Sent`, in order, up to the terminal one.
    async fn receipt_states(mut receipt: RequestReceipt) -> Vec<ReceiptState> {
        let mut states = Vec::new();
        loop {
            let Some(state) = timeout(INTEROP_TIMEOUT, receipt.changed())
                .await
                .expect("the receipt must settle")
            else {
                break;
            };
            let terminal = matches!(state, ReceiptState::Ready(_) | ReceiptState::Failed(_));
            states.push(state);
            if terminal {
                break;
            }
        }
        states
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn packet_receipt_goes_straight_from_sent_to_ready() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;

        let receipt = requester.client.request_with_receipt(
            requester.transport.clone(),
            requester.identity.clone(),
            desc,
            "/echo".to_string(),
            requester.envelope(Value::from("r")),
            RequestOptions::default(),
            requester.cancel.clone(),
        );

        assert_eq!(
            receipt_states(receipt).await,
            vec![ReceiptState::Ready(Value::from("r"))]
        );
        assert_eq!(recorder.last().branch, SizeBranch::Packet);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resource_receipt_reports_delivery_before_ready() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let card = card();

        let receipt = requester.client.request_with_receipt(
            requester.transport.clone(),
            requester.identity.clone(),
            desc,
            "/card".to_string(),
            requester.envelope(card.clone()),
            RequestOptions::default(),
            requester.cancel.clone(),
        );

        assert_eq!(
            receipt_states(receipt).await,
            vec![ReceiptState::Delivered, ReceiptState::Ready(card)]
        );
        assert_eq!(recorder.last().branch, SizeBranch::Resource);
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receipt_fails_with_the_timeout_when_nothing_answers() {
        let recorder = Arc::new(Recorder::default());
        recorder.queue(Script::Hang);
        let (responder, requester, desc) = pair(recorder.clone()).await;

        let receipt = requester.client.request_with_receipt(
            requester.transport.clone(),
            requester.identity.clone(),
            desc,
            "/slow".to_string(),
            requester.envelope(Value::Nil),
            short_options(),
            requester.cancel.clone(),
        );

        assert_eq!(receipt.wait().await, Err(timed_out("/slow")));
        assert_eq!(requester.client.pending_len(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receipt_fails_with_the_refusal_the_dispatcher_sends() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let list = TrustList::default().identity(&identity_hex(&requester), false);
        let gate = gate(&responder, recorder.clone(), &list, "r3-receipt-refused");

        let receipt = requester.client.request_with_receipt(
            requester.transport.clone(),
            requester.identity.clone(),
            desc,
            TEST_PATH.to_string(),
            requester.envelope(Value::Nil),
            RequestOptions::default(),
            requester.cancel.clone(),
        );

        assert_eq!(
            receipt_states(receipt).await,
            vec![ReceiptState::Failed(R3Error::Refused(
                RefusalCode::NoAccess
            ))]
        );
        assert_eq!(gate.sink.count(), 1);
        requester.stop().await;
        responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_runtime_serves_through_its_dispatcher_from_the_start() {
        install_log_collector();
        let pair = NodePair::start_as_started("r3-runtime-dispatcher").await;
        let stranger = TransportIdentity::new_from_rand(OsRng);

        let err = pair
            .client_b
            .request(
                &pair.responder.transport,
                &stranger,
                &pair.a_desc,
                STATUS_PATH,
                pair.responder.envelope(Value::Nil),
                short_options(),
            )
            .await
            .unwrap_err();

        assert_eq!(err, timed_out(STATUS_PATH));
        assert_eq!(pair.recorder_a.seen_count(), 0);
        assert!(pair.node_a.trust().records().is_empty());
        let from = format!(
            "from {} on link",
            &stranger.as_identity().address_hash.to_hex_string()[..8]
        );
        assert!(
            debug_snapshot()
                .iter()
                .any(|m| m.contains(&from) && m.ends_with("dropped: unknown identity")),
            "node A must drop the stranger before decoding"
        );
        pair.stop_node_a().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_runtime_dispatcher_takes_a_provider_after_start() {
        let pair = NodePair::start_as_started("r3-runtime-register").await;
        let peer = TransportIdentity::new_from_rand(OsRng);
        let slot = MeshSlot::default();
        slot.install(pair.node_a.clone()).unwrap();
        pair.node_a
            .trust()
            .trust_identity(
                &slot,
                &peer.as_identity().address_hash.to_hex_string(),
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let ask = |path: &'static str| {
            pair.client_b.request(
                &pair.responder.transport,
                &peer,
                &pair.a_desc,
                path,
                pair.responder.envelope(Value::from("ping")),
                short_options(),
            )
        };

        for (served, path) in [STATUS_PATH, MESSAGE_PATH].into_iter().enumerate() {
            let outcome = ask(path).await.unwrap();
            assert_eq!(
                DispatchError::from_value(&outcome.value),
                Some(DispatchError::NoProvider {
                    path: path.to_string(),
                }),
                "{path} is a placeholder until a provider registers"
            );
            assert_eq!(pair.recorder_a.seen_count(), served);

            assert!(
                pair.node_a
                    .dispatcher()
                    .register(path, pair.recorder_a.clone())
                    .unwrap()
                    .is_none(),
                "registering over the {path} placeholder displaces nothing"
            );

            let outcome = ask(path).await.unwrap();
            assert_eq!(outcome.value, Value::from("ping"));
            assert_eq!(pair.recorder_a.seen_count(), served + 1);
            let seen = pair.recorder_a.last();
            assert_eq!(seen.path_hash, PathHash::of(path));
            assert_eq!(
                seen.destination,
                Some(destination_address(
                    &pair.responder.origin().0,
                    &peer.as_identity().address_hash
                )),
                "the provider sees the requester's instance under the identity it proved"
            );
        }
        pair.stop_node_a().await;
    }

    /// The origin a request names is read from the state `rekey` replaces, so after a rekey
    /// the far side derives the fork's destination from it and never the original's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_after_rekey_claims_the_fork_instance() {
        let pair = NodePair::start("r3-runtime-rekey-origin").await;
        pair.introduce_b_to_a().await;
        let list =
            TrustList::default().identity(&pair.a_desc.identity.address_hash.to_hex_string(), true);
        let _gate = gate(
            &pair.responder,
            pair.recorder_b.clone(),
            &list,
            "r3-runtime-rekey-origin-trust",
        );
        let original_hash = pair.node_a.destination_hash().await;
        pair.node_a
            .rekey(ForkRekey {
                original_instance_id: Some(pair.node_a.instance_id().await),
                fork_instance_id: Session::default().ensure_mesh_instance_id().to_string(),
            })
            .await
            .unwrap();
        let fork_hash = pair.node_a.destination_hash().await;
        assert_ne!(fork_hash, original_hash);

        pair.node_a
            .request(
                &pair.responder.desc,
                TEST_PATH,
                Value::from("from the fork"),
                RequestOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(
            pair.recorder_b
                .last()
                .destination
                .map(|hash| hash.to_hex_string()),
            Some(fork_hash),
            "the far side must see the fork's instance, not the one the node started as"
        );
        pair.stop_node_a().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_the_node_fails_an_in_flight_receipt_with_shutdown() {
        let pair = NodePair::start("r3-runtime-receipt-shutdown").await;
        pair.introduce_b_to_a().await;
        pair.recorder_b.queue(Script::Hang);
        let receipt = pair
            .node_a
            .request_with_receipt(
                &pair.responder.desc,
                "/slow",
                Value::Nil,
                RequestOptions::default(),
            )
            .await
            .unwrap();
        wait_until("node B to receive the hanging request", || {
            pair.recorder_b.seen_count() == 1
        })
        .await;
        let slot = MeshSlot::default();
        slot.install(pair.node_a.clone()).unwrap();

        assert!(slot.stop().await.unwrap());

        let states = timeout(SHUTDOWN_GRACE, receipt_states(receipt))
            .await
            .expect("the receipt must settle within the shutdown grace");
        assert_eq!(states, vec![ReceiptState::Failed(R3Error::Shutdown)]);
        pair.cancel_b.cancel();
        pair.responder.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_runtime_refuses_to_start_on_a_corrupt_trust_list() {
        let (addr, relay, _) = loopback_relay().await;
        let tmp = TempDir::new("r3-runtime-corrupt-trust");
        let paths = mesh_paths(&tmp);
        let identity_path = paths.identity_path.clone();
        let trust_path = mesh_config_dir(&paths.config_dir).join("trust.yaml");
        fs::create_dir_all(trust_path.parent().unwrap()).unwrap();
        fs::write(&trust_path, "version: 1\nidentities: [not, a, map]\n").unwrap();

        let Err(err) = MeshRuntime::start(
            &private_config(addr.port()),
            true,
            &mut Session::default(),
            paths,
            NodeOptions::default(),
        )
        .await
        else {
            panic!("a corrupt trust list must refuse the start");
        };

        let text = format!("{err:#}");
        assert!(
            text.contains(&trust_path.display().to_string()),
            "the error must name the trust file: {text}"
        );
        assert!(
            !identity_path.exists(),
            "a refused start must not mint an identity"
        );
        relay.abort();
    }

    /// A peer the list knows but does not admit must learn nothing about which paths exist:
    /// the refusal for a served path and for a path nobody registered is the same bytes,
    /// under the default-closed rule (which knocks for each) and under a deny (which
    /// does not), and neither ever decodes the payload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unadmitted_peer_cannot_tell_a_served_path_from_an_unknown_one() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let identity = identity_hex(&requester);
        let destination = requester_destination_hex(&requester);
        let lists = [
            TrustList::default().identity(&identity, false),
            TrustList::default()
                .identity(&identity, true)
                .deny(&destination),
        ];
        let knocks_expected = [2, 0];
        let paths = [TEST_PATH, "/nope"];

        let mut tails = Vec::new();
        for (n, list) in lists.iter().enumerate() {
            let gate = gate(
                &responder,
                recorder.clone(),
                list,
                &format!("r3-path-leak-{n}"),
            );
            for path in paths {
                let mut events = requester.transport.out_link_events();
                let err = requester
                    .request(&desc, path, Value::from(path))
                    .await
                    .unwrap_err();
                assert_eq!(
                    err,
                    R3Error::Refused(RefusalCode::NoAccess),
                    "{path} under list {n}"
                );
                let payloads = response_payloads(&mut events);
                assert_eq!(payloads.len(), 1, "one response to {path} under list {n}");
                tails.push(payloads[0][19..].to_vec());
            }
            assert_eq!(
                gate.sink.count(),
                knocks_expected[n],
                "knocks under list {n}"
            );
            if knocks_expected[n] > 0 {
                let knocked: Vec<PathHash> = gate
                    .sink
                    .knocks
                    .lock()
                    .iter()
                    .map(|k| k.path_hash)
                    .collect();
                assert_eq!(
                    knocked,
                    paths.iter().map(|p| PathHash::of(p)).collect::<Vec<_>>(),
                    "a default-closed knock names the path that was asked for"
                );
            }
        }

        assert_eq!(tails.len(), 4);
        assert!(
            tails.iter().all(|tail| tail == &tails[0]),
            "every refusal must be the same bytes: {tails:?}"
        );
        assert_eq!(recorder.seen_count(), 0, "no handler was entered");
        // The identity tier runs before any decoding (see the stranger tests). The
        // destination tier cannot: a knock has to name the path that was asked for and the
        // refusal log line names it too, and the path hash is inside the frame. So each of
        // these four refusals decodes exactly one frame from an identity the user listed,
        // and never more than that.
        assert_eq!(
            responder.server.decoded_count(),
            4,
            "one decode per known-identity refusal, none beyond"
        );
        assert!(
            debug_snapshot()
                .iter()
                .any(|m| m.contains("refused: DefaultClosed (knocked)"))
                && debug_snapshot()
                    .iter()
                    .any(|m| m.contains("refused: DestinationDenied")),
            "each refusal names the rule that fired"
        );
        requester.stop().await;
        responder.stop().await;
    }

    /// The knock path is not a way in for identities the list has never seen, and a link
    /// that never proved an identity is not served even on a registered path: both get no
    /// bytes back, no knock, no decode, and never reach a handler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stranger_cannot_knock_and_an_anonymous_link_is_not_served() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let gate = gate(
            &responder,
            recorder.clone(),
            &TrustList::default(),
            "r3-stranger-knock",
        );

        let proven = identified_link(&requester, &responder, &desc).await;
        let mut events = requester.transport.out_link_events();
        let err = request_on(
            &requester,
            &proven,
            KNOCK_PATH,
            Value::from("let me in"),
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();
        assert_eq!(err, timed_out(KNOCK_PATH));
        assert!(
            response_payloads(&mut events).is_empty(),
            "a stranger's knock gets no bytes back"
        );
        assert_eq!(
            gate.sink.count(),
            0,
            "a stranger's knock never reaches the sink"
        );

        let anonymous = open_link(&requester.transport, &desc, TEST_PATH, link_deadline())
            .await
            .unwrap();
        let mut events = requester.transport.out_link_events();
        let err = request_on(
            &requester,
            &anonymous,
            TEST_PATH,
            Value::from("hello"),
            Deadline::after(SHORT_REQUEST_TIMEOUT),
        )
        .await
        .unwrap_err();
        assert_eq!(err, timed_out(TEST_PATH));
        assert!(
            response_payloads(&mut events).is_empty(),
            "an anonymous request gets no bytes back"
        );

        assert_eq!(recorder.seen_count(), 0, "no handler was entered");
        assert_eq!(responder.server.decoded_count(), 0, "nothing was decoded");
        assert_eq!(gate.sink.count(), 0);
        requester.stop().await;
        responder.stop().await;
    }

    /// Trust is consulted on every request, not once per link: a peer served a moment ago
    /// is dropped on the very same link after the user blocks it. Along the way, a second
    /// `register` hands back the provider it replaces and the replacement is the one that
    /// serves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_a_served_peer_drops_its_next_request_on_the_same_link() {
        install_log_collector();
        let pair = NodePair::start_as_started("r3-runtime-block-live").await;
        let peer = TransportIdentity::new_from_rand(OsRng);
        let peer_hex = peer.as_identity().address_hash.to_hex_string();
        let slot = MeshSlot::default();
        slot.install(pair.node_a.clone()).unwrap();
        pair.node_a
            .trust()
            .trust_identity(&slot, &peer_hex, TrustOptions::default(), SystemTime::now())
            .unwrap();
        let dispatcher = pair.node_a.dispatcher();
        let replaced = Arc::new(Recorder::default());
        assert!(
            dispatcher
                .register(STATUS_PATH, replaced.clone())
                .unwrap()
                .is_none()
        );
        assert!(
            dispatcher
                .register(STATUS_PATH, pair.recorder_a.clone())
                .unwrap()
                .is_some(),
            "registering a path again returns the provider it replaces"
        );

        let outcome = pair
            .client_b
            .request(
                &pair.responder.transport,
                &peer,
                &pair.a_desc,
                STATUS_PATH,
                pair.responder.envelope(Value::from("before")),
                short_options(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.value, Value::from("before"));
        assert_eq!(pair.recorder_a.seen_count(), 1);
        assert_eq!(
            replaced.seen_count(),
            0,
            "the replaced provider is never entered"
        );
        let served_on = pair.recorder_a.last().link_id;

        let removed = pair
            .node_a
            .trust()
            .block_identity(&slot, &peer_hex, None, SystemTime::now())
            .unwrap();
        assert!(
            removed.is_empty(),
            "identity trust alone has no destinations to remove"
        );

        let err = pair
            .client_b
            .request(
                &pair.responder.transport,
                &peer,
                &pair.a_desc,
                STATUS_PATH,
                pair.responder.envelope(Value::from("after")),
                short_options(),
            )
            .await
            .unwrap_err();
        assert_eq!(err, timed_out(STATUS_PATH));
        assert_eq!(
            pair.recorder_a.seen_count(),
            1,
            "nothing reaches the provider once the peer is blocked"
        );
        let from = format!(
            "from {} on link {}",
            &peer_hex[..8],
            served_on.to_hex_string()
        );
        assert!(
            debug_snapshot()
                .iter()
                .any(|m| m.contains(&from) && m.ends_with("dropped: blocked identity")),
            "the drop is logged against the same link that was served: {from}"
        );
        pair.stop_node_a().await;
    }

    /// The link a refusal for `prefix` was logged against, read back from the debug log the
    /// way an operator would, so a later request can be matched to the same link.
    fn link_in_refusal_log(prefix: &str, outcome: &str) -> String {
        let from = format!("from {prefix} on link ");
        let line = debug_snapshot()
            .into_iter()
            .find(|m| m.contains(&from) && m.ends_with(outcome))
            .unwrap_or_else(|| panic!("no debug line for {prefix} ends with {outcome:?}"));
        let after = &line[line.find(&from).unwrap() + from.len()..];
        after[..after
            .find(':')
            .expect("the link hash is followed by a colon")]
            .to_string()
    }

    /// The flow a knock exists for. A peer whose identity the list knows asks from an
    /// instance the list does not carry, is knocked for and refused; the user trusts that
    /// instance; the peer's retry over the very same link is served. Nothing reconnects in
    /// between, so the grant, like the block, is read per request rather than per link.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trusting_a_knocking_instance_serves_its_retry_on_the_same_link() {
        install_log_collector();
        let pair = NodePair::start_as_started("r3-runtime-knock-then-trust").await;
        pair.introduce_b_to_a().await;
        let b_identity = pair.responder.dest.lock().await.identity.clone();
        let b_identity_hex = b_identity.as_identity().address_hash.to_hex_string();
        let b_instance_hex = pair.responder.desc.address_hash.to_hex_string();
        let slot = MeshSlot::default();
        slot.install(pair.node_a.clone()).unwrap();
        let trust = pair.node_a.trust();
        // Known identity with no listed instance: where a peer stands after the user trusted
        // one of its instances and later withdrew it.
        trust
            .trust_destination(
                &slot,
                &b_instance_hex,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        trust.untrust_destination(&slot, &b_instance_hex).unwrap();
        assert_eq!(
            trust.identity_standing(&b_identity_hex),
            IdentityStanding::Trusted {
                all_destinations: false
            }
        );
        assert!(
            pair.node_a
                .dispatcher()
                .register(STATUS_PATH, pair.recorder_a.clone())
                .unwrap()
                .is_none()
        );
        let status = |body: &str| {
            pair.client_b.request(
                &pair.responder.transport,
                &b_identity,
                &pair.a_desc,
                STATUS_PATH,
                pair.responder.envelope(Value::from(body)),
                short_options(),
            )
        };

        let err = status("may I?").await.unwrap_err();
        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
        assert_eq!(
            pair.recorder_a.seen_count(),
            0,
            "a knock never enters the handler"
        );
        let knocked_on =
            link_in_refusal_log(&b_identity_hex[..8], "refused: DefaultClosed (knocked)");
        assert_debug_logged(&format!(
            "Mesh knock from {} for destination {} on link {} via",
            &b_identity_hex[..8],
            &b_instance_hex[..8],
            knocked_on
        ));

        let granted = trust
            .trust_destination(
                &slot,
                &b_instance_hex,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(granted.change, TrustChange::Added);
        assert_eq!(granted.identity_hash, b_identity_hex);

        let outcome = status("again").await.unwrap();
        assert_eq!(outcome.value, Value::from("again"));
        assert_eq!(pair.recorder_a.seen_count(), 1);
        let seen = pair.recorder_a.last();
        assert_eq!(
            seen.link_id.to_hex_string(),
            knocked_on,
            "the retry is served on the link that knocked"
        );
        assert_eq!(seen.destination, Some(pair.responder.desc.address_hash));
        assert_eq!(seen.identity, Some(b_identity.as_identity().address_hash));
        assert_debug_logged(&format!(
            "from {} on link {}: served: DestinationTrusted",
            &b_identity_hex[..8],
            knocked_on
        ));
        pair.stop_node_a().await;
    }

    /// Every line the serving path logs names a peer by a truncated hash only. After one
    /// peer has been knocked for through the sink the runtime installs, refused as
    /// unverifiable, told a path is unknown and served, neither its full identity hash nor
    /// its full instance hash appears anywhere in the debug or warn log.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serving_path_logs_never_carry_a_full_identity_or_instance_hash() {
        install_log_collector();
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let identity = identity_hex(&requester);
        let instance = requester_destination_hex(&requester);
        let gate_with_logging_sink = |list: &TrustList, tag: &str| {
            let (trust, tmp) = list.open(tag);
            let dispatcher = Dispatcher::new(trust, Arc::new(LoggingKnockSink));
            assert!(
                dispatcher
                    .register(TEST_PATH, recorder.clone())
                    .unwrap()
                    .is_none()
            );
            responder.server.set_handler(Arc::new(dispatcher));
            tmp
        };

        let _knock_tmp = gate_with_logging_sink(
            &TrustList::default().identity(&identity, false),
            "r3-log-hashes-knock",
        );
        let err = requester
            .request(&desc, TEST_PATH, Value::from("knock"))
            .await
            .unwrap_err();
        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
        let err = requester
            .request(&desc, KNOCK_PATH, Value::from("an introduction"))
            .await
            .unwrap_err();
        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));

        let _served_tmp = gate_with_logging_sink(
            &TrustList::default().identity(&identity, true),
            "r3-log-hashes-served",
        );
        let link = identified_link(&requester, &responder, &desc).await;
        let err = requester
            .client
            .request_on_link(
                &requester.transport,
                &link,
                TEST_PATH,
                Value::Nil,
                request_deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err, R3Error::Refused(RefusalCode::NoAccess));
        let outcome = request_on(
            &requester,
            &link,
            "/nowhere",
            Value::Nil,
            request_deadline(),
        )
        .await
        .unwrap();
        assert!(DispatchError::from_value(&outcome.value).is_some());
        let outcome = request_on(
            &requester,
            &link,
            TEST_PATH,
            Value::from("served"),
            request_deadline(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.value, Value::from("served"));
        assert_eq!(recorder.seen_count(), 1);

        let logs: Vec<String> = debug_snapshot()
            .into_iter()
            .chain(warn_snapshot())
            .collect();
        let truncated = format!("from {} on link", &identity[..8]);
        assert!(
            logs.iter().any(|m| m.contains(&truncated)),
            "the scan must see the serving path's own lines"
        );
        assert!(
            logs.iter().any(|m| m.contains("Mesh knock from")),
            "the scan must see the runtime's knock sink"
        );
        let offenders: Vec<&String> = logs
            .iter()
            .filter(|m| m.contains(&identity) || m.contains(&instance))
            .collect();
        assert!(
            offenders.is_empty(),
            "log lines carry a full peer hash: {offenders:#?}"
        );
        requester.stop().await;
        responder.stop().await;
    }

    /// Handler slots come back when a handler finishes normally, not only when it times
    /// out: a full house of concurrent requests all answer, every permit returns, and the
    /// next request over the same link is served rather than dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handler_slots_return_after_every_normal_completion() {
        let recorder = Arc::new(Recorder::default());
        let (responder, requester, desc) = pair(recorder.clone()).await;
        let link = open_link(&requester.transport, &desc, "/echo", link_deadline())
            .await
            .unwrap();

        let in_flight: Vec<_> = (0..MAX_CONCURRENT_INBOUND_REQUESTS)
            .map(|n| {
                let client = requester.client.clone();
                let transport = requester.transport.clone();
                let link = link.clone();
                tokio::spawn(async move {
                    client
                        .request_on_link(
                            &transport,
                            &link,
                            "/echo",
                            Value::from(format!("req-{n}")),
                            request_deadline(),
                        )
                        .await
                })
            })
            .collect();
        for (n, task) in in_flight.into_iter().enumerate() {
            let outcome = task.await.unwrap().unwrap();
            assert_eq!(outcome.value, Value::from(format!("req-{n}")));
        }
        assert_eq!(recorder.seen_count(), MAX_CONCURRENT_INBOUND_REQUESTS);
        wait_until("every handler slot to return", || {
            responder.server.available_permits() == MAX_CONCURRENT_INBOUND_REQUESTS
        })
        .await;

        let outcome = request_on(
            &requester,
            &link,
            "/echo",
            Value::from("one more"),
            request_deadline(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.value, Value::from("one more"));
        assert_eq!(recorder.seen_count(), MAX_CONCURRENT_INBOUND_REQUESTS + 1);
        requester.stop().await;
        responder.stop().await;
    }
}
