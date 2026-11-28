/// A file size that needs a multi-segment Resource: half a MiB past the size the
/// library sends as one segment.
pub(crate) const MULTI_SEGMENT_BYTES: usize =
    rns_transport::resource::MAX_EFFICIENT_SIZE + 512 * 1024;

#[cfg(unix)]
use super::announce::AnnounceAppData;
use super::hex_lower;
#[cfg(unix)]
use super::message::{PeerBody, from_request_body, is_farewell, received_reply};
#[cfg(unix)]
pub(crate) use super::node::LAN_SILENT_NOTE;
use super::node::MeshPaths;
#[cfg(all(test, unix))]
pub(crate) use super::node::RecordingEnvoy;
#[cfg(unix)]
use super::node::arm_destination_caps;
#[cfg(unix)]
use super::node::{MeshRuntime, MeshSlot, NodeOptions};
use super::peers::PeerTable;
#[cfg(unix)]
pub(crate) use super::peers::{FarewellTime, LeftMark};
#[cfg(unix)]
pub(crate) use super::peers::{PEER_TTL, PeerSighting};
pub(crate) use super::propagation::PropagationNode;
#[cfg(unix)]
use super::propagation::pn_announce_app_data;
#[cfg(all(test, unix))]
pub(crate) use super::propagation::pn_announce_app_data as propagation_announce_app_data;
use super::propagation::propagation_node_destination;
#[cfg(all(test, unix))]
pub(crate) use super::propagation::test_support::FakeNode as StoringPropagationNode;
pub(crate) use super::propagation_fetch::{Delivery, InboundMessage, InboundSink};
#[cfg(unix)]
use super::propagation_fetch::{FETCH_TRANSFER_LIMIT_KB, GET_PATH};
pub(crate) use super::protocol::Compatibility;
#[cfg(all(test, unix))]
pub(crate) use super::request::network::{
    NodePair, Script as ResponderScript, access_body, fetch_body, hook_sink_for, installed_slot,
    share_docs_from_a, short_options, trusting_b, wire_field, wire_status,
};
#[cfg(unix)]
pub(crate) use super::request::{ACCESS_PATH, FETCH_PATH, LIST_PATH};
#[cfg(unix)]
use super::request::{
    Admission, Dispatcher, HANDLER_TIMEOUT, InboundRequest, KNOWN_PATHS, LibraryHandler,
    LoggingKnockSink, RequestHandler,
};
pub(crate) use super::request::{
    AdmittedRequest, Handler, MESSAGE_PATH, NAME_HASH_LEN, OriginName, PathHash, RefusalCode,
    Reply, RequestId,
};
use super::session_destination_name;
use super::snapshot::{BriefState, MeshSnapshot, TurnState};
#[cfg(unix)]
use super::trust::TrustOptions;
use super::trust::{KnockProof, LiveMesh, TrustStore};
use super::{mesh_config_dir, rfc3339_utc};
use crate::config::MeshConfig;
#[cfg(unix)]
use crate::config::Session;
use crate::config::mesh_config::{MeshBrief, MeshInterface};
use crate::config::todo::TodoList;

#[cfg(unix)]
use async_trait::async_trait;
use parking_lot::Mutex;
use rmpv::Value;
use rns_transport::destination::SingleInputDestination;
#[cfg(unix)]
use rns_transport::destination::link::LinkId;
#[cfg(unix)]
use rns_transport::destination::request::RequestAllow;
#[cfg(unix)]
use rns_transport::destination::{DestinationDesc, DestinationName};
#[cfg(unix)]
use rns_transport::hash::AddressHash;
#[cfg(unix)]
use rns_transport::identity::Identity;
use rns_transport::identity::PrivateIdentity as TransportIdentity;
#[cfg(unix)]
use rns_transport::iface::tcp_client::TcpClient;
#[cfg(unix)]
use rns_transport::iface::tcp_server::TcpServer;
use rns_transport::rand_core::OsRng;
#[cfg(unix)]
use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
#[cfg(unix)]
use std::sync::atomic::AtomicU16;
#[cfg(unix)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};
#[cfg(unix)]
use tokio::io::AsyncReadExt;
#[cfg(unix)]
use tokio::net::{TcpListener, TcpSocket};
#[cfg(unix)]
use tokio::sync::broadcast;
#[cfg(unix)]
use tokio::task::{JoinHandle, JoinSet};
#[cfg(unix)]
use tokio::time::{sleep, timeout};

/// How often `wait_until` polls.
#[cfg(unix)]
pub(crate) const POLL: Duration = Duration::from_millis(100);
/// Ceiling on any one wait in a loopback network test.
#[cfg(unix)]
pub(crate) const INTEROP_TIMEOUT: Duration = Duration::from_secs(15);
/// Reticulum's original 500-byte MTU. TCP interfaces default to `TcpClient::DEFAULT_MTU`
/// (262144), which makes every card-sized frame a single packet; the legacy MTU puts the
/// packet/resource boundary (link MDU 431) where small test payloads can reach it, and
/// is what constrained interfaces still negotiate.
#[cfg(unix)]
pub(crate) const LEGACY_LINK_MTU: usize = 500;

pub(crate) struct TempDir {
    pub(crate) path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        // Wall-clock nanos alone are not unique across parallel tests; a
        // process-wide counter makes every name distinct.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("coyote-mesh-{tag}-{nanos}-{seq}"));
        fs::create_dir_all(&path).unwrap();
        // The temp root is handed out resolved, since what the code under test hands
        // back is resolved too: macOS reaches /var through a link and Windows may name
        // the profile by its short form, and a fixture joined on the raw root would
        // never equal either. It goes through the same helper production uses, so a
        // temp root deeper than MAX_PATH is spelled the same way on both sides too.
        let path = super::canonicalize(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The names in `path`'s directory, sorted, so a test can pin that an atomic write
/// left nothing beside the file it replaced.
pub(crate) fn siblings_of(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// `secs` past the Unix epoch: the same instant in every run.
pub(crate) fn t_epoch(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// `secs` past a fixed instant a few hours back, so every `t_recent(secs)` is in the
/// past and well inside `PROPAGATION_NODE_TTL` of the clock the pickers read.
pub(crate) fn t_recent(secs: u64) -> SystemTime {
    static BASE: LazyLock<SystemTime> =
        LazyLock::new(|| SystemTime::now() - Duration::from_secs(10_000));
    *BASE + Duration::from_secs(secs)
}

pub(crate) fn fake_hash(fill: u8) -> String {
    hex_lower(&[fill; 16])
}

/// A msgpack map built by hand, so a request body never depends on the encoders under
/// test.
pub(crate) fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(key, value)| (Value::from(key), value))
            .collect(),
    )
}

pub(crate) struct MeshOff;

impl LiveMesh for MeshOff {
    fn peers(&self) -> Option<Arc<PeerTable>> {
        None
    }
}

/// A running mesh: the peer table, and the knocks its cache would hold.
pub(crate) struct MeshOn(
    pub(crate) Arc<PeerTable>,
    pub(crate) Mutex<Vec<(String, KnockProof)>>,
);

impl LiveMesh for MeshOn {
    fn peers(&self) -> Option<Arc<PeerTable>> {
        Some(self.0.clone())
    }

