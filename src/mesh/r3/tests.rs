use super::error::{R3Error, RefusalCode};
use super::frame::{PathHash, RequestFrame, RequestId, ResponseFrame};

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

// Two real nodes over loopback TCP. Unix-only like the node tests: the `MeshRuntime` test
// mints an owner-only identity file, which only unix implements.
#[cfg(unix)]
mod network {
    use super::super::client::{
        DEFAULT_LINK_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, Deadline, R3Client, RequestOptions,
        RequestOutcome, SizeBranch, identify, open_link,
    };
    use super::super::error::{R3Error, RefusalCode};
    use super::super::frame::{
        MAX_R3_PAYLOAD_BYTES, PathHash, RequestFrame, RequestId, ResponseFrame,
    };
    use super::super::server::{InboundRequest, R3Server, Reply, RequestHandler};
    use crate::config::{ForkRekey, Session};
    use crate::mesh::announce::AnnounceAppData;
    use crate::mesh::node::{MeshRuntime, MeshSlot, NodeOptions, REKEY_GRACE, SHUTDOWN_GRACE};
    use crate::mesh::test_support::{TempDir, mesh_paths, private_config};
    use crate::testing::{debug_snapshot, install_log_collector};

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use rand_core::OsRng;
    use rmpv::Value;
    use rns_transport::destination::link::LinkId;
    use rns_transport::destination::{DestinationDesc, DestinationName, SingleInputDestination};
    use rns_transport::hash::{AddressHash, Hash};
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use rns_transport::iface::InterfaceSharedConfig;
    use rns_transport::iface::tcp_client::TcpClient;
    use rns_transport::iface::tcp_server::TcpServer;
    use rns_transport::resource::{LINK_PACKET_MDU, ResourceEvent, ResourceEventKind};
    use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};
    use tokio_util::sync::CancellationToken;

    const POLL: Duration = Duration::from_millis(100);
    const INTEROP_TIMEOUT: Duration = Duration::from_secs(15);
    /// Reticulum's original 500-byte MTU. TCP interfaces default to `TcpClient::DEFAULT_MTU`
    /// (262144), which makes every card-sized frame a single packet; the legacy MTU puts the
    /// packet/resource boundary where small test payloads can reach it, and is what
    /// constrained interfaces still negotiate.
    const LEGACY_LINK_MTU: usize = 500;
    /// How long a dropped `Script::Hang` handler takes to go away; see `Abandoned`.
    const ABANDON_DELAY: Duration = Duration::from_millis(250);
    /// How long a request these tests never answer waits before it gives up.
    const SHORT_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
    /// Bytes of array header, request id and bin32 header around a response frame's body.
    const RESPONSE_FRAME_OVERHEAD: usize = 24;
    /// Bytes of array header, time, path hash and bin32 header around a request frame's body.
    const REQUEST_FRAME_OVERHEAD: usize = 33;

    async fn closed_port() -> u16 {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    }

    async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
        while !condition() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            sleep(POLL).await;
        }
    }

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
    }

    #[async_trait]
    impl RequestHandler for Recorder {
        async fn handle(&self, request: InboundRequest) -> Reply {
            self.seen.lock().push(Seen {
                link_id: request.link_id,
                request_id: request.request_id,
                identity: request.identity.map(|identity| identity.address_hash),
                path_hash: request.path_hash,
                branch: request.branch,
            });
            let next = self.script.lock().pop_front();
            match next {
                Some(Script::Reply(reply)) => reply,
                Some(Script::Hang) => {
                    let _abandoned = Abandoned(&self.abandoned);
                    std::future::pending().await
                }
                None => Reply::Value(request.data),
            }
        }
    }

    /// The listening node: a bare transport with a `TcpServer`, one destination and an
    /// `R3Server`, the shape `MeshConfig` cannot express since it only connects outward.
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
            let port = closed_port().await;
            let transport = Arc::new(Transport::new(TransportConfig::new(
                "b",
                &TransportIdentity::new_from_rand(OsRng),
                false,
            )));
            let server = Arc::new(R3Server::new());
            server.set_handler(handler);
            let cancel = CancellationToken::new();
            tokio::spawn(server.clone().run(
                transport.clone(),
                transport.in_link_events(),
                transport.resource_events(),
                cancel.clone(),
            ));
            let tcp = TcpServer::new(format!("127.0.0.1:{port}"), transport.iface_manager())
                .with_client_mtu(client_mtu);
            let status = tcp.runtime_status_handle();
            let iface = transport
                .iface_manager()
                .lock()
                .await
                .spawn(tcp, TcpServer::spawn);
            wait_until("the responder to listen", || {
                status.to_json()["listener_state"].as_str() == Some("listening")
            })
            .await;
            let dest = transport
                .add_destination(
                    TransportIdentity::new_from_rand(OsRng),
                    fresh_destination_name(),
                )
                .await;
            let desc = dest.lock().await.desc;
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

        async fn stop(self) {
            self.cancel.cancel();
            self.transport
                .iface_manager()
                .lock()
                .await
                .stop_interface(self.iface);
        }
    }

    /// The connecting node: a bare transport with a `TcpClient` and an `R3Client`.
    struct Requester {
        transport: Arc<Transport>,
        identity: TransportIdentity,
        client: Arc<R3Client>,
        announces: broadcast::Receiver<AnnounceEvent>,
        iface: AddressHash,
        iface_task: JoinHandle<()>,
        cancel: CancellationToken,
    }

    impl Requester {
        async fn connect(port: u16, mtu: usize) -> Self {
            let identity = TransportIdentity::new_from_rand(OsRng);
            let transport = Arc::new(Transport::new(TransportConfig::new("a", &identity, false)));
            let announces = transport.recv_announces().await;
            let client = Arc::new(R3Client::new());
            let cancel = CancellationToken::new();
            tokio::spawn(client.clone().run(
                transport.out_link_events(),
                transport.resource_events(),
                cancel.clone(),
            ));
            let tcp = TcpClient::new(format!("127.0.0.1:{port}")).with_mtu(mtu);
            let status = tcp.runtime_status_handle();
            let context = transport.iface_manager().lock().await.new_context(tcp);
            let iface = *context.channel.address();
            let iface_task = tokio::spawn(TcpClient::spawn(context));
            wait_until("the requester to connect", || {
                status.to_json()["stream_state"].as_str() == Some("connected")
            })
            .await;
            Self {
                transport,
                identity,
                client,
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
                    data,
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

    /// A body whose request frame encodes to exactly `target` bytes; the bin header grows
    /// at 256 bytes so the search is over lengths rather than arithmetic.
    fn request_body_of_encoded_len(target: usize) -> Value {
        (0..target)
            .map(|n| Value::Binary(vec![0xab; n]))
            .find(|body| RequestFrame::new("/echo", body.clone()).encode().len() == target)
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
        let desc = *desc;
        let in_flight = tokio::spawn(async move {
            client
                .request(
                    &transport,
                    &identity,
                    &desc,
                    "/slow",
                    Value::Nil,
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
        client_b: Arc<R3Client>,
        cancel_b: CancellationToken,
        node_a: Arc<MeshRuntime>,
        recorder_a: Arc<Recorder>,
        a_desc: DestinationDesc,
        _tmp: TempDir,
    }

    impl NodePair {
        async fn start(tag: &str) -> Self {
            let recorder_b = Arc::new(Recorder::default());
            // A `MeshRuntime` joins with `TcpClient`'s default MTU, so the responder matches it.
            let responder = Responder::listen(recorder_b, TcpServer::DEFAULT_CLIENT_MTU).await;
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
            Self {
                responder,
                client_b,
                cancel_b,
                node_a,
                recorder_a,
                a_desc,
                _tmp: tmp,
            }
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
            let body = request_body_of_encoded_len(target);
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
                    Value::Nil,
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
        let in_flight = tokio::spawn(async move {
            client
                .request(
                    &transport,
                    &identity,
                    &desc,
                    "/slow",
                    Value::Nil,
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
                Value::Nil,
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
                Value::from("after"),
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
                card(),
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
        let b_in_flight = tokio::spawn(async move {
            client
                .request(
                    &transport_b,
                    &identity_b,
                    &a_desc,
                    "/slow",
                    Value::Nil,
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
}