    fn knock(&self, destination_hash: &str, _now: SystemTime) -> Option<KnockProof> {
        self.1
            .lock()
            .iter()
            .find(|(destination, _)| destination == destination_hash)
            .map(|(_, proof)| proof.clone())
    }
}

/// A real announce's worth of hashes: the destination is derived from the name and
/// identity exactly as the transport does it.
pub(crate) struct Announced {
    pub(crate) destination_hash: String,
    pub(crate) identity_hash: String,
    pub(crate) name_hash: String,
}

pub(crate) fn announced(aspect: &str) -> Announced {
    announced_as(TransportIdentity::new_from_rand(OsRng), aspect)
}

pub(crate) fn announced_as(identity: TransportIdentity, aspect: &str) -> Announced {
    let name = session_destination_name(aspect);
    let destination = SingleInputDestination::new(identity, name);
    Announced {
        destination_hash: destination.desc.address_hash.to_hex_string(),
        identity_hash: destination.desc.identity.address_hash.to_hex_string(),
        name_hash: hex_lower(name.as_name_hash_slice()),
    }
}

/// A propagation node under a fresh identity, at the `lxmf.propagation` destination.
pub(crate) fn node(enabled: bool) -> PropagationNode {
    PropagationNode {
        destination: propagation_node_destination(
            *TransportIdentity::new_from_rand(OsRng).as_identity(),
        ),
        stamp_cost: 8,
        per_transfer_limit_kb: 256,
        propagation_enabled: enabled,
    }
}

pub(crate) fn hex(node: &PropagationNode) -> String {
    node.destination.address_hash.to_hex_string()
}

/// A TCP listener that accepts every connection and holds it open until the peer hangs
/// up, which is all a `TcpClient` needs to report itself connected. The counter is the
/// number of accepted streams the peer has closed. Unix-only with everything below it:
/// the loopback fixtures and the pty harness have only been run on unix so far, and
/// lifting that gate is future work.
#[cfg(unix)]
pub(crate) async fn loopback_relay() -> (SocketAddr, JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (handle, closed) = serve_as_sink(listener).await;
    (addr, handle, closed)
}

/// Serves `listener` the way `loopback_relay` does: every connection is accepted and
/// drained until the peer hangs up, and the counter is the number of streams closed.
#[cfg(unix)]
pub(crate) async fn serve_as_sink(listener: TcpListener) -> (JoinHandle<()>, Arc<AtomicUsize>) {
    let closed = Arc::new(AtomicUsize::new(0));
    let counter = closed.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut sink = [0u8; 1024];
                while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    (handle, closed)
}

/// Serves `listener` like `serve_as_sink`, except that aborting the handle closes
/// every accepted stream along with the listener, the way a daemon that exits takes
/// its sockets with it: the stand-in for an rnsd whose going away the attached
/// shared-instance client has to notice.
#[cfg(unix)]
pub(crate) async fn serve_as_daemon(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut streams = JoinSet::new();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            streams.spawn(async move {
                let mut sink = [0u8; 1024];
                while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
            });
        }
    })
}

/// A loopback port that refuses connections until the test calls `.listen(backlog)`
/// on the returned socket, which turns it into the same byte sink `loopback_relay`
/// is once handed to `serve_as_sink`. A bound socket that is not listening accepts
/// nothing, though not the same way everywhere: Linux refuses the dial, macOS leaves
/// it to time out. `TcpListener::bind` would listen at once.
#[cfg(unix)]
pub(crate) async fn held_port() -> (u16, TcpSocket) {
    let socket = TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    (socket.local_addr().unwrap().port(), socket)
}

/// A Reticulum transport node in-process: a `TcpServer` on a transport with transport
/// mode on. Two `MeshRuntime`s that both join it hear each other's announces through
/// its rebroadcasts and link to each other through it, so neither side is a stub.
#[cfg(unix)]
pub(crate) struct TransportRelay {
    transport: Arc<Transport>,
    iface: AddressHash,
    pub(crate) port: u16,
}

#[cfg(unix)]
impl TransportRelay {
    pub(crate) async fn start() -> Self {
        Self::start_on(closed_port().await).await
    }

    /// Listens on `port`. The upstream server retries a failed bind every 5 s
    /// without a word, so the first bind is watched here rather than letting a held
    /// port hang the test: while the port is still being released (a restart-shaped
    /// test reusing it, or a parallel test's ephemeral grab) the bind is retried a
    /// bounded number of times; any other failure panics. Rebroadcasts every
    /// announce a few times, ~5 s apart, so a node that joins after the other one
    /// announced still hears it.
    pub(crate) async fn start_on(port: u16) -> Self {
        Self::start_on_with_retries(port, 4).await
    }

    /// `start_on` with a subscription to the relay's announces taken before it
    /// listens: a node whose client is redialing the port announces as soon as the
    /// bind answers, which can be before `start_on` has returned.
    pub(crate) async fn start_on_subscribed(
        port: u16,
    ) -> (Self, broadcast::Receiver<AnnounceEvent>) {
        Self::start_on_with_retries_subscribed(port, 4).await
    }

    /// How many times the first bind is retried while the port reads as in use.
    const BIND_RETRIES: usize = 10;
    /// The pause between two of those attempts.
    const BIND_RETRY_PAUSE: Duration = Duration::from_millis(200);

    /// `start_on` rebroadcasting each announce once plus `retries` more times. With
    /// no retries a node that joins later hears nothing it did not ask for, and
    /// learns an earlier announce only from the relay's answer to its own path
    /// request.
    pub(crate) async fn start_on_with_retries(port: u16, retries: u8) -> Self {
        Self::start_on_with_retries_subscribed(port, retries)
            .await
            .0
    }

    async fn start_on_with_retries_subscribed(
        port: u16,
        retries: u8,
    ) -> (Self, broadcast::Receiver<AnnounceEvent>) {
        let mut config =
            TransportConfig::new("relay", &TransportIdentity::new_from_rand(OsRng), false);
        config.set_transport_enabled(true);
        config.set_announce_retry_limit(retries);
        let transport = Arc::new(Transport::new(config));
        let announces = transport.recv_announces().await;
        let mut attempt = 1;
        let (iface, status) = loop {
            let (tcp, first_bind) =
                TcpServer::new(format!("127.0.0.1:{port}"), transport.iface_manager())
                    .with_client_mtu(TcpServer::DEFAULT_CLIENT_MTU)
                    .with_startup_result();
            let status = tcp.runtime_status_handle();
            let iface = transport
                .iface_manager()
                .lock()
                .await
                .spawn(tcp, TcpServer::spawn);
            match first_bind.await.expect("the relay reports its first bind") {
                Ok(()) => break (iface, status),
                // The server reports the bind error as its Display text, not as an
                // `io::Error`, so the kind is read off the OS message.
                Err(err) if err.contains("Address already in use") => {
                    // Stop the half-spawned server before giving up as well as before
                    // retrying: left alive it keeps re-binding every few seconds and
                    // would grab the port the moment its holder lets go.
                    let _ = timeout(Duration::from_secs(5), transport.stop_interface(iface)).await;
                    assert!(
                        attempt <= Self::BIND_RETRIES,
                        "the relay's first bind of 127.0.0.1:{port} failed after {attempt} attempts: {err}"
                    );
                    attempt += 1;
                    sleep(Self::BIND_RETRY_PAUSE).await;
                }
                Err(err) => {
                    panic!("the relay's first bind of 127.0.0.1:{port} failed: {err}")
                }
            }
        };
        wait_until("the relay to listen", || {
            status.to_json()["listener_state"].as_str() == Some("listening")
        })
        .await;
        (
            Self {
                transport,
                iface,
                port,
            },
            announces,
        )
    }

    /// A subscription to the announces the relay hears, taken before the event a
    /// test waits for so none is missed.
    pub(crate) async fn announces(&self) -> broadcast::Receiver<AnnounceEvent> {
        self.transport.recv_announces().await
    }

    /// Consumes `announces` until the relay has heard one for `destination`. From
    /// then on the relay's path table points at the interface that announce came
    /// in on, which matters after a node restarts: until its start announce lands,
    /// the relay still forwards towards its dead link.
    pub(crate) async fn heard_from(
        announces: &mut broadcast::Receiver<AnnounceEvent>,
        destination: &str,
    ) {
        Self::heard_from_each(announces, &[destination]).await;
    }

    /// `heard_from` for several destinations in whichever order their announces
    /// arrive: nodes whose clients redial a restarted relay together announce
    /// within the same millisecond, and an announce consumed while waiting for
    /// another node is not heard again.
    pub(crate) async fn heard_from_each(
        announces: &mut broadcast::Receiver<AnnounceEvent>,
        destinations: &[&str],
    ) {
        let mut pending = destinations
            .iter()
            .map(|destination| {
                (
                    *destination,
                    AddressHash::new_from_hex_string(destination).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
        while let Some((destination, _)) = pending.first() {
            let event = tokio::time::timeout_at(deadline, announces.recv())
                .await
                .unwrap_or_else(|_| panic!("the relay must hear {destination}'s announce"))
                .unwrap();
            let heard = event.destination.lock().await.desc.address_hash;
            pending.retain(|(_, hash)| *hash != heard);
        }
    }

    pub(crate) async fn stop(self) {
        let _ = timeout(
            Duration::from_secs(5),
            self.transport.stop_interface(self.iface),
        )
        .await;
    }
}

/// How long two nodes on a `TransportRelay` get to file each other.
#[cfg(unix)]
const PEER_FILING_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the asker re-requests the answerer's path while waiting to file it.
#[cfg(unix)]
const PATH_REQUEST_INTERVAL: Duration = Duration::from_secs(5);

/// A `/status` round trip from the node in `asker` to the node in `answerer` over
/// whatever relay joins them, each trusting the other's destination the way
/// `.mesh trust` does. Panics with the step that did not happen.
#[cfg(unix)]
pub(crate) async fn status_round_trip(
    asker: &Arc<MeshSlot>,
    answerer: &Arc<MeshSlot>,
) -> super::card::StatusCard {
    let asking = asker.get().expect("the asking slot holds a node");
    let answering = answerer.get().expect("the answering slot holds a node");
    wait_for_peer(&asking, &answering.current_destination_hash()).await;
    wait_for_peer(&answering, &asking.current_destination_hash()).await;
    asking
        .trust()
        .trust_destination(
            asker.as_ref(),
            &answering.current_destination_hash(),
            TrustOptions::default(),
            SystemTime::now(),
        )
        .expect("the asker trusts the answerer");
    answering
        .trust()
        .trust_destination(
            answerer.as_ref(),
            &asking.current_destination_hash(),
            TrustOptions::default(),
            SystemTime::now(),
        )
        .expect("the answerer trusts the asker");
    let desc = asking
        .resolve_destination(&answering.current_destination_hash())
        .await
        .expect("the answerer's destination resolves from its announce");
    asking
        .request_status(crate::mesh::message::Route::unasked(desc))
        .await
        .unwrap_or_else(|err| panic!("the status request failed: {err:?}"))
}

/// Waits for `runtime` to file `destination`. An announce the relay heard before this
/// node joined is not replayed on its own; the relay answers a path request with the
/// announce it cached, so one goes out every `PATH_REQUEST_INTERVAL`.
#[cfg(unix)]
pub(crate) async fn wait_for_peer(runtime: &MeshRuntime, destination: &str) {
    let peers = runtime.peers();
    let hash = AddressHash::new_from_hex_string(destination).unwrap();
    let transport = runtime
        .transport_handle()
        .await
        .expect("the node is running");
    let deadline = tokio::time::Instant::now() + PEER_FILING_TIMEOUT;
    let mut next_request = tokio::time::Instant::now();
    while peers.get(destination).is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {destination} to be filed"
        );
        if tokio::time::Instant::now() >= next_request {
            transport.request_path(&hash, None, None).await;
            next_request = tokio::time::Instant::now() + PATH_REQUEST_INTERVAL;
        }
        sleep(POLL).await;
    }
}

/// A relay with nodes A and B on it where A has filed B but B holds nothing of A:
/// no peer table row and no identity in its transport. Both meet first, then B is
/// stopped, its `peers.json` deleted and B restarted, so the relay's single
/// rebroadcast of A's start announce cannot reach the new B. The restarted B's start
/// announce can go out before its client has dialed the relay, so the relay asks for
/// B's path until it hears B on the new link. Returns the relay, A and B.
#[cfg(unix)]
pub(crate) async fn pair_where_b_has_not_heard_a(
    tag: &str,
) -> (TransportRelay, StartedRuntime, StartedRuntime) {
    let relay = TransportRelay::start_on_with_retries(closed_port().await, 0).await;
    let a = started_runtime_on(&format!("{tag}-a"), relay.port).await;
    let mut b = started_runtime_on(&format!("{tag}-b"), relay.port).await;
    let to_a = a.runtime.current_destination_hash();
    let to_b = b.runtime.current_destination_hash();
    wait_for_peer(&a.runtime, &to_b).await;
    wait_for_peer(&b.runtime, &to_a).await;

    let mut relay_announces = relay.announces().await;
    let slot_b = Arc::new(MeshSlot::default());
    slot_b.install(b.runtime.clone()).unwrap();
    assert!(slot_b.stop().await.unwrap());
    std::fs::remove_file(super::mesh_cache_dir(&mesh_paths(&b.tmp).cache_dir).join("peers.json"))
        .expect("B's stop wrote its peer table");
    b.restart_on(relay.port).await;
    let hash_b = AddressHash::new_from_hex_string(&to_b).unwrap();
    let deadline = tokio::time::Instant::now() + PEER_FILING_TIMEOUT;
    loop {
        let heard = tokio::time::timeout(PATH_REQUEST_INTERVAL, async {
            loop {
                let event = match relay_announces.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        panic!("the relay's announce channel closed before it heard B")
                    }
                };
                if event.destination.lock().await.desc.address_hash == hash_b {
                    return;
                }
            }
        });
        if heard.await.is_ok() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the relay never heard the restarted B ({to_b})"
        );
        relay.transport.request_path(&hash_b, None, None).await;
    }

    assert!(b.runtime.peers().get(&to_a).is_none(), "B has no row for A");
    assert!(
        !b.knows_identity_of(&to_a).await,
        "B's transport holds nothing of A"
    );
    (relay, a, b)
}

/// A runtime that has filed and trusts a `PeerStub` whose trust list names nobody, so
/// a link to the stub comes up and every request on it draws the silence a Coyote
/// gives an identity it does not know. The slot holds the runtime, as a trust grant
/// needs a live node.
#[cfg(unix)]
pub(crate) async fn beside_a_stub_that_does_not_know_it(
    tag: &str,
) -> (PeerStub, StartedRuntime, Arc<MeshSlot>) {
    let stub = PeerStub::listen(&format!("{tag}-stub"), TcpServer::DEFAULT_CLIENT_MTU).await;
    let started = started_runtime_on(tag, stub.port()).await;
    let slot = Arc::new(MeshSlot::default());
    slot.install(started.runtime.clone()).unwrap();
    stub.announce(Some("Stub")).await;
    let to = stub.destination_hex();
    stub.wait_to_be_filed(&started.runtime.peers(), &to).await;
    started
        .runtime
        .trust()
        .trust_destination(
            slot.as_ref(),
            &to,
            TrustOptions::default(),
            SystemTime::now(),
        )
        .expect("the node trusts the stub");
    (stub, started, slot)
}

pub(crate) fn private_config(port: u16) -> MeshConfig {
    MeshConfig {
        interfaces: vec![MeshInterface::Private {
            host: "127.0.0.1".to_string(),
            port,
        }],
        ..MeshConfig::default()
    }
}

/// A trust list in the file's own format, so a test starts from any state without the
/// mutators, which need a live peer table to prove a destination's identity.
#[derive(Default)]
pub(crate) struct TrustList {
    identities: Vec<(String, bool)>,
    destinations: Vec<(String, String)>,
    denied: Vec<String>,
    blocked: Vec<String>,
}

impl TrustList {
    pub(crate) fn identity(mut self, hash: &str, all_destinations: bool) -> Self {
        self.identities.push((hash.to_string(), all_destinations));
        self
    }

    /// A destination bound to `identity`, which gets the identity record the file
    /// requires (without `all_destinations`) if it has none yet.
    pub(crate) fn destination(mut self, hash: &str, identity: &str) -> Self {
        if !self.identities.iter().any(|(known, _)| known == identity) {
            self.identities.push((identity.to_string(), false));
        }
        self.destinations
            .push((hash.to_string(), identity.to_string()));
        self
    }

    pub(crate) fn deny(mut self, hash: &str) -> Self {
        self.denied.push(hash.to_string());
        self
    }

    pub(crate) fn block(mut self, hash: &str) -> Self {
        self.blocked.push(hash.to_string());
        self
    }

    /// Writes the list as `trust.yaml` under `config_dir`, where a node started with
    /// that config dir reads it.
    pub(crate) fn write(&self, config_dir: &Path) {
        let ts = rfc3339_utc(UNIX_EPOCH + Duration::from_secs(1_790_000_000));
        let mut text = format!("version: {}\n", super::trust::TRUST_FILE_VERSION);
        if !self.identities.is_empty() {
            text.push_str("identities:\n");
            for (hash, all_destinations) in &self.identities {
                text.push_str(&format!(
                    "  {hash}:\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n    all_destinations: {all_destinations}\n"
                ));
            }
        }
        if !self.destinations.is_empty() {
            text.push_str("destinations:\n");
            for (hash, identity) in &self.destinations {
                text.push_str(&format!(
                    "  {hash}:\n    identity: {identity}\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n"
                ));
            }
        }
        for (section, hashes) in [
            ("denied_destinations", &self.denied),
            ("blocked_identities", &self.blocked),
        ] {
            if !hashes.is_empty() {
                text.push_str(&format!("{section}:\n"));
                for hash in hashes {
                    text.push_str(&format!("  {hash}:\n    added_at: {ts}\n    note: null\n"));
                }
            }
        }
        let path = mesh_config_dir(config_dir).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
    }

    pub(crate) fn open(&self, tag: &str) -> (std::sync::Arc<TrustStore>, TempDir) {
        let tmp = TempDir::new(tag);
        self.write(&tmp.path);
        (
            std::sync::Arc::new(TrustStore::open(&tmp.path).unwrap()),
            tmp,
        )
    }
}

/// The ports `closed_port` hands out.
#[cfg(unix)]
const CLOSED_PORT_POOL: std::ops::Range<u16> = 20_000..30_000;

/// A loopback port nothing listens on, never handed out twice in one process. The
/// pool sits below the default ephemeral ranges (Linux 32768-60999, macOS
/// 49152-65535), so a later `bind` to port 0 anywhere in the process cannot be given
/// a port a test holds back to bind itself later. The walk through the pool starts at
/// a multiplicative hash of the process id spread over the whole pool, so test
/// processes running side by side on one host draw from different stretches of it.
/// A port something else has bound right now is passed over.
#[cfg(unix)]
pub(crate) async fn closed_port() -> u16 {
    static DRAWN: AtomicU16 = AtomicU16::new(0);
    let start = pool_start(std::process::id());
    loop {
        let drawn = draw(&DRAWN);
        let port = pool_port(start, drawn);
        if let Ok(probe) = TcpListener::bind(("127.0.0.1", port)).await {
            drop(probe);
            return port;
        }
    }
}

#[cfg(unix)]
const fn pool_span() -> u16 {
    CLOSED_PORT_POOL.end - CLOSED_PORT_POOL.start
}

/// Takes the next draw from `drawn` and returns the value before it; the counter
/// stops at the pool's span instead of wrapping, and a draw past it panics.
#[cfg(unix)]
fn draw(drawn: &AtomicU16) -> u16 {
    let mut current = drawn.load(Ordering::Relaxed);
    loop {
        assert!(
            current < pool_span(),
            "every port in {CLOSED_PORT_POOL:?} was handed out"
        );
        match drawn.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(previous) => return previous,
            Err(actual) => current = actual,
        }
    }
}

/// The port `drawn` steps past `start` into the pool, wrapping at its end.
#[cfg(unix)]
fn pool_port(start: u16, drawn: u16) -> u16 {
    let step = (u32::from(start) + u32::from(drawn)) % u32::from(pool_span());
    CLOSED_PORT_POOL.start + u16::try_from(step).unwrap()
}

/// Where a process with id `pid` starts its walk through the pool: Fibonacci hashing
/// scatters neighbouring ids across the whole span.
#[cfg(unix)]
fn pool_start(pid: u32) -> u16 {
    let scaled = (u64::from(pid.wrapping_mul(0x9E37_79B9)) * u64::from(pool_span())) >> 32;
    u16::try_from(scaled).unwrap()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Every start and draw a `u16` can hold lands in the pool without overflowing.
    #[test]
    fn pool_port_stays_in_the_pool_and_wraps_at_its_end() {
        let span = pool_span();
        assert_eq!(pool_port(0, 0), CLOSED_PORT_POOL.start);
        assert_eq!(pool_port(span - 1, 0), CLOSED_PORT_POOL.end - 1);
        assert_eq!(pool_port(span - 1, 1), CLOSED_PORT_POOL.start);
        assert!(CLOSED_PORT_POOL.contains(&pool_port(u16::MAX, u16::MAX)));
        for start in [0, 1, span / 2, span - 1, span, u16::MAX] {
            for drawn in [0, 1, span - 1, span, u16::MAX] {
                assert!(CLOSED_PORT_POOL.contains(&pool_port(start, drawn)));
            }
        }
    }

    /// The counter hands out each draw below the span once and stops at the span.
    #[test]
    fn draw_counts_up_to_the_span_and_stops_there() {
        let drawn = AtomicU16::new(pool_span() - 1);
        assert_eq!(draw(&drawn), pool_span() - 1);
        assert_eq!(drawn.load(Ordering::Relaxed), pool_span());
        let past = std::panic::catch_unwind(|| draw(&drawn));
        assert!(past.is_err(), "a draw past the span panics");
        assert_eq!(drawn.load(Ordering::Relaxed), pool_span());
    }

    /// Neighbouring process ids start in both halves of the pool, never past its end.
    #[test]
    fn pool_start_spreads_process_ids_over_the_whole_pool() {
        let span = pool_span();
        for pid in [0, 1, u32::MAX] {
            assert!(pool_start(pid) < span, "pid {pid}");
        }
        let starts: Vec<u16> = (0..1000).map(pool_start).collect();
        assert!(starts.iter().all(|&start| start < span));
        assert!(starts.iter().any(|&start| start < span / 2));
        assert!(starts.iter().any(|&start| start >= span / 2));
    }

    /// Successive draws in one process are all in the pool and never repeat.
    #[tokio::test]
    async fn closed_port_hands_out_distinct_ports_from_the_pool() {
        let mut ports = Vec::new();
        for _ in 0..50 {
            ports.push(closed_port().await);
        }
        assert!(ports.iter().all(|port| CLOSED_PORT_POOL.contains(port)));
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 50);
    }
}

#[cfg(unix)]
pub(crate) async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        sleep(POLL).await;
    }
}

/// Drops `runtime`'s path to `destination_hex` while its transport keeps the
/// description, as an expired route leaves a held peer. Whether there was a path.
#[cfg(unix)]
pub(crate) async fn expire_path(runtime: &MeshRuntime, destination_hex: &str) -> bool {
    let hash = AddressHash::new_from_hex_string(destination_hex).unwrap();
    runtime
        .transport_handle()
        .await
        .unwrap()
        .expire_path(&hash)
        .await
}

/// Reticulum ingress control on an interface younger than two hours holds every
/// announce for an unknown destination for 360 s once announces arrive faster than
/// 3.5 a second. The relay echoes a node's start announce back at it, so a fresh peer's
/// announce landing in that burst would be held past every wait in the suites.
#[cfg(unix)]
pub(crate) async fn disable_ingress_control(runtime: &MeshRuntime) {
    let transport = runtime
        .transport_handle()
        .await
        .expect("the node just started");
    let manager = transport.iface_manager();
    let mut manager = manager.lock().await;
    for iface in manager.interface_hashes() {
        let mut config = manager.shared_config(&iface).cloned().unwrap_or_default();
        config.ingress_control = Some(false);
        manager.set_shared_config(iface, config);
    }
}

/// A listening node: a bare transport with a `TcpServer` and one destination whose
/// request paths a `LibraryHandler` answers on, the shape `MeshConfig` cannot express
/// since it only connects outward.
#[cfg(unix)]
pub(crate) struct Listener {
    pub(crate) transport: Arc<Transport>,
    pub(crate) adapter: Arc<LibraryHandler>,
    pub(crate) dest: Arc<tokio::sync::Mutex<SingleInputDestination>>,
    pub(crate) desc: DestinationDesc,
    pub(crate) iface: AddressHash,
    pub(crate) port: u16,
}

#[cfg(unix)]
impl Listener {
    /// Listens with `handler` behind every path in `paths`, bounded by `HANDLER_TIMEOUT`.
    pub(crate) async fn listen(
        handler: Arc<dyn RequestHandler>,
        client_mtu: usize,
        identity: TransportIdentity,
        name: DestinationName,
        paths: &[&str],
    ) -> Self {
        Self::listen_with_timeout(handler, client_mtu, identity, name, paths, HANDLER_TIMEOUT).await
    }

    /// `listen` with the handler bound by `handler_timeout` instead. The destination is
    /// armed by the node's own `arm_destination_caps`, so the library refuses an oversize
    /// request before the handler sees it and an oversize reply before it is sent.
    pub(crate) async fn listen_with_timeout(
        handler: Arc<dyn RequestHandler>,
        client_mtu: usize,
        identity: TransportIdentity,
        name: DestinationName,
        paths: &[&str],
        handler_timeout: Duration,
    ) -> Self {
        let port = closed_port().await;
        let transport = Arc::new(Transport::new(TransportConfig::new(
            "b",
            &TransportIdentity::new_from_rand(OsRng),
            false,
        )));
        let adapter =
            Arc::new(LibraryHandler::new(handler).with_handler_timeout_for_test(handler_timeout));
        let tcp = TcpServer::new(format!("127.0.0.1:{port}"), transport.iface_manager())
            .with_client_mtu(client_mtu);
        let status = tcp.runtime_status_handle();
        let iface = transport
            .iface_manager()
            .lock()
            .await
            .spawn(tcp, TcpServer::spawn);
        wait_until("the listener to listen", || {
            status.to_json()["listener_state"].as_str() == Some("listening")
        })
        .await;
        let mut destination = SingleInputDestination::new(identity, name);
        arm_destination_caps(&mut destination);
        for path in paths {
            destination
                .register_request_handler(path, adapter.clone(), RequestAllow::All, true)
                .unwrap();
        }
        let dest = transport.register_destination(destination).await;
        let desc = dest.lock().await.desc;
        Self {
            transport,
            adapter,
            dest,
            desc,
            iface,
            port,
        }
    }

    pub(crate) async fn announce(&self, app_data: Option<&[u8]>) {
        let packet = self.dest.lock().await.announce(OsRng, app_data).unwrap();
        self.transport.send_packet(packet).await;
    }

    pub(crate) async fn stop(self) {
        self.transport
            .iface_manager()
            .lock()
            .await
            .stop_interface(self.iface);
    }
}

/// One `/get` round as the fake propagation node saw it.
#[cfg(unix)]
#[derive(Clone)]
pub(crate) struct Seen {
    pub(crate) request_id: RequestId,
    pub(crate) identity: Option<AddressHash>,
    pub(crate) data: Value,
}

/// Answers each round with the next scripted value and records what arrived. A
/// listening post, not a propagation node: it never reads the request.
#[cfg(unix)]
#[derive(Default)]
pub(crate) struct Script {
    seen: Mutex<Vec<Seen>>,
    replies: Mutex<VecDeque<Value>>,
}

#[cfg(unix)]
impl Script {
    pub(crate) fn reply_with(&self, values: impl IntoIterator<Item = Value>) {
        self.replies.lock().extend(values);
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().clone()
    }
}

#[cfg(unix)]
#[async_trait]
impl RequestHandler for Script {
    fn admit(&self, _link_id: LinkId, _identity: Option<&Identity>) -> Admission {
        Admission::Admit
    }

    async fn handle(&self, request: InboundRequest) -> Reply {
        self.seen.lock().push(Seen {
            request_id: request.request_id,
            identity: request.identity.map(|identity| identity.address_hash),
            data: request.data,
        });
        let next = self.replies.lock().pop_front();
        match next {
            Some(value) => Reply::Value(value),
            None => Reply::Silent,
        }
    }
}

/// A `Listener` on `lxmf.propagation` with a `Script` behind it.
#[cfg(unix)]
pub(crate) struct FakeNode {
    pub(crate) listener: Listener,
    pub(crate) script: Arc<Script>,
}

#[cfg(unix)]
impl FakeNode {
    pub(crate) async fn listen() -> Self {
        Self::listen_with_mtu(LEGACY_LINK_MTU).await
    }

    pub(crate) async fn listen_with_mtu(client_mtu: usize) -> Self {
        let script = Arc::new(Script::default());
        let listener = Listener::listen(
            script.clone(),
            client_mtu,
            TransportIdentity::new_from_rand(OsRng),
            DestinationName::new("lxmf", "propagation"),
            &[GET_PATH],
        )
        .await;
        Self { listener, script }
    }

    pub(crate) async fn announce(&self) {
        self.listener
            .announce(Some(&pn_announce_app_data(
                true,
                0,
                FETCH_TRANSFER_LIMIT_KB as i64,
            )))
            .await;
    }

    /// Announces `identity`'s `name` destination from the node's transport, the way a
    /// peer's own announce reaches us through the mesh, and returns its hash.
    pub(crate) async fn announce_as(
        &self,
        identity: TransportIdentity,
        name: DestinationName,
    ) -> AddressHash {
        let dest = self
            .listener
            .transport
            .add_destination(identity, name)
            .await;
        let mut dest = dest.lock().await;
        let packet = dest.announce(OsRng, None).unwrap();
        self.listener.transport.send_packet(packet).await;
        dest.desc.address_hash
    }

    pub(crate) fn hex(&self) -> String {
        self.listener.desc.address_hash.to_hex_string()
    }

    pub(crate) async fn stop(self) {
        self.listener.stop().await;
    }
}

/// Serves `/message` the way a peer's slot does, minus the slot: each body is decoded,
/// kept, and acknowledged by id.
#[cfg(unix)]
#[derive(Default)]
struct MessageRecorder {
    seen: Mutex<Vec<PeerBody>>,
}

#[cfg(unix)]
#[async_trait]
impl Handler for MessageRecorder {
    async fn handle(&self, request: AdmittedRequest) -> Reply {
        match from_request_body(&request.body) {
            Ok(body) => {
                let reply = received_reply(&body.id);
                self.seen.lock().push(body);
                Reply::Value(reply)
            }
            Err(_) => Reply::Code(RefusalCode::InvalidData),
        }
    }
}

/// A trusted, reachable peer as a `MeshRuntime` sees one: a `Listener` behind the real
/// dispatcher over its own trust list, recording every peer message it is sent. Trust is
/// granted after the fact with `trust`, since the runtime that will be trusted needs
/// this stub's port before it can start.
#[cfg(unix)]
pub(crate) struct PeerStub {
    listener: Listener,
    identity: TransportIdentity,
    recorder: Arc<MessageRecorder>,
    /// The trust list in force and the handlers beside the recorder; the dispatcher
    /// is rebuilt from both whenever either changes.
    list: Mutex<TrustList>,
    extra: Mutex<Vec<(&'static str, Arc<dyn Handler>)>>,
    trust_dir: TempDir,
    /// What the last `announce` carried, so a wait on the node filing this stub can
    /// announce it again unchanged.
    last_announce: Mutex<Option<Vec<u8>>>,
}

#[cfg(unix)]
impl PeerStub {
    pub(crate) async fn listen(tag: &str, client_mtu: usize) -> Self {
        let identity = TransportIdentity::new_from_rand(OsRng);
        let recorder = Arc::new(MessageRecorder::default());
        let trust_dir = TempDir::new(tag);
        let handler = Self::gate(&TrustList::default(), &trust_dir, recorder.clone(), &[]);
        let listener = Listener::listen(
            handler,
            client_mtu,
            identity.clone(),
            session_destination_name(tag),
            &KNOWN_PATHS,
        )
        .await;
        Self {
            listener,
            identity,
            recorder,
            list: Mutex::new(TrustList::default()),
            extra: Mutex::new(Vec::new()),
            trust_dir,
            last_announce: Mutex::new(None),
        }
    }

    fn gate(
        list: &TrustList,
        trust_dir: &TempDir,
        recorder: Arc<MessageRecorder>,
        extra: &[(&'static str, Arc<dyn Handler>)],
    ) -> Arc<dyn RequestHandler> {
        list.write(&trust_dir.path);
        let trust = Arc::new(TrustStore::open(&trust_dir.path).unwrap());
        let dispatcher = Dispatcher::new(trust, Arc::new(LoggingKnockSink));
        dispatcher.register(MESSAGE_PATH, recorder).unwrap();
        for (path, handler) in extra {
            dispatcher.register(path, handler.clone()).unwrap();
        }
        Arc::new(dispatcher)
    }

    fn regate(&self) {
        self.listener.adapter.set_handler(Self::gate(
            &self.list.lock(),
            &self.trust_dir,
            self.recorder.clone(),
            &self.extra.lock(),
        ));
    }

    /// Serves `path` with `handler` beside the recorder.
    pub(crate) fn serve(&self, path: &'static str, handler: Arc<dyn Handler>) {
        self.extra.lock().push((path, handler));
        self.regate();
    }

    /// Trusts the instance at `destination_hex` bound to `identity_hex`, so its requests
    /// reach the recorder instead of knocking.
    pub(crate) fn trust(&self, destination_hex: &str, identity_hex: &str) {
        *self.list.lock() = TrustList::default().destination(destination_hex, identity_hex);
        self.regate();
    }

    /// Trusts every instance of `identity_hex`, as a peer that trusts all of a
    /// person's sessions does.
    pub(crate) fn trust_identity(&self, identity_hex: &str) {
        *self.list.lock() = TrustList::default().identity(identity_hex, true);
        self.regate();
    }

    /// Knows `identity_hex` without trusting any of its instances: a knock from one
    /// of them is admitted and refused `NoAccess`, which is the knock landing.
    pub(crate) fn know_identity(&self, identity_hex: &str) {
        *self.list.lock() = TrustList::default().identity(identity_hex, false);
        self.regate();
    }

    /// Announces as a Coyote node, which is what gets this stub into a runtime's peer
    /// table with a path.
    pub(crate) async fn announce(&self, display_name: Option<&str>) {
        let app_data = AnnounceAppData {
            version: 1,
            display_name: display_name.map(str::to_string),
        }
        .encode()
        .unwrap();
        *self.last_announce.lock() = Some(app_data.clone());
        self.listener.announce(Some(&app_data)).await;
    }

    /// Waits for `peers` to hold this stub under `to`, announcing again every
    /// `REANNOUNCE` until it does. An announce is one packet with no retry of its own,
    /// and the node reports its relay connected as soon as the socket is while the stub
    /// registers the accepted client a task later, so the first packet can leave with
    /// nobody to receive it. A node that heard it files the stub within one poll and
    /// never hears a second; the cadence stays far below the announce burst rate that
    /// trips ingress control.
    pub(crate) async fn wait_to_be_filed(&self, peers: &crate::mesh::peers::PeerTable, to: &str) {
        const REANNOUNCE: Duration = Duration::from_secs(1);
        let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
        let mut next_announce = tokio::time::Instant::now() + REANNOUNCE;
        while peers.get(to).is_none() {
            let now = tokio::time::Instant::now();
            assert!(
                now < deadline,
                "timed out waiting for the node to file the stub"
            );
            if now >= next_announce {
                let app_data = self.last_announce.lock().clone();
                self.listener.announce(app_data.as_deref()).await;
                next_announce = now + REANNOUNCE;
            }
            sleep(POLL).await;
        }
    }

    pub(crate) fn port(&self) -> u16 {
        self.listener.port
    }

    pub(crate) fn destination_hex(&self) -> String {
        self.listener.desc.address_hash.to_hex_string()
    }

    pub(crate) fn identity_hex(&self) -> String {
        self.identity.address_hash().to_hex_string()
    }

    /// The name this stub's instance is derived from, as a stored message names it.
    pub(crate) fn origin(&self) -> OriginName {
        OriginName::of(&self.listener.desc.name)
    }

    /// Every well-formed `/message` body received so far, in arrival order.
    pub(crate) fn seen(&self) -> Vec<PeerBody> {
        self.recorder.seen.lock().clone()
    }

    /// Takes the farewells received so far out of what `seen` returns, in arrival
    /// order, so a test that stops a node beside the stub can check the goodbye and
    /// read every other message as before.
    pub(crate) fn take_farewells(&self) -> Vec<PeerBody> {
        let mut seen = self.recorder.seen.lock();
        let (farewells, rest) = seen
            .drain(..)
            .partition(|body| is_farewell(body.kind, body.fields.as_ref()));
        *seen = rest;
        farewells
    }

    pub(crate) async fn stop(self) {
        self.listener.stop().await;
    }
}

/// A sighting whose destination really derives from its identity and name hash, as
/// `trust_destination` requires, for a peer that never announces on the wire.
#[cfg(unix)]
pub(crate) fn derived_sighting(aspect: &str, display_name: Option<&str>) -> PeerSighting {
    let identity = TransportIdentity::new_from_rand(OsRng);
    let name = session_destination_name(aspect);
    let desc = SingleInputDestination::new(identity, name).desc;
    PeerSighting {
        destination_hash: desc.address_hash.to_hex_string(),
        identity_hash: desc.identity.address_hash.to_hex_string(),
        name_hash: hex_lower(name.as_name_hash_slice()),
        public_key: None,
        display_name: display_name.map(str::to_string),
        protocol_version: 1,
        hops: 1,
    }
}

/// A connecting node: a bare transport with a `TcpClient`, and an identity to prove on
/// links.
#[cfg(unix)]
pub(crate) struct Connector {
    pub(crate) transport: Arc<Transport>,
    pub(crate) identity: TransportIdentity,
    pub(crate) announces: broadcast::Receiver<AnnounceEvent>,
    pub(crate) iface: AddressHash,
    pub(crate) iface_task: JoinHandle<()>,
}

#[cfg(unix)]
impl Connector {
    pub(crate) async fn connect(port: u16, mtu: usize) -> Self {
        let identity = TransportIdentity::new_from_rand(OsRng);
        let transport = Arc::new(Transport::new(TransportConfig::new("a", &identity, false)));
        let announces = transport.recv_announces().await;
        let tcp = TcpClient::new(format!("127.0.0.1:{port}")).with_mtu(mtu);
        let status = tcp.runtime_status_handle();
        let context = transport.iface_manager().lock().await.new_context(tcp);
        let iface = *context.channel.address();
        let iface_task = tokio::spawn(TcpClient::spawn(context));
        wait_until("the connector to connect", || {
            status.to_json()["stream_state"].as_str() == Some("connected")
        })
        .await;
        Self {
            transport,
            identity,
            announces,
            iface,
            iface_task,
        }
    }

    /// Consumes announces until the one for `hash` arrives and returns its description
    /// and app_data as the transport delivered them.
    pub(crate) async fn learn(&mut self, hash: &AddressHash) -> (DestinationDesc, Vec<u8>) {
        let deadline = tokio::time::Instant::now() + INTEROP_TIMEOUT;
        loop {
            let event = tokio::time::timeout_at(deadline, self.announces.recv())
                .await
                .expect("the connector must hear the announce")
                .unwrap();
            let desc = event.destination.lock().await.desc;
            if desc.address_hash == *hash {
                return (desc, event.app_data.as_slice().to_vec());
            }
        }
    }

    pub(crate) async fn stop(self) {
        // Bounded so a transport that stalls on the stop cannot hang the test.
        let _ = timeout(
            super::node::SHUTDOWN_GRACE,
            self.transport.stop_interface(self.iface),
        )
        .await;
        self.iface_task.abort();
    }
}

pub(crate) fn mesh_paths(tmp: &TempDir) -> MeshPaths {
    MeshPaths {
        identity_path: tmp.path.join("config").join("mesh").join("identity.key"),
        cache_dir: tmp.path.join("cache"),
        config_dir: tmp.path.join("config"),
    }
}

pub(crate) fn snapshot_fixture() -> MeshSnapshot {
    MeshSnapshot {
        objective: Some("ship it".into()),
        state: TurnState::idle_now(),
        repo: None,
        todo: TodoList::default(),
        brief: BriefState {
            mode: MeshBrief::Auto,
            text: None,
            digest_generated_at: None,
        },
        session: None,
        cwd: PathBuf::new(),
        captured_at: SystemTime::now(),
    }
}

pub(crate) fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Every `.rs` file under `src/mesh`, for the tests that grep the module's own source.
pub(crate) fn rust_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let mut sources = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("mesh"),
        &mut sources,
    );
    sources
}

/// A source file with its line endings normalised to `\n`, so the marker searches
/// the scanning tests do (`"#[cfg(test)]\nmod tests"` and the like) hold on a CRLF
/// checkout as well.
pub(crate) fn read_source(path: &Path) -> String {
    fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

#[cfg(unix)]
pub(crate) struct StartedRuntime {
    pub(crate) runtime: Arc<MeshRuntime>,
    pub(crate) session: Session,
    pub(crate) relay_handle: JoinHandle<()>,
    pub(crate) tmp: TempDir,
}

#[cfg(unix)]
impl StartedRuntime {
    /// A fresh node on the same identity, cache and config dirs, joined to `port`:
    /// what `.mesh on` after `.mesh off` builds. The caller has stopped the old node
    /// (through its slot, which is what writes the peer table); the new one starts
    /// from that table with a transport that holds no route yet.
    pub(crate) async fn restart_on(&mut self, port: u16) {
        self.runtime = MeshRuntime::start(
            &private_config(port),
            true,
            &mut self.session,
            mesh_paths(&self.tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
    }

    /// Whether the node's transport has learned `destination`'s identity from an
    /// announce, which is what a request to it needs and what a peer table row alone
    /// does not give.
    pub(crate) async fn knows_identity_of(&self, destination: &str) -> bool {
        let hash = AddressHash::new_from_hex_string(destination).unwrap();
        self.runtime
            .transport_handle()
            .await
            .expect("the node is running")
            .destination_identity(&hash)
            .await
            .is_some()
    }

    /// Every announce and path response the node's transport accepts from now on.
    pub(crate) async fn announces(&self) -> broadcast::Receiver<AnnounceEvent> {
        self.runtime
            .transport_handle()
            .await
            .expect("the node is running")
            .recv_announces()
            .await
    }
}

/// A runtime joined to a loopback relay, with its identity and cache under a temp dir.
#[cfg(unix)]
pub(crate) async fn started_runtime(tag: &str) -> StartedRuntime {
    started_runtime_with(tag, |_| {}).await
}

/// `started_runtime` with the node's config adjusted first, for a test whose node
/// must stage or protect a directory of the test's choosing.
#[cfg(unix)]
pub(crate) async fn started_runtime_with(
    tag: &str,
    adjust: impl FnOnce(&mut MeshConfig),
) -> StartedRuntime {
    started_runtime_with_options(tag, NodeOptions::default(), adjust).await
}

/// `started_runtime_with` starting the node with `options` instead of the defaults.
#[cfg(unix)]
pub(crate) async fn started_runtime_with_options(
    tag: &str,
    options: NodeOptions,
    adjust: impl FnOnce(&mut MeshConfig),
) -> StartedRuntime {
    let (addr, relay_handle, _) = loopback_relay().await;
    let mut config = private_config(addr.port());
    adjust(&mut config);
    started_runtime_at(tag, config, options, relay_handle).await
}

/// A runtime joined over a loopback relay it is about to lose: install it in a slot
/// (which runs its interface watcher), then hand the relay to `leave_relay`.
#[cfg(unix)]
pub(crate) async fn started_runtime_on_doomed_relay(tag: &str) -> (StartedRuntime, TransportRelay) {
    let relay = TransportRelay::start_on_with_retries(closed_port().await, 0).await;
    let started = started_runtime_on(tag, relay.port).await;
    (started, relay)
}

/// Stops `relay` and marks `runtime`'s interface rows unreachable, so it sits on no
/// relay; a drop alone leaves a TCP row connected until its redials fail.
#[cfg(unix)]
pub(crate) async fn leave_relay(relay: TransportRelay, runtime: &MeshRuntime) {
    relay.stop().await;
    runtime.mark_interfaces_unreachable();
}

/// A runtime joined to whatever listens on `port`, such as a `PeerStub`. There is no
/// relay task, so `relay_handle` is a task already finished and aborting it does
/// nothing.
#[cfg(unix)]
pub(crate) async fn started_runtime_on(tag: &str, port: u16) -> StartedRuntime {
    started_runtime_on_with(tag, port, |_| {}).await
}

/// `started_runtime_on` with the node's config adjusted first.
#[cfg(unix)]
pub(crate) async fn started_runtime_on_with(
    tag: &str,
    port: u16,
    adjust: impl FnOnce(&mut MeshConfig),
) -> StartedRuntime {
    let mut config = private_config(port);
    adjust(&mut config);
    started_runtime_at(
        tag,
        config,
        NodeOptions::default(),
        tokio::spawn(std::future::ready(())),
    )
    .await
}

#[cfg(unix)]
async fn started_runtime_at(
    tag: &str,
    config: MeshConfig,
    options: NodeOptions,
    relay_handle: JoinHandle<()>,
) -> StartedRuntime {
    started_runtime_as(tag, config, options, relay_handle, None).await
}

/// `started_runtime_on` for another session of `sibling`'s user: the node starts on
/// a copy of `sibling`'s identity key, so it announces a destination of its own under
/// the same identity.
#[cfg(unix)]
pub(crate) async fn started_session_of(
    tag: &str,
    port: u16,
    sibling: &StartedRuntime,
    adjust: impl FnOnce(&mut MeshConfig),
) -> StartedRuntime {
    let key = mesh_paths(&sibling.tmp).identity_path;
    let mut config = private_config(port);
    adjust(&mut config);
    let relay_handle = tokio::spawn(std::future::ready(()));
    started_runtime_as(
        tag,
        config,
        NodeOptions::default(),
        relay_handle,
        Some(&key),
    )
    .await
}

#[cfg(unix)]
async fn started_runtime_as(
    tag: &str,
    config: MeshConfig,
    options: NodeOptions,
    relay_handle: JoinHandle<()>,
    identity_from: Option<&std::path::Path>,
) -> StartedRuntime {
    let tmp = TempDir::new(tag);
    if let Some(key) = identity_from {
        let to = mesh_paths(&tmp).identity_path;
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(key, &to).unwrap();
    }
    let mut session = Session::default();
    let runtime = MeshRuntime::start(&config, true, &mut session, mesh_paths(&tmp), options)
        .await
        .unwrap();
    StartedRuntime {
        runtime,
        session,
        relay_handle,
        tmp,
    }
}
