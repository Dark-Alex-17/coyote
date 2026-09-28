use crate::config::mesh_config::{MeshConfig, MeshInterface};
use crate::config::{ForkRekey, Session, paths};
use crate::mesh::announce::{
    AnnounceAppData, HEARTBEAT_SECS, REANNOUNCE_FLOOR_SECS, announce_app_data,
};
use crate::mesh::brief::{Brief, Digest, assemble_brief, digest_objective_for};
use crate::mesh::card::{CardSource, DISPLAY_NAME_MAX_CHARS, StatusHandler, build_card};
use crate::mesh::envoy::{EnvoyJob, EnvoySink};
use crate::mesh::events::{
    BriefUpdateSource, MeshEvent, MeshHookSink, MeshHooks, NodeFacts, Routed, TrustHookObserver,
};
use crate::mesh::idle::{IdleNotify, IdleSink, Origin};
use crate::mesh::knock::{
    ChannelKnockSink, KNOCK_LINK_TIMEOUT, KNOCK_QUEUE_CAPACITY, KNOCK_REQUEST_TIMEOUT, KnockError,
    KnockGate, KnockIntro, KnockOutcome, KnockRouting, KnockSurface, KnockVia, drain_knocks,
    knock_message,
};
use crate::mesh::knocks::KnockCache;
use crate::mesh::limits::{FoldNotice, PeerLimitConfig, PeerLimits, PeerRefusal, RefusalReason};
use crate::mesh::lock::InstanceLock;
use crate::mesh::message::{
    CHECK_INBOX_NEXT_ACTION, ModelNotes, OutboundPeer, PEER_LINE_MAX_CHARS, PeerAdmission,
    PeerInbox, PeerKind, PeerMessage, PeerMessageHandler, PeerRouting, PeerSurface, PeerVia,
    RawPeerMessage, collect_next_action, unix_now,
};
use crate::mesh::notify::{Notification, NotificationSink, Source};
use crate::mesh::peers::{PEER_TABLE_MAX_ENTRIES, PeerChange, PeerSighting, PeerTable};
use crate::mesh::pending::{
    Correlations, InboundRecord, InboundStore, PendingRecord, PendingStore,
};
use crate::mesh::propagation::{
    self, OutboundMessage, PropagationError, PropagationNode, PropagationOptions,
};
use crate::mesh::propagation_fetch::{self, FetchError, FetchOptions, FetchReport, InboundSink};
use crate::mesh::propagation_nodes::PropagationNodeTable;
use crate::mesh::protocol::{Compatibility, MESH_PROTOCOL_MIN_SUPPORTED, MESH_PROTOCOL_VERSION};
#[cfg(test)]
use crate::mesh::r3::RequestHandler;
use crate::mesh::r3::{
    Dispatcher, Envelope, KNOCK_PATH, MESSAGE_PATH, OriginName, R3Client, R3Error, R3Server,
    RefusalCode, RequestOptions, RequestOutcome, RequestReceipt, STATUS_PATH, short,
};
use crate::mesh::snapshot::MeshSnapshot;
use crate::mesh::trust::TrustStore;
use crate::mesh::{display_text, hex_lower, identity, mesh_cache_dir};
use crate::supervisor::notification::{SystemNotification, mesh_notification};

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwapOption;
use parking_lot::RwLock;
use rand_core::OsRng;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::{DestinationName, SingleInputDestination};
use rns_transport::hash::AddressHash;
use rns_transport::identity::Identity;
use rns_transport::identity::PrivateIdentity as TransportIdentity;
use rns_transport::identity_bridge::{to_core_private_identity, to_transport_private_identity};
use rns_transport::iface::auto::{AutoInterfaceConfig, AutoInterfaceDeviceFilter};
use rns_transport::iface::auto_runtime::{
    AutoDiscoveryRuntime, AutoInterfaceTransportRuntime, AutoRuntimePlan,
};
use rns_transport::iface::tcp_client::TcpClient;
use rns_transport::iface::{IfaceRole, InterfaceMode};
use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

/// One grace window on the transport. `stop` spends at most one on registered tasks and one
/// on transport teardown, plus any `REKEY_GRACE` a concurrent rekey is spending; `start`
/// spends one on registering and first announcing the destination, and `abandon_start` and
/// the `join_lan`/`join_tcp` failure cleanup spend one unwinding. The r3 tests bound their
/// own teardown with it too.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long `rekey` and `announce_now` wait on the transport for each of their steps. The
/// transport takes its handler lock for every one, and a peer that trips its resource-reject
/// path can leave that lock held for good.
pub(crate) const REKEY_GRACE: Duration = Duration::from_secs(5);
/// Poll interval while waiting for a TCP relay to report itself connected.
const CONNECT_POLL: Duration = Duration::from_millis(50);
/// Depth of the host channel a LAN interface feeds the transport through.
const LAN_CHANNEL_CAPACITY: usize = 128;
/// How often the peer table is written back if it changed; `stop` writes it regardless.
const PEER_PERSIST_INTERVAL_SECS: u64 = 30;
/// Refusal shared by `MeshSlot::install` and the `.mesh on` pre-check.
pub(crate) const MESH_ALREADY_ON: &str = "Mesh is already on in this process. Run `.mesh off` first, then `.mesh on` to start it again with the current settings.";

/// Where a node keeps its identity, the user's trust list and its disposable state.
pub(crate) struct MeshPaths {
    pub identity_path: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl MeshPaths {
    pub(crate) fn from_env() -> Self {
        Self {
            identity_path: identity::identity_path(),
            cache_dir: paths::cache_dir(),
            config_dir: paths::config_dir(),
        }
    }
}

pub(crate) struct NodeOptions {
    pub connect_timeout: Duration,
    /// The handle the node fires its `mesh.*` events through; the slot that installs
    /// the node shares it, so pass `MeshSlot::hooks`.
    pub hooks: MeshHooks,
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            connect_timeout: TcpClient::DEFAULT_CONNECT_TIMEOUT,
            hooks: MeshHooks::default(),
        }
    }
}

/// Timeouts for one knock: the direct attempt, then the fallback post.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KnockOptions {
    pub request: RequestOptions,
    pub propagation: PropagationOptions,
}

impl Default for KnockOptions {
    fn default() -> Self {
        Self {
            request: RequestOptions {
                request_timeout: KNOCK_REQUEST_TIMEOUT,
                link_timeout: KNOCK_LINK_TIMEOUT,
            },
            propagation: PropagationOptions::default(),
        }
    }
}

/// One configured interface, resolved to what the transport will be asked to join.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InterfacePlan {
    Lan,
    Tcp {
        kind: &'static str,
        endpoint: String,
    },
}

impl InterfacePlan {
    fn label(&self) -> String {
        match self {
            Self::Lan => "lan".to_string(),
            Self::Tcp { kind, endpoint } => format!("{kind} {endpoint}"),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Tcp { kind, .. } => kind,
        }
    }
}

/// Maps the configured list 1:1; nothing is detected, defaulted, or substituted.
fn plan_interfaces(interfaces: &[MeshInterface]) -> Vec<InterfacePlan> {
    interfaces
        .iter()
        .map(|interface| match interface {
            MeshInterface::Lan => InterfacePlan::Lan,
            MeshInterface::Private { host, port } => InterfacePlan::Tcp {
                kind: "private",
                endpoint: format!("{host}:{port}"),
            },
            MeshInterface::Public { host, port } => InterfacePlan::Tcp {
                kind: "public",
                endpoint: format!("{host}:{port}"),
            },
        })
        .collect()
}

enum JoinedInterface {
    Lan {
        host_iface: AddressHash,
        runtime: AutoDiscoveryRuntime,
    },
    Tcp {
        hash: AddressHash,
        handle: JoinHandle<()>,
        label: String,
    },
}

struct DestinationState {
    dest: Arc<Mutex<SingleInputDestination>>,
    hash: AddressHash,
    /// The name hash requests claim as their origin; moves with `dest` on a rekey.
    origin: OriginName,
    instance_id: String,
    /// `None` only once the runtime has stopped and released it.
    lock: Option<InstanceLock>,
    last_announce: Option<Instant>,
}

/// The instance and destination the node speaks for, readable without waiting on the
/// `destination` mutex: the peer surface and the pending store are asked for them on
/// paths that cannot `.await`.
struct CurrentIds {
    instance_id: String,
    destination_hash: String,
}

/// One in-process Reticulum node: a transport over the configured interfaces, one destination
/// derived from the session's mesh instance id, and the tasks that announce it and track peers.
pub(crate) struct MeshRuntime {
    fingerprint: String,
    transport_identity: TransportIdentity,
    app_data: Vec<u8>,
    announce: bool,
    display_name: Option<String>,
    peer_limits: PeerLimitConfig,
    cache_dir: PathBuf,
    interface_labels: Vec<String>,
    interface_kinds: Vec<&'static str>,
    hooks: MeshHooks,
    /// `None` once `shutdown` has released this owner. Requests and the server loop hold
    /// clones, so the upstream `Drop` that cancels the transport's tasks runs when the last
    /// of those finishes, not when this lock is emptied.
    transport: Mutex<Option<Arc<Transport>>>,
    interfaces: Mutex<Vec<JoinedInterface>>,
    destination: Mutex<DestinationState>,
    ids: RwLock<CurrentIds>,
    peers: Arc<PeerTable>,
    propagation_nodes: Arc<PropagationNodeTable>,
    trust: Arc<TrustStore>,
    r3_client: Arc<R3Client>,
    r3_server: Arc<R3Server>,
    dispatcher: Arc<Dispatcher>,
    knock_gate: Arc<KnockGate>,
    knock_sink: Arc<ChannelKnockSink>,
    /// Where fetched peer messages go; attached by the slot the node is installed into.
    peer_surface: parking_lot::Mutex<Option<Weak<dyn PeerSurface>>>,
    /// Held for the length of one propagation fetch; a second caller is refused, never
    /// queued behind the first.
    fetching: Mutex<()>,
    /// Held across one knock's or message's propagation-node post. `propagate` needs calls
    /// for the same node serialised, so a second poster waits here rather than sharing
    /// the out-link.
    posting: Mutex<()>,
    cancel: CancellationToken,
    tasks: parking_lot::Mutex<Vec<JoinHandle<()>>>,
}

impl MeshRuntime {
    /// Brings a node up for `session` and returns it running. Validation comes first so a bad
    /// config touches nothing on disk; the instance lock is taken before the identity is minted
    /// so a refused start never creates a key. A trust list that does not load refuses the
    /// start outright: the node never serves against a partial list.
    pub(crate) async fn start(
        config: &MeshConfig,
        function_calling_support: bool,
        session: &mut Session,
        paths: MeshPaths,
        options: NodeOptions,
    ) -> Result<Arc<Self>> {
        // `validate` is a no-op while `enabled` is false, and the caller is about to turn the
        // mesh on, so the checks have to run against the enabled view of the config.
        let mut enabled_view = config.clone();
        enabled_view.enabled = true;
        enabled_view.validate(function_calling_support)?;
        let plans = plan_interfaces(config.interfaces());
        let app_data = announce_app_data(config)?;
        let trust = Arc::new(TrustStore::open(&paths.config_dir)?);
        trust.set_observer(Arc::new(TrustHookObserver(options.hooks.clone())));

        let instance_id = session.ensure_mesh_instance_id().to_string();
        let lock = InstanceLock::acquire(&paths.cache_dir, &instance_id)?;
        let core_identity = identity::load_or_mint_identity(&paths.identity_path)?;
        let fingerprint = identity::fingerprint(&core_identity);
        let transport_identity = to_transport_private_identity(&core_identity);
        let peers = Arc::new(PeerTable::load(
            mesh_cache_dir(&paths.cache_dir).join("peers.json"),
            SystemTime::now(),
        )?);

        let transport = Transport::new(TransportConfig::new("coyote", &transport_identity, false));
        // Every stream is subscribed before an interface is joined so nothing is missed.
        let announces = transport.recv_announces().await;
        let out_link_events = transport.out_link_events();
        let in_link_events = transport.in_link_events();
        let client_resource_events = transport.resource_events();
        let server_resource_events = transport.resource_events();
        let mut joined = Vec::with_capacity(plans.len());
        for plan in &plans {
            match join_interface(&transport, plan, &options).await {
                Ok(iface) => joined.push(iface),
                Err(err) => {
                    abandon_start(transport, joined).await;
                    return Err(err);
                }
            }
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        let (dest, hash, origin) = match register_destination(
            &transport,
            &transport_identity,
            &instance_id,
            &app_data,
            deadline,
        )
        .await
        {
            Ok(registered) => registered,
            Err(err) => {
                abandon_start(transport, joined).await;
                return Err(err.context(format!(
                    "The mesh node's destination could not be registered within {}s",
                    SHUTDOWN_GRACE.as_secs()
                )));
            }
        };
        let mut last_announce = None;
        if config.announce {
            match timeout_at(
                deadline,
                announce_destination(&transport, &dest, &hash, &app_data),
            )
            .await
            {
                Ok(Ok(sent_at)) => last_announce = Some(sent_at),
                Ok(Err(err)) => {
                    abandon_start(transport, joined).await;
                    return Err(err);
                }
                Err(_) => {
                    abandon_start(transport, joined).await;
                    bail!(
                        "The mesh node's first announce for destination {} was not sent within {}s",
                        hash.to_hex_string(),
                        SHUTDOWN_GRACE.as_secs()
                    );
                }
            }
        }
        debug!(
            "Started mesh node {fingerprint} (instance {instance_id}) as destination {}",
            hash.to_hex_string()
        );

        // Nothing fallible may follow: a failure once the tasks exist would leak them.
        let transport = Arc::new(transport);
        let r3_server = Arc::new(R3Server::new());
        let (knock_sink, knock_rx) = ChannelKnockSink::new(KNOCK_QUEUE_CAPACITY);
        let dispatcher = Arc::new(Dispatcher::new(trust.clone(), knock_sink.clone()));
        r3_server.set_handler(dispatcher.clone());
        let knock_gate = Arc::new(KnockGate::new(
            trust.clone(),
            peers.clone(),
            KnockCache::new(&paths.cache_dir, config.knock_retention_hours),
            options.hooks.clone(),
        ));
        let runtime = Arc::new(Self {
            fingerprint,
            transport_identity,
            app_data,
            announce: config.announce,
            display_name: config.display_name.clone(),
            peer_limits: PeerLimitConfig::from(config),
            cache_dir: paths.cache_dir,
            interface_labels: plans.iter().map(InterfacePlan::label).collect(),
            interface_kinds: plans.iter().map(InterfacePlan::kind).collect(),
            hooks: options.hooks,
            transport: Mutex::new(Some(transport.clone())),
            interfaces: Mutex::new(joined),
            destination: Mutex::new(DestinationState {
                dest,
                hash,
                origin,
                instance_id: instance_id.clone(),
                lock: Some(lock),
                last_announce,
            }),
            ids: RwLock::new(CurrentIds {
                instance_id,
                destination_hash: hash.to_hex_string(),
            }),
            peers,
            propagation_nodes: Arc::new(PropagationNodeTable::new()),
            trust,
            r3_client: Arc::new(R3Client::new()),
            r3_server,
            dispatcher,
            knock_gate,
            knock_sink,
            peer_surface: parking_lot::Mutex::new(None),
            fetching: Mutex::new(()),
            posting: Mutex::new(()),
            cancel: CancellationToken::new(),
            tasks: parking_lot::Mutex::new(Vec::new()),
        });
        runtime.register_task(tokio::spawn(runtime.r3_client.clone().run(
            out_link_events,
            client_resource_events,
            runtime.cancellation_token(),
        )));
        runtime.register_task(tokio::spawn(runtime.r3_server.clone().run(
            transport,
            in_link_events,
            server_resource_events,
            runtime.cancellation_token(),
        )));
        runtime.register_task(tokio::spawn(receive_announces(
            announces,
            runtime.peers.clone(),
            runtime.trust.clone(),
            runtime.propagation_nodes.clone(),
            runtime.hooks.clone(),
            runtime.cancellation_token(),
        )));
        runtime.register_task(tokio::spawn(drain_knocks(
            knock_rx,
            runtime.knock_gate.clone(),
            runtime.cancellation_token(),
        )));
        runtime.register_task(tokio::spawn(sweep_peers(
            runtime.peers.clone(),
            runtime.cancellation_token(),
        )));
        runtime.register_task(tokio::spawn(persist_peers_periodically(
            runtime.peers.clone(),
            runtime.cancellation_token(),
        )));
        if config.announce {
            runtime.register_task(tokio::spawn(announce_periodically(
                runtime.clone(),
                runtime.cancellation_token(),
            )));
        }
        Ok(runtime)
    }

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn instance_id(&self) -> String {
        self.destination.lock().await.instance_id.clone()
    }

    /// `instance_id` without the wait, from the cache `start` fills and `rekey` moves.
    pub(crate) fn current_instance_id(&self) -> String {
        self.ids.read().instance_id.clone()
    }

    /// `destination_hash` without the wait, from the cache `start` fills and `rekey` moves.
    pub(crate) fn current_destination_hash(&self) -> String {
        self.ids.read().destination_hash.clone()
    }

    pub(crate) fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The configured display name, as the status card carries it to trusted peers.
    /// `display_name_on_public` gates announces only; the card is exempt because it
    /// reaches trusted destinations and nobody else.
    pub(crate) fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// The per-peer ceilings the node was started with, for the slot that installs it.
    pub(crate) fn peer_limits(&self) -> PeerLimitConfig {
        self.peer_limits
    }

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn destination_hash(&self) -> String {
        self.destination.lock().await.hash.to_hex_string()
    }

    /// Human labels of the joined interfaces, in config order.
    pub(crate) fn interfaces(&self) -> Vec<String> {
        self.interface_labels.clone()
    }

    /// The joined interfaces by kind only (`lan`, `private`, `public`), in config order.
    pub(crate) fn interface_kinds(&self) -> Vec<&'static str> {
        self.interface_kinds.clone()
    }

    pub(crate) fn hooks(&self) -> &MeshHooks {
        &self.hooks
    }

    pub(crate) fn peers(&self) -> Arc<PeerTable> {
        self.peers.clone()
    }

    /// The LXMF propagation nodes heard so far, as `fetch_propagated` chooses among them.
    pub(crate) fn propagation_nodes(&self) -> Arc<PropagationNodeTable> {
        self.propagation_nodes.clone()
    }

    pub(crate) fn trust(&self) -> Arc<TrustStore> {
        self.trust.clone()
    }

    /// The gate every inbound request passes; providers register their paths on it.
    /// `register` returns the provider it displaced, a placeholder counting as nothing
    /// displaced; `/knock` is owned by the dispatcher itself and registering it is refused.
    pub(crate) fn dispatcher(&self) -> Arc<Dispatcher> {
        self.dispatcher.clone()
    }

    /// Where every knock lands, from a link or from a propagation node.
    pub(crate) fn knock_gate(&self) -> Arc<KnockGate> {
        self.knock_gate.clone()
    }

    /// Where peer messages fetched from a propagation node land. Held weakly: the slot
    /// owns the runtime.
    pub(crate) fn attach_peer_surface(&self, surface: Weak<dyn PeerSurface>) {
        *self.peer_surface.lock() = Some(surface);
    }

    /// Knocks the dispatcher had to drop because the gate's queue was full.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn knock_overflow(&self) -> u64 {
        self.knock_sink.overflow()
    }

    #[cfg(test)]
    pub(crate) async fn has_destination(&self, hex: &str) -> bool {
        let hash = AddressHash::new_from_hex_string(hex).unwrap();
        match self.transport.lock().await.as_ref() {
            Some(transport) => transport.has_destination(&hash).await,
            None => false,
        }
    }

    #[cfg(test)]
    pub(crate) async fn max_request_size(&self) -> Option<usize> {
        let state = self.destination.lock().await;
        state.dest.lock().await.max_request_size()
    }

    /// The node's private key as hex, for tests that prove no hook env carries it.
    #[cfg(test)]
    pub(crate) fn private_key_hex(&self) -> String {
        to_core_private_identity(&self.transport_identity).to_hex_string()
    }

    /// Arms the upstream advertisement-time request cap, which production code leaves off
    /// because rejecting on it deadlocks the transport (rev 3ed5932). Tests use it to wedge a
    /// node on purpose and prove the node's waits stay bounded.
    #[cfg(test)]
    pub(crate) async fn arm_request_cap_for_test(&self, cap: usize) {
        let state = self.destination.lock().await;
        state
            .dest
            .lock()
            .await
            .set_max_request_size(cap)
            .expect("the upstream cap setter is infallible");
    }

    /// A token cancelled when the node stops; every task the node owns should watch it.
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancel.child_token()
    }

    /// Adopts a task so `stop` waits for it to finish.
    pub(crate) fn register_task(&self, handle: JoinHandle<()>) {
        self.tasks.lock().push(handle);
    }

    /// A handle on the running transport, `None` once the node has stopped. Taken so the
    /// `transport` guard is not held across the caller's waits.
    pub(super) async fn transport_handle(&self) -> Option<Arc<Transport>> {
        self.transport.lock().await.clone()
    }

    /// Hands `build(origin)` to `node` for `recipient`, queued behind `posting` since
    /// `propagate` needs one caller per node at a time. `build` gets this node's current
    /// origin so the stored message names the instance sending it.
    pub(super) async fn post_to_node(
        &self,
        recipient: &Identity,
        node: &PropagationNode,
        build: impl FnOnce(&OriginName) -> OutboundMessage,
        options: &PropagationOptions,
    ) -> Result<(), PropagationError> {
        let _posting = self.posting.lock().await;
        let transport = self
            .transport_handle()
            .await
            .ok_or(PropagationError::Link(R3Error::NotRunning))?;
        let sender = to_core_private_identity(&self.transport_identity);
        let origin = self.destination.lock().await.origin;
        propagation::propagate(
            &transport,
            &sender,
            recipient,
            node,
            &build(&origin),
            self.cancellation_token(),
            options,
        )
        .await?;
        Ok(())
    }

    /// Sends one request to `destination` over a link, proving this node's identity first
    /// and naming its current instance as the origin.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn request(
        &self,
        destination: &DestinationDesc,
        path: &str,
        data: rmpv::Value,
        options: RequestOptions,
    ) -> Result<RequestOutcome, R3Error> {
        let envelope = self.envelope(data).await;
        self.request_envelope(destination, path, envelope, options)
            .await
    }

    /// `request` with the envelope already built. Every request to a peer passes here or
    /// through `request_with_receipt`: a peer the table knows speaks another protocol is
    /// refused before any link is opened, and a peer that refuses this node's version over
    /// the wire is marked so the next request is refused here.
    async fn request_envelope(
        &self,
        destination: &DestinationDesc,
        path: &str,
        envelope: Envelope,
        options: RequestOptions,
    ) -> Result<RequestOutcome, R3Error> {
        let dest_hex = destination.address_hash.to_hex_string();
        refuse_incompatible_peer(&self.peers, &dest_hex, path)?;
        let transport = self
            .transport
            .lock()
            .await
            .clone()
            .ok_or(R3Error::NotRunning)?;
        let sent = envelope.version;
        let request = self.r3_client.request(
            &transport,
            &self.transport_identity,
            destination,
            path,
            envelope,
            options,
        );
        let outcome = tokio::select! {
            () = self.cancel.cancelled() => Err(R3Error::Shutdown),
            outcome = request => outcome,
        };
        note_version_refusal(&self.peers, &dest_hex, path, sent, &outcome);
        outcome
    }

    /// `request` as a receipt: returns as soon as the request is on its own task and reports
    /// its progress from there. Stopping the node settles every receipt still in flight,
    /// link opening included, as `Failed(Shutdown)`.
    // Reached by the REPL mesh commands and the message tools once they land.
    #[allow(dead_code)]
    pub(crate) async fn request_with_receipt(
        &self,
        destination: &DestinationDesc,
        path: &str,
        data: rmpv::Value,
        options: RequestOptions,
    ) -> Result<RequestReceipt, R3Error> {
        let dest_hex = destination.address_hash.to_hex_string();
        refuse_incompatible_peer(&self.peers, &dest_hex, path)?;
        let transport = self
            .transport
            .lock()
            .await
            .clone()
            .ok_or(R3Error::NotRunning)?;
        let envelope = self.envelope(data).await;
        let sent = envelope.version;
        let client = self.r3_client.clone();
        let identity = self.transport_identity.clone();
        let peers = self.peers.clone();
        let (destination, path) = (*destination, path.to_string());
        Ok(RequestReceipt::track(
            self.cancellation_token(),
            move |delivered| async move {
                let outcome = client
                    .request_with(
                        &transport,
                        &identity,
                        &destination,
                        &path,
                        envelope,
                        options,
                        Some(delivered),
                    )
                    .await;
                note_version_refusal(&peers, &dest_hex, &path, sent, &outcome);
                outcome
            },
        ))
    }

    /// `body` in the envelope naming the instance this node speaks for right now, read per
    /// request so a rekeyed node claims its new instance and never a cached one.
    async fn envelope(&self, body: rmpv::Value) -> Envelope {
        Envelope::new(self.destination.lock().await.origin, body)
    }

    /// Asks `destination` to trust this node's current instance, with `intro` as the
    /// words. The link is tried first; a peer that cannot be reached gets the knock held
    /// by a propagation node until it next fetches.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn knock(
        &self,
        destination: &DestinationDesc,
        intro: &KnockIntro,
    ) -> Result<KnockOutcome, KnockError> {
        self.knock_with(destination, intro, KnockOptions::default())
            .await
    }

    /// `knock` with its timeouts chosen. The dispatcher answers a knock with `NoAccess` by
    /// design, so that refusal (or an answer) means the knock landed. Any other refusal
    /// code did not file the knock and is not unreachability, so it is reported as-is.
    /// So is a peer that knows this identity and refuses this node's protocol version: it
    /// heard the knock and could not read it, and a stored one would meet the same peer.
    /// A peer that does not know it stays silent and the knock falls back to
    /// store-and-forward as before.
    /// Only the peer-unreachable errors fall back to store-and-forward; an oversize or
    /// undecodable frame is this node's fault and storing it would not help. Posts to the
    /// propagation node queue behind `posting`, since `propagate` needs one caller per
    /// node at a time.
    pub(crate) async fn knock_with(
        &self,
        destination: &DestinationDesc,
        intro: &KnockIntro,
        options: KnockOptions,
    ) -> Result<KnockOutcome, KnockError> {
        let dest_hex = destination.address_hash.to_hex_string();
        let dest8 = short(&dest_hex);
        let unreachable = match self
            .request(destination, KNOCK_PATH, intro.to_r3_body(), options.request)
            .await
        {
            Ok(_) | Err(R3Error::Refused(RefusalCode::NoAccess)) => {
                return Ok(KnockOutcome {
                    via: KnockVia::Direct,
                });
            }
            Err(err @ (R3Error::Timeout { .. } | R3Error::LinkFailed(_) | R3Error::LinkClosed)) => {
                err
            }
            Err(R3Error::NotRunning | R3Error::Shutdown) => return Err(KnockError::NotRunning),
            Err(err @ R3Error::UnsupportedVersion { .. }) => return Err(KnockError::Direct(err)),
            Err(err) => {
                debug!("Mesh knock to {dest8} was not filed over the link: {err}");
                return Err(KnockError::Direct(err));
            }
        };
        // Selected before queueing behind another post: a knocker with no node to fall
        // back on is told so at once rather than after someone else's transfer.
        let node = self
            .propagation_nodes
            .select_for_posting()
            .map_err(|_| KnockError::NoPropagationNode)?;
        let node_hex = node.destination.address_hash.to_hex_string();
        debug!(
            "Mesh knock to {dest8} could not be delivered over a link ({unreachable}); storing it with propagation node {}",
            short(&node_hex)
        );
        self.post_to_node(
            &destination.identity,
            &node,
            |origin| knock_message(intro, origin),
            &options.propagation,
        )
        .await
        .map_err(|err| match err {
            PropagationError::Cancelled
            | PropagationError::Link(R3Error::Shutdown | R3Error::NotRunning) => {
                KnockError::NotRunning
            }
            other => KnockError::Propagation(other),
        })?;
        Ok(KnockOutcome {
            via: KnockVia::StoreAndForward,
        })
    }

    /// Fetches the messages the nearest announced propagation node holds for this node,
    /// handing the ones that pass every check to `sink`, knocks and peer messages
    /// excepted: those go to the knock gate and the peer surface and never reach `sink`.
    /// The dedup store is read from disk
    /// for each fetch and written back before the node is told to delete anything, so a
    /// message survives a restart between the two as a remembered id rather than a second
    /// delivery. One fetch runs at a time across every Coyote process of this identity;
    /// stopping the node cancels it.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn fetch_propagated(
        &self,
        sink: &dyn InboundSink,
    ) -> Result<FetchReport, FetchError> {
        let Ok(_fetching) = self.fetching.try_lock() else {
            return Err(FetchError::AlreadyRunning);
        };
        let store_path = mesh_cache_dir(&self.cache_dir).join("propagation.json");
        let _lock = propagation_fetch::FetchLock::acquire(&store_path)?;
        let transport = self
            .transport
            .lock()
            .await
            .clone()
            .ok_or(FetchError::Link(R3Error::NotRunning))?;
        let node = self.propagation_nodes.select()?;
        let mut store = propagation_fetch::FetchStore::load(store_path, SystemTime::now())?;
        let peers = PeerRouting {
            trust: &self.trust,
            surface: self.peer_surface.lock().as_ref().and_then(Weak::upgrade),
            inner: sink,
        };
        let routing = KnockRouting {
            gate: &self.knock_gate,
            inner: &peers,
        };
        propagation_fetch::fetch(
            &transport,
            &self.r3_client,
            &self.transport_identity,
            &node,
            &self.peers,
            &self.trust,
            &mut store,
            &routing,
            &FetchOptions::default(),
            self.cancellation_token(),
        )
        .await
    }

    /// Replaces the dispatcher `start` installed, so a test can watch requests directly.
    #[cfg(test)]
    pub(crate) fn set_request_handler(&self, handler: Arc<dyn RequestHandler>) {
        self.r3_server.set_handler(handler);
    }

    /// Announces the current destination unless it was announced within
    /// `REANNOUNCE_FLOOR_SECS`, in which case nothing is sent and `Ok(false)` is returned.
    pub(crate) async fn announce_now(&self) -> Result<bool> {
        let mut state = self.destination.lock().await;
        if state
            .last_announce
            .is_some_and(|at| at.elapsed() < Duration::from_secs(REANNOUNCE_FLOOR_SECS))
        {
            return Ok(false);
        }
        let transport = self.running_transport().await?;
        timeout(REKEY_GRACE, self.send_announce(&mut state, &transport))
            .await
            .map_err(|_| {
                anyhow!(
                    "The mesh transport did not send the announce for destination {} within {}s; run `.mesh off` and then `.mesh on` to restart the node",
                    state.hash.to_hex_string(),
                    REKEY_GRACE.as_secs()
                )
            })??;
        Ok(true)
    }

    /// A handle on the transport, taken so the `transport` guard is not held across the
    /// caller's waits and a concurrent `request` or `shutdown` does not queue behind them.
    async fn running_transport(&self) -> Result<Arc<Transport>> {
        self.transport
            .lock()
            .await
            .clone()
            .context("The mesh node has been stopped; run `.mesh on` to start it again")
    }

    async fn send_announce(
        &self,
        state: &mut DestinationState,
        transport: &Transport,
    ) -> Result<()> {
        state.last_announce =
            Some(announce_destination(transport, &state.dest, &state.hash, &self.app_data).await?);
        Ok(())
    }

    /// Moves the live destination from the original session to its fork. An `Err` always
    /// leaves the original destination, instance id and lock in place and served: every
    /// fallible step (instance-id check, stopped-transport check, fork lock acquisition,
    /// registering the fork's destination) happens before the swap, and the fork's
    /// destination is registered before the original is released so a transport that stalls
    /// on the registration leaves the node serving exactly what it served before. The two
    /// hashes differ (`mesh.{instance_id}`), so both may be registered at once; a release of
    /// the original that does not finish within `REKEY_GRACE` is logged and the swap goes
    /// ahead, leaving the original registered until the node stops. Each transport wait is
    /// bounded because a wedged handler lock would otherwise hold `destination` for good.
    pub(crate) async fn rekey(&self, rekey: ForkRekey) -> Result<()> {
        let mut state = self.destination.lock().await;
        if rekey.original_instance_id.as_deref() != Some(state.instance_id.as_str()) {
            bail!(
                "The mesh node is serving instance {} rather than the forked session's instance {}, so the fork cannot take over its destination. Run `.mesh off` and then `.mesh on` in the fork to join the mesh with the fork's own destination.",
                state.instance_id,
                rekey.original_instance_id.as_deref().unwrap_or("(none)")
            );
        }
        let transport = self.running_transport().await?;
        let lock = InstanceLock::acquire(&self.cache_dir, &rekey.fork_instance_id)?;

        let deadline = tokio::time::Instant::now() + REKEY_GRACE;
        let (dest, hash, origin) = register_destination(
            &transport,
            &self.transport_identity,
            &rekey.fork_instance_id,
            &self.app_data,
            deadline,
        )
        .await
        .with_context(|| {
            format!(
                "The fork's destination could not be registered within {}s, so the node still serves destination {}. Run `.mesh off` and then `.mesh on` in the fork to join the mesh with the fork's own destination.",
                REKEY_GRACE.as_secs(),
                state.hash.to_hex_string(),
            )
        })?;
        if timeout_at(deadline, transport.deregister_destination(&state.hash))
            .await
            .is_err()
        {
            warn!(
                "The mesh transport did not release destination {} within {}s; it stays registered until the node stops",
                state.hash.to_hex_string(),
                REKEY_GRACE.as_secs()
            );
        }
        debug!(
            "Re-keyed mesh node {} from instance {} to fork instance {} (destination {})",
            self.fingerprint,
            state.instance_id,
            rekey.fork_instance_id,
            hash.to_hex_string()
        );
        *state = DestinationState {
            dest,
            hash,
            origin,
            instance_id: rekey.fork_instance_id,
            lock: Some(lock),
            last_announce: None,
        };
        *self.ids.write() = CurrentIds {
            instance_id: state.instance_id.clone(),
            destination_hash: hash.to_hex_string(),
        };
        if self.announce {
            // With `last_announce` still `None`, the next heartbeat tick announces the fork.
            match timeout_at(deadline, self.send_announce(&mut state, &transport)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => warn!(
                    "Failed to announce mesh node {} for fork instance {} after re-keying; the heartbeat will announce it: {err:#}",
                    self.fingerprint, state.instance_id
                ),
                Err(_) => warn!(
                    "Mesh node {} did not announce fork instance {} within {}s of re-keying; the heartbeat will announce it",
                    self.fingerprint,
                    state.instance_id,
                    REKEY_GRACE.as_secs()
                ),
            }
        }
        Ok(())
    }

    /// Cancels and joins the node's tasks, stops the interfaces, drops the transport and
    /// releases the instance lock, in that order so nothing outlives what it depends on. The
    /// task joins share one grace window and the transport teardown another.
    async fn shutdown(&self) -> Result<()> {
        self.cancel.cancel();
        let tasks = std::mem::take(&mut *self.tasks.lock());
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for mut handle in tasks {
            if timeout_at(deadline, &mut handle).await.is_err() {
                handle.abort();
                warn!(
                    "Mesh task did not stop within {}s; aborted it",
                    SHUTDOWN_GRACE.as_secs()
                );
            }
        }

        let interfaces = std::mem::take(&mut *self.interfaces.lock().await);
        let mut state = self.destination.lock().await;
        let Some(transport) = self.transport.lock().await.take() else {
            return Ok(());
        };
        // The transport takes its handler lock for each of these, and a peer that trips its
        // resource-reject path can leave that lock held for good; the deadline is shared so
        // a wedged transport costs one grace window, not one per step.
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for iface in interfaces {
            stop_interface(&transport, iface, deadline).await;
        }
        if timeout_at(deadline, transport.deregister_destination(&state.hash))
            .await
            .is_err()
        {
            warn!(
                "Mesh destination {} could not be deregistered within {}s; dropping the transport",
                state.hash.to_hex_string(),
                SHUTDOWN_GRACE.as_secs()
            );
        }
        drop(transport);
        state.lock = None;
        if let Err(err) = self.peers.persist_if_dirty() {
            warn!("Failed to persist the mesh peer table on stop: {err:#}");
        }
        debug!(
            "Stopped mesh node {} (instance {}, destination {})",
            self.fingerprint,
            state.instance_id,
            state.hash.to_hex_string()
        );
        Ok(())
    }
}

/// Registers the destination for `instance_id` and attaches `app_data` to its announces;
/// each transport wait gives up at `deadline`. A destination registered but left without
/// its announce data is deregistered again, best effort, and the error says whether that
/// worked. The destination carries no `max_request_size`: rejecting an advertisement on it
/// deadlocks the upstream transport (rev 3ed5932), so oversize requests are dropped after
/// assembly by `R3Server` instead.
async fn register_destination(
    transport: &Transport,
    identity: &TransportIdentity,
    instance_id: &str,
    app_data: &[u8],
    deadline: tokio::time::Instant,
) -> Result<(Arc<Mutex<SingleInputDestination>>, AddressHash, OriginName)> {
    let name = DestinationName::new("coyote", &format!("mesh.{instance_id}"));
    let destination = SingleInputDestination::new(identity.clone(), name);
    let hash = destination.desc.address_hash;
    let origin = OriginName::of(&destination.desc.name);
    let dest = timeout_at(deadline, transport.register_destination(destination))
        .await
        .map_err(|_| {
            anyhow!(
                "The mesh transport did not register destination {} in time",
                hash.to_hex_string()
            )
        })?;
    let app_data = transport.set_destination_announce_app_data(&dest, Some(app_data.to_vec()));
    if timeout_at(deadline, app_data).await.is_err() {
        let released = timeout_at(deadline, transport.deregister_destination(&hash))
            .await
            .is_ok();
        bail!(
            "The mesh transport registered destination {} but did not attach its announce data in time; {}",
            hash.to_hex_string(),
            if released {
                "it was deregistered again"
            } else {
                "it could not be deregistered either, so the node serves it without announce data"
            }
        );
    }
    Ok((dest, hash, origin))
}

/// Builds and sends one announce for `dest`, returning when it was sent.
async fn announce_destination(
    transport: &Transport,
    dest: &Arc<Mutex<SingleInputDestination>>,
    hash: &AddressHash,
    app_data: &[u8],
) -> Result<Instant> {
    let packet = dest
        .lock()
        .await
        .announce(OsRng, Some(app_data))
        .map_err(|err| {
            anyhow!(
                "Failed to build the mesh announce for destination {}: {err}",
                hash.to_hex_string()
            )
        })?;
    transport.send_packet(packet).await;
    debug!(
        "Sent mesh announce for destination {}",
        hash.to_hex_string()
    );
    Ok(Instant::now())
}

/// Unwinds a start that failed once the transport existed: the joined interfaces are stopped
/// and the transport goes with them, which cancels its own tasks.
async fn abandon_start(transport: Transport, joined: Vec<JoinedInterface>) {
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    for iface in joined {
        stop_interface(&transport, iface, deadline).await;
    }
}

async fn join_interface(
    transport: &Transport,
    plan: &InterfacePlan,
    options: &NodeOptions,
) -> Result<JoinedInterface> {
    let joined = match plan {
        InterfacePlan::Lan => join_lan(transport).await?,
        InterfacePlan::Tcp { kind, endpoint } => {
            join_tcp(transport, kind, endpoint, options.connect_timeout).await?
        }
    };
    debug!("Joined mesh interface {}", plan.label());
    Ok(joined)
}

async fn join_lan(transport: &Transport) -> Result<JoinedInterface> {
    let plan = AutoRuntimePlan::from_system(
        AutoInterfaceConfig::default(),
        AutoInterfaceDeviceFilter::default(),
    )
    .map_err(|err| {
        anyhow!(
            "Failed to enumerate network interfaces for the mesh lan interface: {err}. Remove the lan entry from mesh.interfaces or fix the host's networking."
        )
    })?;
    let manager = transport.iface_manager();
    let channel = manager.lock().await.new_channel_with_role_and_mode(
        LAN_CHANNEL_CAPACITY,
        IfaceRole::Multicast,
        InterfaceMode::default(),
    );
    let host_iface = channel.address;
    let bridge = AutoInterfaceTransportRuntime::from_channel(channel, manager);
    match plan
        .spawn_discovery_runtime_with_native_scope_ids_and_transport(Some(bridge), None)
        .await
    {
        Ok(runtime) => Ok(JoinedInterface::Lan {
            host_iface,
            runtime,
        }),
        Err(err) => {
            detach_interface(
                transport,
                host_iface,
                "lan",
                tokio::time::Instant::now() + SHUTDOWN_GRACE,
            )
            .await;
            bail!(
                "Failed to bind the mesh lan interface: {err}. Another process may hold the discovery port; remove the lan entry from mesh.interfaces or stop that process."
            )
        }
    }
}

async fn join_tcp(
    transport: &Transport,
    kind: &'static str,
    endpoint: &str,
    connect_timeout: Duration,
) -> Result<JoinedInterface> {
    let client = TcpClient::new(endpoint).with_connect_timeout(connect_timeout);
    let status = client.runtime_status_handle();
    let context = transport.iface_manager().lock().await.new_context(client);
    let hash = *context.channel.address();
    let handle = tokio::spawn(TcpClient::spawn(context));
    let label = format!("{kind} {endpoint}");

    let deadline = Instant::now() + connect_timeout + Duration::from_secs(1);
    let failure = loop {
        let snapshot = status.to_json();
        match snapshot["stream_state"].as_str() {
            Some("connected") => {
                return Ok(JoinedInterface::Tcp {
                    hash,
                    handle,
                    label,
                });
            }
            // The client's first connect failed; it would now retry forever on its own.
            Some("reconnecting" | "closed") => {
                break snapshot["last_error"]
                    .as_str()
                    .unwrap_or("connection refused")
                    .to_string();
            }
            _ if Instant::now() >= deadline => break "connection timed out".to_string(),
            _ => sleep(CONNECT_POLL).await,
        }
    };
    stop_interface(
        transport,
        JoinedInterface::Tcp {
            hash,
            handle,
            label,
        },
        tokio::time::Instant::now() + SHUTDOWN_GRACE,
    )
    .await;
    bail!(
        "Mesh relay {endpoint} (type: {kind}) is unreachable: {failure}. The node cannot join the mesh until that relay is reachable; fix mesh.interfaces or the relay."
    )
}

/// Detaches the interface at `iface` from the transport, giving up at `deadline`; `false`
/// when it did not go.
async fn detach_interface(
    transport: &Transport,
    iface: AddressHash,
    label: &str,
    deadline: tokio::time::Instant,
) -> bool {
    if timeout_at(deadline, transport.stop_interface(iface))
        .await
        .is_err()
    {
        warn!(
            "Mesh interface {label} could not be detached from the transport within {}s",
            SHUTDOWN_GRACE.as_secs()
        );
        return false;
    }
    true
}

/// Detaches one interface, giving up at `deadline` on every wait against the transport.
async fn stop_interface(
    transport: &Transport,
    iface: JoinedInterface,
    deadline: tokio::time::Instant,
) {
    match iface {
        JoinedInterface::Lan {
            host_iface,
            runtime,
        } => {
            runtime.stop().await;
            if detach_interface(transport, host_iface, "lan", deadline).await {
                debug!("Left mesh interface lan");
            }
        }
        JoinedInterface::Tcp {
            hash,
            mut handle,
            label,
        } => {
            if !detach_interface(transport, hash, &label, deadline).await {
                handle.abort();
                return;
            }
            if timeout_at(deadline, &mut handle).await.is_err() {
                handle.abort();
                warn!(
                    "Mesh interface {label} did not stop within {}s; aborted it",
                    SHUTDOWN_GRACE.as_secs()
                );
                return;
            }
            debug!("Left mesh interface {label}");
        }
    }
}

/// What `record_announce` read off a Coyote announce and did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FiledAnnounce {
    change: PeerChange,
    display_name: Option<String>,
    protocol_version: u16,
    compatibility: Compatibility,
}

/// Files one received announce in the peer table; `None` when it is not a Coyote announce.
fn record_announce(
    peers: &PeerTable,
    destination_hash: String,
    identity_hash: String,
    name_hash: String,
    app_data: &[u8],
    hops: u8,
    now: SystemTime,
) -> Option<FiledAnnounce> {
    let Some(decoded) = AnnounceAppData::decode(app_data) else {
        debug!("Ignored announce from {destination_hash} ({hops} hops): not a Coyote node");
        return None;
    };
    debug!(
        "Received mesh announce from {destination_hash} ({hops} hops, protocol version {})",
        decoded.version
    );
    let compatibility = Compatibility::of(decoded.version);
    if let Compatibility::Incompatible { found } = compatibility {
        debug!(
            "Mesh peer {} is incompatible: it speaks protocol {found}, this Coyote supports {MESH_PROTOCOL_MIN_SUPPORTED}..={MESH_PROTOCOL_VERSION}",
            short(&destination_hash)
        );
    }
    let change = peers.observe(
        PeerSighting {
            destination_hash: destination_hash.clone(),
            identity_hash,
            name_hash,
            display_name: decoded.display_name.clone(),
            protocol_version: decoded.version,
            hops,
        },
        now,
    );
    match change {
        PeerChange::Added => debug!("Added mesh peer {destination_hash}"),
        PeerChange::Refreshed => debug!("Refreshed mesh peer {destination_hash}"),
    }
    Some(FiledAnnounce {
        change,
        display_name: decoded.display_name,
        protocol_version: decoded.version,
        compatibility,
    })
}

/// Fires of `mesh.peer.discovered` per second across all peers, and the most the bucket
/// holds. A filer that has spent its tokens drops the fire rather than queueing it.
const DISCOVERED_FIRES_PER_SEC: f64 = 16.0;

/// A destination's last `mesh.peer.discovered` fire and the facts it carried.
struct DiscoveredFire {
    at: SystemTime,
    display_name: Option<String>,
    protocol_version: u16,
    hops: u8,
}

/// Which `mesh.peer.discovered` fires get through. A peer's first sighting is always due;
/// a refresh only when its name, protocol version or hop count differ from what was last
/// fired for it, or that fire is at least `HEARTBEAT_SECS` old. On top of that a token
/// bucket caps the rate across all peers and may drop a due fire, in which case the peer's
/// next refresh fires with `COYOTE_MESH_FIRST_SEEN=false`. The map forgets destinations whose last fire is
/// older than two heartbeats and is capped at the peer table's size, so a flood of
/// distinct destinations cannot grow it without bound.
#[derive(Default)]
struct DiscoveredThrottle {
    fired: HashMap<String, DiscoveredFire>,
    tokens: f64,
    refilled_at: Option<SystemTime>,
    dropping: bool,
}

impl DiscoveredThrottle {
    fn admits(
        &mut self,
        destination: &str,
        filed: &FiledAnnounce,
        hops: u8,
        now: SystemTime,
    ) -> bool {
        let due = match (filed.change, self.fired.get(destination)) {
            (PeerChange::Added, _) | (PeerChange::Refreshed, None) => true,
            (PeerChange::Refreshed, Some(last)) => {
                last.display_name != filed.display_name
                    || last.protocol_version != filed.protocol_version
                    || last.hops != hops
                    || now
                        .duration_since(last.at)
                        .is_ok_and(|since| since.as_secs() >= HEARTBEAT_SECS)
            }
        };
        if !due {
            return false;
        }
        self.refill(now);
        if self.tokens < 1.0 {
            if !self.dropping {
                debug!(
                    "Mesh peer discovery is firing hooks faster than {DISCOVERED_FIRES_PER_SEC} a second; dropping fires until the rate falls"
                );
                self.dropping = true;
            }
            return false;
        }
        self.tokens -= 1.0;
        self.dropping = false;
        self.remember(destination, filed, hops, now);
        true
    }

    fn refill(&mut self, now: SystemTime) {
        self.tokens = match self.refilled_at {
            None => DISCOVERED_FIRES_PER_SEC,
            Some(at) => {
                let elapsed = now.duration_since(at).unwrap_or_default();
                (self.tokens + elapsed.as_secs_f64() * DISCOVERED_FIRES_PER_SEC)
                    .min(DISCOVERED_FIRES_PER_SEC)
            }
        };
        self.refilled_at = Some(now);
    }

    fn remember(&mut self, destination: &str, filed: &FiledAnnounce, hops: u8, now: SystemTime) {
        if !self.fired.contains_key(destination) {
            if let Some(forgotten_before) = now.checked_sub(Duration::from_secs(2 * HEARTBEAT_SECS))
            {
                self.fired.retain(|_, last| last.at > forgotten_before);
            }
            if self.fired.len() >= PEER_TABLE_MAX_ENTRIES {
                let oldest = self
                    .fired
                    .iter()
                    .min_by_key(|(_, last)| last.at)
                    .map(|(hash, _)| hash.clone());
                if let Some(oldest) = oldest {
                    self.fired.remove(&oldest);
                }
            }
        }
        self.fired.insert(
            destination.to_string(),
            DiscoveredFire {
                at: now,
                display_name: filed.display_name.clone(),
                protocol_version: filed.protocol_version,
                hops,
            },
        );
    }
}

/// Where a heard announce goes: the peer table, the trust list's sightings, and
/// `mesh.peer.discovered`, trusted or not, compatible or not, as far as the throttle
/// lets it through.
struct AnnounceFiler<'a> {
    peers: &'a PeerTable,
    trust: &'a TrustStore,
    hooks: &'a MeshHooks,
    throttle: DiscoveredThrottle,
}

impl AnnounceFiler<'_> {
    fn file(
        &mut self,
        destination_hash: String,
        identity_hash: String,
        name_hash: String,
        app_data: &[u8],
        hops: u8,
        now: SystemTime,
    ) -> Option<PeerChange> {
        let filed = record_announce(
            self.peers,
            destination_hash.clone(),
            identity_hash.clone(),
            name_hash.clone(),
            app_data,
            hops,
            now,
        )?;
        self.trust
            .mark_seen(&destination_hash, &identity_hash, &name_hash, now);
        if self.throttle.admits(&destination_hash, &filed, hops, now) {
            self.hooks.fire(MeshEvent::PeerDiscovered {
                destination: destination_hash,
                identity: identity_hash,
                name: filed.display_name,
                hops,
                protocol_version: filed.protocol_version,
                compatibility: filed.compatibility,
                change: filed.change,
            });
        }
        Some(filed.change)
    }
}

/// The pre-flight half of the version gate: a peer the table knows speaks a protocol this
/// node does not is refused before any link is opened. A peer the table does not know
/// proceeds; the wire decides.
fn refuse_incompatible_peer(peers: &PeerTable, dest_hex: &str, path: &str) -> Result<(), R3Error> {
    match peers.get(dest_hex).map(|record| record.compatibility) {
        Some(Compatibility::Incompatible { found }) => {
            debug!(
                "Mesh request for {path} to {} was not sent: the peer speaks protocol {found}, this Coyote supports {MESH_PROTOCOL_MIN_SUPPORTED}..={MESH_PROTOCOL_VERSION}",
                short(dest_hex)
            );
            Err(R3Error::UnsupportedVersion {
                found: Some(found),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            })
        }
        Some(Compatibility::Compatible) | None => Ok(()),
    }
}

/// The other half: a peer that refused this node's version over the wire is marked so the
/// next request stops at `refuse_incompatible_peer`. The peer's `max` is the newest
/// protocol it speaks, which is what the record names. A refusal whose window is empty or
/// contains the version this node sent is not a version mismatch, and marks nothing: the
/// peer's word alone does not get to cut a compatible peer off.
fn note_version_refusal(
    peers: &PeerTable,
    dest_hex: &str,
    path: &str,
    sent: u16,
    outcome: &Result<RequestOutcome, R3Error>,
) {
    let Err(R3Error::UnsupportedVersion { min, max, .. }) = outcome else {
        return;
    };
    if min > max || (*min..=*max).contains(&sent) {
        debug!(
            "Mesh request for {path} to {} was refused for protocol version {sent} with an inconsistent window {min}..={max}; the peer is not marked",
            short(dest_hex)
        );
        return;
    }
    peers.mark_incompatible(dest_hex, *max);
    debug!(
        "Mesh request for {path} to {} was refused for its protocol version; the peer speaks protocol {max} and is marked incompatible",
        short(dest_hex)
    );
}

async fn receive_announces(
    mut announces: broadcast::Receiver<AnnounceEvent>,
    peers: Arc<PeerTable>,
    trust: Arc<TrustStore>,
    propagation_nodes: Arc<PropagationNodeTable>,
    hooks: MeshHooks,
    cancel: CancellationToken,
) {
    let mut filer = AnnounceFiler {
        peers: &peers,
        trust: &trust,
        hooks: &hooks,
        throttle: DiscoveredThrottle::default(),
    };
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => break,
            event = announces.recv() => match event {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!("Mesh announce receiver fell behind and skipped {skipped} announces");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };
        let desc = event.destination.lock().await.desc;
        let now = SystemTime::now();
        if propagation_nodes.observe_announce(&desc, event.app_data.as_slice(), event.hops, now) {
            continue;
        }
        filer.file(
            desc.address_hash.to_hex_string(),
            desc.identity.address_hash.to_hex_string(),
            hex_lower(&event.name_hash),
            event.app_data.as_slice(),
            event.hops,
            now,
        );
    }
}

fn log_aged_out_peers(aged_out: &[String]) {
    for hash in aged_out {
        debug!("Aged out mesh peer {hash}");
    }
}

async fn sweep_peers(peers: Arc<PeerTable>, cancel: CancellationToken) {
    let mut ticks = interval(Duration::from_secs(HEARTBEAT_SECS));
    // The first tick of an interval fires immediately; the table was already swept on load.
    ticks.tick().await;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticks.tick() => {
                log_aged_out_peers(&peers.sweep(SystemTime::now()));
            }
        }
    }
}

async fn persist_peers_periodically(peers: Arc<PeerTable>, cancel: CancellationToken) {
    let mut ticks = interval(Duration::from_secs(PEER_PERSIST_INTERVAL_SECS));
    // The first tick of an interval fires immediately; nothing has changed since load.
    ticks.tick().await;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticks.tick() => {
                if let Err(err) = peers.persist_if_dirty() {
                    warn!("Failed to persist the mesh peer table: {err:#}");
                }
            }
        }
    }
}

async fn announce_periodically(runtime: Arc<MeshRuntime>, cancel: CancellationToken) {
    let mut ticks = interval(Duration::from_secs(HEARTBEAT_SECS));
    // The first tick fires immediately; `start` already sent that announce.
    ticks.tick().await;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = ticks.tick() => {
                if let Err(err) = runtime.announce_now().await {
                    warn!("Failed to send the mesh heartbeat announce: {err:#}");
                }
            }
        }
    }
}

/// The process-wide home of the running node. Empty until the mesh is turned on; shared by
/// every `AppState` clone so replacing the config never detaches the runtime.
///
/// The snapshot, objective override, digest, user brief and brief live in lock-free slots:
/// the REPL holds the session context write-locked for a whole turn, so anything that
/// serves peers must be readable without it.
///
/// `snapshot()` is the picture taken at the last turn boundary. `objective_override()`,
/// `digest()` and `user_brief()` are live and may be newer than it. Consumers overlay
/// `objective_override()` on `snapshot().objective` when it is `Some`. `brief()` is
/// derived: `reassemble_brief` rebuilds it from the snapshot (mode, todo, card fields),
/// the digest and the user brief whenever any of them changes, so it is never stored
/// directly. The live `brief()` is authoritative, including `None`: a cleared brief must
/// not fall back to the snapshot's copy. `snapshot().brief` is the value at capture, kept
/// so a snapshot is self-describing.
///
/// `digest_epoch` counts the clears made by `clear_digest_for_new_epoch`. A generation
/// records the epoch it started under and publishes through `publish_digest_at`, which
/// refuses once the epoch has moved on, so a digest of one session cannot land after the
/// switch to another. Both run under `reassembly`, so the check and the store are one step.
///
/// The notifier slot is where lines meant for the person at the keyboard go; it is filled by
/// whichever front end owns the terminal, so mesh code never needs to know which one is
/// running.
///
/// The idle slot is where events that also concern the model go; the interactive REPL's
/// idle-time driver fills it. Without one, `push_idle` keeps the human line and drops the
/// model's copy, since a headless run has no transcript for it to reach.
///
/// Peer messages land in `peer_inbox` for the model to read on its next `mesh__check_inbox`,
/// with a note in `model_notes` so it knows to look; replies to questions this node asked
/// are matched in `correlations` first. With an envoy attached, messages and questions
/// go to it instead of the inbox; the questions it escalates to the person at the
/// keyboard wait in `inbound`, a per-instance file like `correlations`' store.
///
/// `limits` holds each peer's hourly windows and in-flight envoy runs. It belongs to the
/// slot rather than the node so a `.mesh off`/`.mesh on` does not hand a flooding peer
/// a fresh window; `install` configures it from the node's config.
#[derive(Default)]
pub(crate) struct MeshSlot {
    inner: RwLock<Option<Arc<MeshRuntime>>>,
    snapshot: ArcSwapOption<MeshSnapshot>,
    objective_override: ArcSwapOption<String>,
    digest: ArcSwapOption<Digest>,
    digest_epoch: AtomicU64,
    user_brief: ArcSwapOption<String>,
    brief: ArcSwapOption<Brief>,
    reassembly: parking_lot::Mutex<()>,
    notifier: ArcSwapOption<Arc<dyn NotificationSink>>,
    idle: ArcSwapOption<Arc<dyn IdleSink>>,
    envoy: ArcSwapOption<Arc<dyn EnvoySink>>,
    hooks: MeshHooks,
    limits: Arc<PeerLimits>,
    peer_inbox: PeerInbox,
    correlations: Correlations,
    model_notes: ModelNotes,
    inbound: parking_lot::Mutex<Option<Arc<InboundStore>>>,
}

impl MeshSlot {
    pub(crate) fn get(&self) -> Option<Arc<MeshRuntime>> {
        self.inner.read().clone()
    }

    pub(crate) fn limits(&self) -> &Arc<PeerLimits> {
        &self.limits
    }

    /// Refuses while a node is already running: two nodes in one process would fight over
    /// the same instance lock and identity. Installing also puts this slot behind the
    /// node's `/status` and `/message` providers, knock gate and peer surface, held weakly
    /// since the slot owns the node, and reopens the questions the node's instance left
    /// pending on disk, adopting them before the `/message` provider is registered so a
    /// reply admitted in between still finds its question. A pending file this Coyote
    /// cannot read is logged with its remedy and the node serves with no questions
    /// pending: a late reply to one of them lands as an ordinary message. So this fails
    /// only for a node already on.
    /// A knock the gate admits between `MeshRuntime::start` and this call is cached but
    /// not surfaced, and not marked as surfaced either, so a repeat from that identity
    /// still earns its one line. The caller refreshes the session's tool catalog once
    /// this returns, since the `mesh__*` tools are declared only while a node is on.
    pub(crate) fn install(self: &Arc<Self>, runtime: Arc<MeshRuntime>) -> Result<()> {
        if self.get().is_some() {
            bail!(MESH_ALREADY_ON);
        }
        let store = PendingStore::new(runtime.cache_dir(), &runtime.current_instance_id());
        let inbound = InboundStore::new(runtime.cache_dir(), &runtime.current_instance_id());
        let pending = reopen_pending(&store);
        // Nothing may `.await` while this guard is held: the status handler reads the same
        // lock for the display name.
        let mut slot = self.inner.write();
        if slot.is_some() {
            bail!(MESH_ALREADY_ON);
        }
        self.correlations.adopt(store, pending);
        *self.inbound.lock() = Some(Arc::new(inbound));
        self.limits.configure(runtime.peer_limits());
        let source = Arc::downgrade(self) as Weak<dyn CardSource>;
        runtime
            .dispatcher()
            .register(STATUS_PATH, Arc::new(StatusHandler::new(source)))?;
        let surface = Arc::downgrade(self) as Weak<dyn PeerSurface>;
        runtime.dispatcher().register(
            MESSAGE_PATH,
            Arc::new(PeerMessageHandler::new(surface.clone())),
        )?;
        runtime.attach_peer_surface(surface);
        runtime
            .knock_gate()
            .attach(Arc::downgrade(self) as Weak<dyn KnockSurface>);
        // A node started on its own handle takes the slot's sink, so what it fires
        // reaches the same place as what the slot fires.
        if !self.hooks.same_handle(runtime.hooks())
            && let Some(sink) = self.hooks.current()
        {
            runtime.hooks().set(sink);
        }
        let facts = node_facts(&runtime);
        *slot = Some(runtime);
        // Fired outside the lock: the sink is not the slot's to trust with it.
        drop(slot);
        self.hooks.fire(MeshEvent::Started(facts));
        Ok(())
    }

    /// Takes the node out of the slot and shuts it down, letting go of its pending
    /// questions with it: they stay on disk for the next install of that instance.
    /// Detaching before the shutdown grace means a reply served during the grace lands as
    /// an ordinary message and its question stays open on disk, which is the price of
    /// never writing a stopped node's file. An envoy run in flight is cut short first:
    /// its reply would go out through the node being stopped. `Ok(false)` when nothing
    /// was running. The caller refreshes the session's tool catalog afterwards so the
    /// `mesh__*` tools go with the node.
    pub(crate) async fn stop(&self) -> Result<bool> {
        let taken = self.inner.write().take();
        match taken {
            Some(runtime) => {
                if let Some(envoy) = self.envoy.load_full() {
                    envoy.interrupt();
                }
                self.correlations.detach_store();
                self.inbound.lock().take();
                let facts = node_facts(&runtime);
                let shutdown = runtime.shutdown().await;
                self.hooks.fire(MeshEvent::Stopped(facts));
                shutdown?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Re-keys the running node for a forked session and binds the pending questions to
    /// the fork's own file, since a fork asks its own questions and must not collect the
    /// original's; a no-op while the mesh is off. The fork's questions are adopted before
    /// the node re-keys so a reply the fork's destination serves at once finds them, and
    /// a re-key that fails puts the original's back, since the original is what stays
    /// served. Questions peers escalated before the fork are copied into the fork's
    /// inbound file so `.mesh answer` still finds them after the switch. A fork file this
    /// Coyote cannot read is logged and the fork starts with no questions pending, as
    /// `install` does: the node must not be reported as failed for a cache file.
    pub(crate) async fn rekey(&self, rekey: ForkRekey) -> Result<()> {
        let Some(runtime) = self.get() else {
            return Ok(());
        };
        let fork_store = PendingStore::new(runtime.cache_dir(), &rekey.fork_instance_id);
        let pending = reopen_pending(&fork_store);
        self.correlations.adopt(fork_store, pending);
        let fork_inbound = Arc::new(InboundStore::new(
            runtime.cache_dir(),
            &rekey.fork_instance_id,
        ));
        let current_inbound = self.inbound.lock().clone();
        let carry = || {
            if let Some(current) = &current_inbound
                && let Err(err) = fork_inbound.adopt_from(current, SystemTime::now())
            {
                warn!(
                    "Mesh questions escalated before the fork could not be carried into it, so `.mesh answer` will not find them: {err:#}"
                );
            }
        };
        carry();
        *self.inbound.lock() = Some(Arc::clone(&fork_inbound));
        // A record filed into the old store between the first carry and the swap would
        // otherwise be lost; adoption is by id, so repeating it is harmless.
        carry();
        let rekeyed = runtime.rekey(rekey).await;
        if rekeyed.is_err() {
            let instance_id = runtime.current_instance_id();
            let original = PendingStore::new(runtime.cache_dir(), &instance_id);
            let pending = reopen_pending(&original);
            self.correlations.adopt(original, pending);
            *self.inbound.lock() = Some(Arc::new(InboundStore::new(
                runtime.cache_dir(),
                &instance_id,
            )));
        }
        rekeyed
    }

    pub(crate) fn publish(&self, snapshot: MeshSnapshot) {
        self.snapshot.store(Some(Arc::new(snapshot)));
        self.reassemble_brief();
    }

    pub(crate) fn snapshot(&self) -> Option<Arc<MeshSnapshot>> {
        self.snapshot.load_full()
    }

    /// Errors only when nothing was ever published. Whether a published snapshot is too old
    /// to serve is the caller's decision, via `MeshSnapshot::age`.
    // Reached by the envoy request handlers once they land.
    #[allow(dead_code)]
    pub(crate) fn snapshot_or_stale_error(&self) -> Result<Arc<MeshSnapshot>> {
        self.snapshot().ok_or_else(|| {
            anyhow!(
                "No session snapshot has been published yet, so there is nothing to serve: this process has not reached its first turn boundary. Every entry point (each REPL line, headless run, or ACP prompt) publishes one when its turn ends, so let the current turn finish or send one line, then try again."
            )
        })
    }

    pub(crate) fn set_objective_override(&self, objective: Option<String>) {
        self.objective_override.store(non_blank(objective));
        self.reassemble_brief();
    }

    pub(crate) fn objective_override(&self) -> Option<Arc<String>> {
        self.objective_override.load_full()
    }

    /// Stores or clears the digest unconditionally and rebuilds the brief. Production
    /// clears through `clear_digest_for_new_epoch` and stores through `publish_digest_at`,
    /// so a generation that outlived its session is refused; tests seed a digest directly.
    #[cfg(test)]
    pub(crate) fn publish_digest(&self, digest: Option<Digest>) {
        let rebuilding = self.reassembly.lock();
        self.digest.store(digest.map(Arc::new));
        self.reassemble_brief_locked(&rebuilding);
    }

    pub(crate) fn digest(&self) -> Option<Arc<Digest>> {
        self.digest.load_full()
    }

    pub(crate) fn digest_epoch(&self) -> u64 {
        self.digest_epoch.load(Ordering::Acquire)
    }

    /// Clears the digest and opens a new epoch in one step, returning it. A generation
    /// started under the previous epoch is refused by `publish_digest_at` from here on,
    /// even if it finishes after this call.
    pub(crate) fn clear_digest_for_new_epoch(&self) -> u64 {
        let rebuilding = self.reassembly.lock();
        let epoch = self.digest_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.digest.store(None);
        self.reassemble_brief_locked(&rebuilding);
        epoch
    }

    /// Stores `digest` and rebuilds the brief only while `epoch` is still the current one;
    /// `false` means the session changed while the digest was being generated and it was
    /// dropped.
    pub(crate) fn publish_digest_at(&self, epoch: u64, digest: Digest) -> bool {
        let rebuilding = self.reassembly.lock();
        if self.digest_epoch.load(Ordering::Acquire) != epoch {
            return false;
        }
        self.digest.store(Some(Arc::new(digest)));
        if self.reassemble_brief_locked(&rebuilding) {
            self.fire_brief_updated(BriefUpdateSource::Digest);
        }
        true
    }

    pub(crate) fn set_user_brief(&self, text: Option<String>) {
        let rebuilding = self.reassembly.lock();
        self.user_brief.store(non_blank(text));
        if self.reassemble_brief_locked(&rebuilding) {
            self.fire_brief_updated(BriefUpdateSource::User);
        }
    }

    pub(crate) fn user_brief(&self) -> Option<Arc<String>> {
        self.user_brief.load_full()
    }

    pub(crate) fn brief(&self) -> Option<Arc<Brief>> {
        self.brief.load_full()
    }

    /// Rebuilds the served brief from the current snapshot, digest and user brief. The
    /// snapshot's `brief.mode` decides what goes in; with no snapshot yet there is no mode
    /// to honour, so nothing is served. Rebuilds are serialised: the digest task and the
    /// turn boundary may both store and rebuild at once, and without the lock the later
    /// store could be overwritten by a rebuild that read before it.
    pub(crate) fn reassemble_brief(&self) {
        let rebuilding = self.reassembly.lock();
        self.reassemble_brief_locked(&rebuilding);
    }

    /// The rebuild itself; the guard proves the caller holds `reassembly`, which is not
    /// reentrant. `true` when the served brief changed.
    fn reassemble_brief_locked(&self, _rebuilding: &parking_lot::MutexGuard<'_, ()>) -> bool {
        let brief = self.snapshot().and_then(|snapshot| {
            let now = SystemTime::now();
            let objective_override = self.objective_override();
            let digest = self.digest();
            let digest_objective = digest_objective_for(&snapshot, digest.as_deref());
            let display_name = CardSource::display_name(self);
            let card = build_card(
                Some(&snapshot),
                objective_override.as_deref().map(String::as_str),
                digest_objective.as_deref(),
                display_name.as_deref(),
                now,
            );
            let user_brief = self.user_brief();
            assemble_brief(
                snapshot.brief.mode,
                Some(&card),
                digest.as_deref(),
                user_brief.as_deref().map(String::as_str),
                &snapshot.todo,
            )
        });
        let changed = self.brief.load().as_deref() != brief.as_ref();
        self.brief.store(brief.map(Arc::new));
        changed
    }

    /// `mesh.brief.updated` with the size of what is served now; never the text.
    fn fire_brief_updated(&self, source: BriefUpdateSource) {
        let chars = self.brief().map_or(0, |brief| brief.text.chars().count());
        self.hooks.fire(MeshEvent::BriefUpdated { source, chars });
    }

    /// Installs where human-facing lines go. The interactive REPL installs its prompt
    /// printer at start-up; headless runs (`-e`, macros, the ACP server) install nothing.
    pub(crate) fn set_notifier(&self, sink: Arc<dyn NotificationSink>) {
        self.notifier.store(Some(Arc::new(sink)));
    }

    /// The REPL calls this once its loop has exited, so a line notified during teardown
    /// falls back to stderr instead of a printer nobody drains any more.
    pub(crate) fn clear_notifier(&self) {
        self.notifier.store(None);
    }

    /// Renders once, then hands the lines to the installed sink. With no sink installed
    /// they go to stderr, so headless modes still see them. A stderr that has gone away
    /// (the parent of a headless run closed it) loses the line rather than panicking the
    /// task that carried it.
    pub(crate) fn notify(&self, note: Notification) {
        let rendered = note.render();
        match self.notifier.load_full() {
            Some(sink) => sink.notify(rendered),
            None => {
                use std::io::Write as _;
                let mut stderr = std::io::stderr().lock();
                for line in rendered.lines() {
                    let _ = writeln!(stderr, "{line}");
                }
            }
        }
    }

    pub(crate) fn set_idle(&self, sink: Arc<dyn IdleSink>) {
        self.idle.store(Some(Arc::new(sink)));
    }

    pub(crate) fn clear_idle(&self) {
        self.idle.store(None);
    }

    /// Installs the envoy that answers inbound messages and questions. Its owner is the
    /// session that runs it, so the slot holds it behind a hook it can drop at any time.
    pub(crate) fn set_envoy(&self, sink: Arc<dyn EnvoySink>) {
        self.envoy.store(Some(Arc::new(sink)));
    }

    pub(crate) fn clear_envoy(&self) {
        self.envoy.store(None);
    }

    /// Installs who runs the `mesh.*` hooks, on this slot's handle and on the installed
    /// node's when that is a separate one, so this may come before or after `install`.
    pub(crate) fn set_hook_sink(&self, sink: Arc<dyn MeshHookSink>) {
        self.hooks.set(Arc::clone(&sink));
        if let Some(hooks) = self.separate_runtime_hooks() {
            hooks.set(sink);
        }
    }

    pub(crate) fn clear_hook_sink(&self) {
        self.hooks.clear();
        if let Some(hooks) = self.separate_runtime_hooks() {
            hooks.clear();
        }
    }

    /// The installed node's hook handle when it is not a clone of this slot's.
    fn separate_runtime_hooks(&self) -> Option<MeshHooks> {
        self.get()
            .map(|runtime| runtime.hooks().clone())
            .filter(|hooks| !hooks.same_handle(&self.hooks))
    }

    /// The handle to start a node with, so its events reach the sink installed here.
    pub(crate) fn hooks(&self) -> MeshHooks {
        self.hooks.clone()
    }

    #[cfg(test)]
    pub(crate) fn envoy_attached(&self) -> bool {
        self.envoy.load().is_some()
    }

    /// Hands an event to the idle-time driver. With no driver installed the human line
    /// goes out through `notify` and the model's copy is dropped, since there is no
    /// transcript to deliver it to. With a driver whose queue is full the whole event is
    /// dropped: the driver counts the overflow (reported at its next summary tick while it
    /// is running, logged once when it stops), and printing the line anyway would let a
    /// flood heavy enough to fill the queue skip the driver's rate limiter. `false` only
    /// for that drop.
    pub(crate) fn push_idle(&self, note: IdleNotify) -> bool {
        match self.idle.load_full() {
            Some(sink) => sink.push(note).is_ok(),
            None => {
                self.notify(Notification::new(note.source, note.text));
                true
            }
        }
    }

    pub(crate) fn peer_inbox(&self) -> &PeerInbox {
        &self.peer_inbox
    }

    pub(crate) fn correlations(&self) -> &Correlations {
        &self.correlations
    }

    /// The notes queued since the last take, for the model's next tool batch.
    pub(crate) fn take_model_notes(&self) -> Vec<SystemNotification> {
        self.model_notes.take()
    }

    /// The store of questions peers asked that the envoy escalated; `None` while the
    /// mesh is off.
    pub(crate) fn inbound_store(&self) -> Option<Arc<InboundStore>> {
        self.inbound.lock().clone()
    }

    /// Gives a slot with no node a store, so an envoy escalation can be tested without
    /// a runtime to `install`.
    #[cfg(test)]
    pub(crate) fn set_inbound_store_for_tests(&self, store: Arc<InboundStore>) {
        *self.inbound.lock() = Some(store);
    }

    /// Where every admitted inbound peer message ends up, from a link or a propagation
    /// node (one refused admission is filed by `file_peer` instead). A reply to a
    /// question this node asked answers its correlation and goes to the inbox. A
    /// message or a question is offered to the envoy first when one is attached, and
    /// the envoy owns it from then on: what it answered comes back through
    /// `record_envoy_exchange`, what it could not through `record_envoy_fallback`. An
    /// envoy that refuses (its queue is full, or the sender is over one of its
    /// ceilings), a bulletin, or no envoy at all means the inbox path as ever; a
    /// refusal also goes back to the peer with its typed reason and earns the person at
    /// the keyboard one refusal line per reason per hour (the inbox summary line still
    /// prints per message; over-limit store-and-forward messages are filed too, bounded
    /// by the inbox and idle-sink caps, not by the hourly gate), so a flood cannot flood
    /// the terminal. Anything that arrived naming a message in `in_reply_to` takes the
    /// inbox path too, whatever its kind: a peer's envoy replying to our envoy's reply
    /// would otherwise keep the two talking forever. Runs on a blocking thread off the
    /// server's request path or on the fetch task, so nothing here awaits.
    pub(crate) fn deliver_peer(&self, mut message: PeerMessage) {
        let wire_reply = message.in_reply_to.is_some();
        let answered = self.answer_correlation(&mut message);
        let for_envoy =
            !answered && !wire_reply && matches!(message.kind, PeerKind::Message | PeerKind::Ask);
        let envoy = for_envoy.then(|| self.envoy.load_full()).flatten();
        let received = ReceivedFacts::of(&message);
        let routed = match envoy {
            None => {
                self.deliver_to_inbox(message, answered);
                Routed::Inbox
            }
            Some(sink) => {
                let (kind, id) = (message.kind, message.message_id.clone());
                let id8 = short(&message.source_identity).to_string();
                match sink.accept(EnvoyJob {
                    message: message.clone(),
                    reservation: None,
                }) {
                    Ok(()) => {
                        debug!("Mesh {kind} {id} from {id8} handed to the envoy");
                        Routed::Envoy
                    }
                    Err(refusal) => {
                        debug!(
                            "Mesh {kind} {id} from {id8} refused by the envoy: {}; delivering it to the inbox",
                            refusal.reason.as_str()
                        );
                        self.refuse_for_envoy(message, refusal);
                        Routed::Inbox
                    }
                }
            }
        };
        self.hooks.fire(received.event(routed));
    }

    /// The envoy refused `original` with a typed reason: it takes the inbox path and the
    /// person at the keyboard gets the folded notice. Replaces the per-message busy line,
    /// which is what a flood would flood the terminal with.
    pub(crate) fn record_envoy_refusal(&self, original: PeerMessage, refusal: &PeerRefusal) {
        let who = self.peer_label(&original.source_identity, &original.source_destination);
        let identity = original.source_identity.clone();
        let via = original.via;
        self.deliver_to_inbox(original, false);
        self.surface_refusal(&identity, refusal, &who, via);
    }

    /// `record_envoy_refusal` plus the correlated reply that tells the peer why, sent
    /// off the request path since this runs where nothing may await. A loop-guard
    /// refusal is never sent: a reply to a reply is the loop it guards against.
    pub(crate) fn refuse_for_envoy(&self, message: PeerMessage, refusal: PeerRefusal) {
        let destination = message.source_destination.clone();
        let id = message.message_id.clone();
        self.record_envoy_refusal(message, &refusal);
        if refusal.reason != RefusalReason::LoopGuard {
            self.send_refusal_reply(&destination, &id, &refusal);
        }
    }

    /// Sends the peer at `destination` the typed refusal of its message `id`, off the
    /// request path since this runs where nothing may await. A reply that cannot go is
    /// logged; the REPL has its line and the inbox the original.
    fn send_refusal_reply(&self, destination: &str, id: &str, refusal: &PeerRefusal) {
        let dest8 = short(destination).to_string();
        let out = match OutboundPeer::new(
            PeerKind::Reply,
            refusal.reason.peer_text(),
            None,
            Some(id),
            Some(refusal.fields()),
        ) {
            Ok(out) => out,
            Err(err) => {
                warn!("Mesh refusal of {id} to instance {dest8} could not be built: {err}");
                return;
            }
        };
        match (self.get(), tokio::runtime::Handle::try_current()) {
            (Some(runtime), Ok(handle)) => {
                let destination = destination.to_string();
                let id = id.to_string();
                handle.spawn(async move {
                    if let Err(err) = runtime.send_peer(&destination, &out).await {
                        warn!("Mesh refusal of {id} to instance {dest8} could not be sent: {err}");
                    }
                });
            }
            (Some(_), Err(_)) => warn!(
                "Mesh refusal of {id} to instance {dest8} could not be sent: no async runtime to send it with"
            ),
            (None, _) => {
                warn!("Mesh refusal of {id} to instance {dest8} could not be sent: mesh is off")
            }
        }
    }

    /// Notes one refusal of `identity` for folding and prints what the fold says: the
    /// first refusal of each reason in the hour, and the counts of any window that has
    /// since rolled over.
    fn surface_refusal(&self, identity: &str, refusal: &PeerRefusal, who: &str, via: PeerVia) {
        let notice = self
            .limits
            .note_refusal(identity, refusal.reason, Instant::now());
        self.push_peer_lines(identity, fold_lines(who, refusal.reason, via, &notice));
    }

    /// Prints the folded counts of a rolled-over window when a message from `identity`
    /// is admitted, so they surface even once the refusals have stopped.
    fn surface_fold_reports(&self, identity: &str, who: &str) {
        let reports = self.limits.take_fold_reports(identity, Instant::now());
        self.push_peer_lines(identity, fold_report_lines(who, &reports));
    }

    fn push_peer_lines(&self, identity: &str, lines: Vec<String>) {
        let id8 = short(identity).to_string();
        for text in lines {
            self.push_idle(IdleNotify {
                source: Source::Message,
                origin: Origin::Peer(id8.clone()),
                text,
                model_note: None,
            });
        }
    }

    /// The envoy answered `original` with `reply_text`. Both land in the inbox, the
    /// original first, so the model reads the exchange in order on its next
    /// `mesh__check_inbox`; the model gets one note for the pair and the person at the
    /// keyboard one line naming both sides. The reply is minted through the one
    /// sanitiser as a message from this node's destination, so it reads back like any
    /// other peer message.
    pub(crate) fn record_envoy_exchange(&self, original: &PeerMessage, reply_text: &str) {
        let runtime = self.get();
        let reply = PeerMessage::new(RawPeerMessage {
            source_identity: runtime
                .as_ref()
                .map(|runtime| runtime.fingerprint().to_string())
                .unwrap_or_default(),
            source_destination: runtime
                .as_ref()
                .map(|runtime| runtime.current_destination_hash())
                .unwrap_or_default(),
            destination: original.source_destination.clone(),
            title: None,
            content: reply_text.to_string(),
            fields: None,
            timestamp: unix_now(),
            message_id: uuid::Uuid::new_v4().simple().to_string(),
            in_reply_to: Some(original.message_id.clone()),
            kind: PeerKind::Reply,
            via: PeerVia::Direct,
        });
        let who = self.peer_name(original);
        let asked = match original.kind {
            PeerKind::Ask => "asked",
            _ => "said",
        };
        let half = PEER_LINE_MAX_CHARS / 2;
        let text = format!(
            "{who} {asked}: {}; envoy replied: {}",
            first_words(original, half),
            first_words(&reply, half)
        );
        let local_id = format!("peer:{}", short(&original.source_destination));
        let id8 = short(&original.source_identity).to_string();
        self.peer_inbox.deliver(original.clone());
        self.peer_inbox.deliver(reply);
        self.model_notes.push(mesh_notification(
            "peer_message",
            &local_id,
            "mesh",
            true,
            CHECK_INBOX_NEXT_ACTION.to_string(),
        ));
        self.push_idle(IdleNotify {
            source: Source::Message,
            origin: Origin::Peer(id8),
            text,
            model_note: None,
        });
    }

    /// The envoy took `original` but did not answer it (timed out, unavailable or
    /// failed): it goes down the inbox path as if no envoy were attached, and the person
    /// at the keyboard gets one more line with `reason`, since the summary line alone
    /// would not tell them the envoy was tried.
    pub(crate) fn record_envoy_fallback(&self, original: PeerMessage, reason: &str) {
        let text = format!(
            "{reason}; {} is in the inbox",
            original.summary_line(self.display_name_of(&original).as_deref())
        );
        let id8 = short(&original.source_identity).to_string();
        self.deliver_to_inbox(original, false);
        self.push_idle(IdleNotify {
            source: Source::Message,
            origin: Origin::Peer(id8),
            text,
            model_note: None,
        });
    }

    /// The envoy asked the human about `original` and the answer will go out through
    /// `answer_inbound`. The original takes the inbox path so the model can read it,
    /// but its note tells the model to wait for `.mesh answer {id}` rather than reply to
    /// a question the human is already being asked; `id` is the peer's message id as
    /// `PeerMessage::new` cleaned it, the same one the idle line shows. The person at
    /// the keyboard gets one line saying the envoy escalated.
    pub(crate) fn record_envoy_escalated(&self, original: PeerMessage, id: &str) {
        let summary = original.summary_line(self.display_name_of(&original).as_deref());
        let id8 = short(&original.source_identity).to_string();
        let local_id = format!("peer:{}", short(&original.source_destination));
        let event = match original.kind {
            PeerKind::Ask => "peer_ask",
            _ => "peer_message",
        };
        self.peer_inbox.deliver(original);
        self.model_notes.push(mesh_notification(
            event,
            &local_id,
            "mesh",
            true,
            format!("the human was asked; wait for `.mesh answer {id}` instead of replying"),
        ));
        self.push_idle(IdleNotify {
            source: Source::Message,
            origin: Origin::Peer(id8),
            text: format!("envoy escalated to the human; {summary} is in the inbox"),
            model_note: None,
        });
    }

    /// A human answer to the escalated inbound question `id`. A live envoy run still
    /// holding the question takes it, and the question stays filed until that run
    /// delivers; otherwise it goes to the peer as a reply, leaves the store here and is
    /// recorded for the leader, whose earlier note said to wait for it.
    /// Never touches `correlations`: this answers a peer's question, not one of ours.
    pub(crate) async fn answer_inbound(&self, id: &str, text: &str) -> Result<()> {
        let Some(store) = self.inbound_store() else {
            bail!("Mesh is off; turn it on with `.mesh on` before answering {id}");
        };
        let Some(record) = store.get(id)? else {
            bail!("no open question {id}");
        };
        if self
            .envoy
            .load_full()
            .is_some_and(|sink| sink.answer(id, text))
        {
            return Ok(());
        }
        let Some(runtime) = self.get() else {
            bail!(
                "Mesh is off, so the answer to {id} cannot be sent to {}",
                short(&record.peer_destination)
            );
        };
        let reply = OutboundPeer::new(PeerKind::Reply, text, None, Some(id), None)?;
        runtime.send_peer(&record.peer_destination, &reply).await?;
        store.remove(id)?;
        self.record_human_answer(&record, text);
        Ok(())
    }

    /// The human's `.mesh answer` to `record` went to the peer with no live run. The
    /// reply lands in the inbox, minted like the envoy's, so the model reads what the
    /// peer heard; one note supersedes the "wait for `.mesh answer`" one and the person
    /// at the keyboard gets one line.
    fn record_human_answer(&self, record: &InboundRecord, text: &str) {
        let runtime = self.get();
        let reply = PeerMessage::new(RawPeerMessage {
            source_identity: runtime
                .as_ref()
                .map(|runtime| runtime.fingerprint().to_string())
                .unwrap_or_default(),
            source_destination: runtime
                .as_ref()
                .map(|runtime| runtime.current_destination_hash())
                .unwrap_or_default(),
            destination: record.peer_destination.clone(),
            title: None,
            content: text.to_string(),
            fields: None,
            timestamp: unix_now(),
            message_id: uuid::Uuid::new_v4().simple().to_string(),
            in_reply_to: Some(record.id.clone()),
            kind: PeerKind::Reply,
            via: PeerVia::Direct,
        });
        let dest8 = short(&record.peer_destination).to_string();
        let text = format!(
            "you answered {dest8}: {}",
            first_words(&reply, PEER_LINE_MAX_CHARS)
        );
        self.peer_inbox.deliver(reply);
        self.model_notes.push(mesh_notification(
            "peer_message",
            &format!("peer:{dest8}"),
            "mesh",
            true,
            "the human answered via `.mesh answer`; nothing to do".to_string(),
        ));
        self.push_idle(IdleNotify {
            source: Source::Message,
            origin: Origin::Peer(short(&record.peer_identity).to_string()),
            text,
            model_note: None,
        });
    }

    /// Matches a reply to the question of ours it answers. A reply that answers nothing
    /// is downgraded to a message, since to this node it is one; `true` when it matched.
    fn answer_correlation(&self, message: &mut PeerMessage) -> bool {
        let answered = message.kind == PeerKind::Reply
            && message
                .in_reply_to
                .as_deref()
                .is_some_and(|id| self.correlations.answer(id, message.clone()));
        if message.kind == PeerKind::Reply && !answered {
            debug!(
                "Mesh reply {} from {} answers no open question of ours; delivering it as a message",
                message.message_id,
                short(&message.source_identity)
            );
            message.kind = PeerKind::Message;
        }
        answered
    }

    fn display_name_at(&self, destination: &str) -> Option<String> {
        self.get()
            .and_then(|runtime| runtime.peers().get(destination))
            .and_then(|peer| peer.display_name)
    }

    fn display_name_of(&self, message: &PeerMessage) -> Option<String> {
        self.display_name_at(&message.source_destination)
    }

    /// The sender as `summary_line` names it: the peer table's display name, cleaned,
    /// or the short hash of its instance.
    pub(crate) fn peer_name(&self, message: &PeerMessage) -> String {
        self.display_name_of(message)
            .and_then(|name| display_text(&name, DISPLAY_NAME_MAX_CHARS))
            .unwrap_or_else(|| short(&message.source_destination).to_string())
    }

    /// The sender as a refusal line names it, the same on every path a refusal takes:
    /// the peer table's display name for its instance, cleaned, or the short hash of
    /// its identity.
    fn peer_label(&self, identity: &str, destination: &str) -> String {
        self.display_name_at(destination)
            .and_then(|name| display_text(&name, DISPLAY_NAME_MAX_CHARS))
            .unwrap_or_else(|| short(identity).to_string())
    }

    /// The inbox path: the message lands in the inbox, the model is told what arrived
    /// and what to call, and the person at the keyboard gets one line. The id in the
    /// model's note is minted here from the sending instance, never the peer's own: an
    /// answered question is named by our correlation id, anything else (a message, an
    /// ask or a bulletin, each its own event) by `peer:<instance>`, so no peer-chosen
    /// text reaches the note.
    fn deliver_to_inbox(&self, message: PeerMessage, answered: bool) {
        let id8 = short(&message.source_identity).to_string();
        let text = message.summary_line(self.display_name_of(&message).as_deref());
        let local_id = format!("peer:{}", short(&message.source_destination));
        let (event, id, next_action) = match (answered, message.kind) {
            (true, _) => {
                let id = message.in_reply_to.clone().unwrap_or_default();
                let next_action = collect_next_action(&id);
                ("peer_reply", id, next_action)
            }
            (false, PeerKind::Bulletin) => (
                "peer_bulletin",
                local_id,
                CHECK_INBOX_NEXT_ACTION.to_string(),
            ),
            (false, PeerKind::Ask) => ("peer_ask", local_id, CHECK_INBOX_NEXT_ACTION.to_string()),
            (false, _) => (
                "peer_message",
                local_id,
                CHECK_INBOX_NEXT_ACTION.to_string(),
            ),
        };
        self.peer_inbox.deliver(message);
        self.model_notes
            .push(mesh_notification(event, &id, "mesh", true, next_action));
        let source = if answered {
            Source::Reply
        } else {
            Source::Message
        };
        self.push_idle(IdleNotify {
            source,
            origin: Origin::Peer(id8),
            text,
            model_note: None,
        });
    }
}

fn non_blank(value: Option<String>) -> Option<Arc<String>> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(Arc::new)
}

fn node_facts(runtime: &MeshRuntime) -> NodeFacts {
    NodeFacts {
        instance_id: runtime.current_instance_id(),
        destination: runtime.current_destination_hash(),
        identity: runtime.fingerprint().to_string(),
        interfaces: runtime.interface_kinds(),
    }
}

/// What `mesh.message.received` and `mesh.bulletin.received` say about a message, taken
/// before it is handed on, since that consumes it. Never its content or fields.
struct ReceivedFacts {
    kind: PeerKind,
    id: String,
    in_reply_to: Option<String>,
    identity: String,
    destination: String,
    title: Option<String>,
    via: PeerVia,
}

impl ReceivedFacts {
    fn of(message: &PeerMessage) -> Self {
        Self {
            kind: message.kind,
            id: message.message_id.clone(),
            in_reply_to: message.in_reply_to.clone(),
            identity: message.source_identity.clone(),
            destination: message.source_destination.clone(),
            title: message.title.clone(),
            via: message.via,
        }
    }

    fn event(self, routed: Routed) -> MeshEvent {
        match self.kind {
            PeerKind::Bulletin => MeshEvent::BulletinReceived {
                id: self.id,
                identity: self.identity,
                destination: self.destination,
                title: self.title,
                via: self.via,
            },
            PeerKind::Message | PeerKind::Ask | PeerKind::Reply => MeshEvent::MessageReceived {
                kind: self.kind,
                id: self.id,
                in_reply_to: self.in_reply_to,
                identity: self.identity,
                destination: self.destination,
                title: self.title,
                via: self.via,
                routed,
            },
        }
    }
}

/// The words `summary_line` shows for `message`, cut to `max_chars`.
fn first_words(message: &PeerMessage, max_chars: usize) -> String {
    let words = if message.content.is_empty() {
        message.title.as_deref().unwrap_or("(no text)")
    } else {
        &message.content
    };
    display_text(words, max_chars).unwrap_or_default()
}

/// The REPL lines for one refusal of `reason` from `who`, as `note_refusal` folds it:
/// the rolled-over counts first, then the one line the first refusal of the hour earns.
fn fold_lines(who: &str, reason: RefusalReason, via: PeerVia, notice: &FoldNotice) -> Vec<String> {
    let mut lines = fold_report_lines(who, &notice.reports);
    if notice.surface {
        lines.push(format!(
            "{who}: {}; further {} refusals from this peer are folded for the hour",
            refusal_phrase(reason, via),
            reason.as_str()
        ));
    }
    lines
}

fn fold_report_lines(who: &str, reports: &[(RefusalReason, u32)]) -> Vec<String> {
    reports
        .iter()
        .map(|(reason, count)| {
            let (plural, verb) = if *count == 1 {
                ("", "was")
            } else {
                ("s", "were")
            };
            format!(
                "{count} more {} refusal{plural} from {who} {verb} folded in the last hour",
                reason.as_str()
            )
        })
        .collect()
}

fn refusal_phrase(reason: RefusalReason, via: PeerVia) -> &'static str {
    match (reason, via) {
        (RefusalReason::RateLimited, PeerVia::Direct) => {
            "over the hourly message limit, refused on its link"
        }
        (RefusalReason::RateLimited, PeerVia::StoreAndForward) => {
            "over the hourly message limit; arrived store-and-forward, the peer is told once an hour"
        }
        (RefusalReason::EnvoyBusy, _) => "the envoy queue is full, filed in the inbox",
        (RefusalReason::EnvoyStopping, _) => "the envoy is stopping, filed in the inbox",
        (RefusalReason::PeerConcurrency, _) => {
            "already has a message with the envoy, filed in the inbox"
        }
        (RefusalReason::TokenCeiling, _) => "over its hourly token ceiling, filed in the inbox",
        (RefusalReason::CostCeiling, _) => "over its hourly cost ceiling, filed in the inbox",
        (RefusalReason::LoopGuard, _) => {
            "sent a reply the envoy will not answer, filed in the inbox"
        }
    }
}

/// The questions `store` holds open or answered and uncollected. A file this Coyote
/// cannot read is logged with its remedy and read as empty, so the node serves either
/// way; a late reply to one of the forgotten questions lands as an ordinary message.
fn reopen_pending(store: &PendingStore) -> Vec<PendingRecord> {
    match store.load_pending(SystemTime::now()) {
        Ok(pending) => {
            if !pending.is_empty() {
                debug!("Reopened {} pending mesh questions", pending.len());
            }
            pending
        }
        Err(err) => {
            warn!(
                "Mesh pending questions could not be reopened, so none are waiting on a reply: {err:#}"
            );
            Vec::new()
        }
    }
}

impl KnockSurface for MeshSlot {
    fn surface(&self, note: IdleNotify) -> bool {
        self.push_idle(note)
    }
}

impl PeerSurface for MeshSlot {
    /// A reply to a question this Coyote asked is admitted without being counted: a peer
    /// past its limit must still be able to answer a `mesh__ask`, and the correlation
    /// only accepts the one reply, from the identity it was asked of, so the exemption
    /// is spent with it. A refusal on the store-and-forward path also earns the
    /// peer one typed reply per identity, per reason, per hour, since no link carries a
    /// code back; on a link the caller's code is the typed refusal. A refused reply
    /// neither earns nor spends one: a reply to a reply is the loop the envoy guards
    /// against. The REPL line is folded on its own count, so it prints whether or not the
    /// peer is told.
    fn admit_peer_message(&self, request: &PeerAdmission) -> Result<(), PeerRefusal> {
        if request.kind == PeerKind::Reply
            && let Some(id) = request.in_reply_to
            && self
                .correlations
                .accepts_reply_from(id, request.source_identity)
        {
            return Ok(());
        }
        let identity = request.source_identity;
        let who = self.peer_label(identity, request.source_destination);
        let now = Instant::now();
        match self.limits.admit_message(identity, now) {
            Ok(()) => {
                self.surface_fold_reports(identity, &who);
                Ok(())
            }
            Err(refusal) => {
                self.surface_refusal(identity, &refusal, &who, request.via);
                if request.via == PeerVia::StoreAndForward
                    && request.in_reply_to.is_none()
                    && self.limits.claim_peer_reply(identity, refusal.reason, now)
                {
                    self.send_refusal_reply(
                        request.source_destination,
                        request.message_id,
                        &refusal,
                    );
                }
                Err(refusal)
            }
        }
    }

    fn deliver_peer(&self, message: PeerMessage) {
        MeshSlot::deliver_peer(self, message);
    }

    fn file_peer(&self, mut message: PeerMessage) {
        let answered = self.answer_correlation(&mut message);
        self.deliver_to_inbox(message, answered);
    }

    fn local_destination(&self) -> Option<String> {
        self.get().map(|runtime| runtime.current_destination_hash())
    }
}

// Tests that start a runtime are unix-only because identity minting writes an owner-only
// file, which is implemented for unix alone so far.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mesh_config::MeshBrief;
    use crate::hooks::HookEvent;
    use crate::mesh::destination_address;
    use crate::mesh::events::{RecordingHookSink, env_value, one_fire};
    use crate::mesh::message::{
        PEER_ID_MAX_CHARS, is_received_reply, peer_lxmf_message, to_r3_body,
    };
    use crate::mesh::notify::RenderedNotification;
    use crate::mesh::peers::PEER_TTL;
    #[cfg(unix)]
    use crate::mesh::peers::PeerRecord;
    use crate::mesh::pending::{
        DEFAULT_COLLECT_TIMEOUT, INBOUND_RECORD_VERSION, InboundRecord, PENDING_RECORD_VERSION,
        PendingState,
    };
    use crate::mesh::propagation_fetch::InboundMessage;
    use crate::mesh::r3::{
        AdmittedRequest, Handler, NAME_HASH_LEN, PathHash, Reply, RequestId, SizeBranch,
    };
    use crate::mesh::rfc3339_utc;
    #[cfg(unix)]
    use crate::mesh::test_support::{
        PeerStub, loopback_relay, started_runtime, started_runtime_on,
    };
    use crate::mesh::test_support::{
        TempDir, TrustList, mesh_paths, private_config, snapshot_fixture,
    };
    use crate::supervisor::mailbox::EnvelopePayload;
    use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};
    use rns_transport::destination::link::LinkId;
    #[cfg(unix)]
    use rns_transport::iface::tcp_server::TcpServer;
    #[cfg(unix)]
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(unix)]
    use tokio::net::TcpListener;

    #[cfg(unix)]
    const POLL: Duration = Duration::from_millis(100);
    #[cfg(unix)]
    const INTEROP_TIMEOUT: Duration = Duration::from_secs(15);

    #[test]
    fn slot_publish_then_read_returns_the_latest_snapshot() {
        let slot = MeshSlot::default();
        assert!(slot.snapshot().is_none());

        let first = snapshot_fixture();
        let first_at = first.captured_at;
        slot.publish(first);
        assert_eq!(slot.snapshot().unwrap().captured_at, first_at);

        let mut second = snapshot_fixture();
        second.captured_at = first_at + Duration::from_secs(1);
        slot.publish(second);
        assert_eq!(
            slot.snapshot().unwrap().captured_at,
            first_at + Duration::from_secs(1)
        );
    }

    #[test]
    fn slot_stale_error_names_the_remedy_until_a_snapshot_lands() {
        let slot = MeshSlot::default();
        let err = slot.snapshot_or_stale_error().unwrap_err().to_string();
        assert!(err.contains("turn boundary"), "{err}");
        slot.publish(snapshot_fixture());
        assert!(slot.snapshot_or_stale_error().is_ok());
    }

    #[test]
    fn slot_override_and_brief_round_trip_and_clear() {
        let slot = MeshSlot::default();
        assert!(slot.objective_override().is_none());
        assert!(slot.digest().is_none());
        assert!(slot.user_brief().is_none());
        assert!(slot.brief().is_none());

        slot.set_objective_override(Some("review the mesh".into()));
        assert_eq!(
            slot.objective_override().unwrap().as_str(),
            "review the mesh"
        );
        slot.set_objective_override(None);
        assert!(slot.objective_override().is_none());

        slot.publish(snapshot_fixture());
        slot.publish_digest(Some(Digest {
            text: "- Working on the mesh".into(),
            generated_at: SystemTime::now(),
            covered_messages: 4,
        }));
        assert_eq!(slot.digest().unwrap().covered_messages, 4);
        let brief = slot.brief().unwrap();
        let heading = format!(
            "## Digest (as of {})\n- Working on the mesh",
            rfc3339_utc(slot.digest().unwrap().generated_at)
        );
        assert!(brief.text.contains(&heading), "{}", brief.text);
        assert_eq!(
            brief.digest_generated_at,
            Some(slot.digest().unwrap().generated_at)
        );

        slot.set_objective_override(Some("override goal".into()));
        let brief = slot.brief().unwrap();
        assert!(
            brief.text.contains("Objective: override goal"),
            "{}",
            brief.text
        );
        slot.set_objective_override(None);
        let brief = slot.brief().unwrap();
        assert!(brief.text.contains("Objective: ship it"), "{}", brief.text);

        slot.set_user_brief(Some("Ask before merging".into()));
        assert_eq!(slot.user_brief().unwrap().as_str(), "Ask before merging");
        let brief = slot.brief().unwrap();
        assert!(
            brief
                .text
                .contains("## Note from the user\nAsk before merging"),
            "{}",
            brief.text
        );

        slot.publish_digest(None);
        assert!(slot.digest().is_none());
        let brief = slot.brief().unwrap();
        assert!(!brief.text.contains("## Digest"), "{}", brief.text);
        assert_eq!(brief.digest_generated_at, None);
        slot.set_user_brief(None);
        assert!(slot.user_brief().is_none());
        assert!(!slot.brief().unwrap().text.contains("## Note from the user"));
    }

    #[test]
    fn slot_refuses_a_digest_from_a_closed_epoch() {
        let slot = MeshSlot::default();
        slot.publish(snapshot_fixture());
        let digest = Digest {
            text: "- Working on the mesh".into(),
            generated_at: SystemTime::now(),
            covered_messages: 4,
        };
        let epoch = slot.digest_epoch();
        assert!(slot.publish_digest_at(epoch, digest.clone()));
        assert!(slot.brief().unwrap().text.contains("## Digest (as of "));

        let next = slot.clear_digest_for_new_epoch();
        assert_eq!(next, epoch + 1);
        assert_eq!(slot.digest_epoch(), next);
        assert!(slot.digest().is_none());
        assert!(!slot.brief().unwrap().text.contains("## Digest"));

        assert!(!slot.publish_digest_at(epoch, digest.clone()));
        assert!(slot.digest().is_none());
        assert!(!slot.brief().unwrap().text.contains("## Digest"));

        assert!(slot.publish_digest_at(next, digest));
        assert_eq!(slot.digest().unwrap().covered_messages, 4);
    }

    #[test]
    fn an_unchanged_digest_keeps_its_age_anchor_across_publishes() {
        let slot = MeshSlot::default();
        slot.publish(snapshot_fixture());
        slot.publish_digest(Some(Digest {
            text: "- Working on the mesh".into(),
            generated_at: SystemTime::now() - Duration::from_secs(30),
            covered_messages: 4,
        }));
        let first = slot.brief().unwrap();
        let heading_line = |text: &str| {
            text.lines()
                .find(|line| line.starts_with("## Digest (as of "))
                .map(str::to_string)
                .unwrap_or_else(|| panic!("no digest heading in {text}"))
        };
        let first_heading = heading_line(&first.text);

        slot.publish(snapshot_fixture());
        let second = slot.brief().unwrap();
        assert_eq!(second.digest_generated_at, first.digest_generated_at);
        assert_eq!(heading_line(&second.text), first_heading);
    }

    #[test]
    fn slot_setters_trim_and_treat_blank_as_cleared() {
        let slot = MeshSlot::default();

        slot.set_objective_override(Some("  review the mesh \n".into()));
        assert_eq!(
            slot.objective_override().unwrap().as_str(),
            "review the mesh"
        );
        slot.set_objective_override(Some("  ".into()));
        assert!(slot.objective_override().is_none());

        slot.set_user_brief(Some("\tWorking on the mesh  ".into()));
        assert_eq!(slot.user_brief().unwrap().as_str(), "Working on the mesh");
        slot.set_user_brief(Some(" \n".into()));
        assert!(slot.user_brief().is_none());
    }

    #[test]
    fn publish_reassembles_the_brief() {
        let slot = MeshSlot::default();
        slot.set_user_brief(Some("Ask before merging".into()));
        assert!(
            slot.brief().is_none(),
            "no snapshot means no mode to honour, so nothing is served"
        );

        slot.publish(snapshot_fixture());
        let brief = slot.brief().unwrap();
        assert!(
            brief
                .text
                .starts_with("## Status\nObjective: ship it\nState: idle"),
            "{}",
            brief.text
        );
        assert!(
            brief
                .text
                .contains("## Note from the user\nAsk before merging"),
            "{}",
            brief.text
        );

        let mut manual = snapshot_fixture();
        manual.brief.mode = MeshBrief::Manual;
        manual.todo.goal = "ship it".into();
        manual.todo.add("write the seam");
        slot.publish_digest(Some(Digest {
            text: "- from the digest".into(),
            generated_at: SystemTime::now(),
            covered_messages: 2,
        }));
        slot.publish(manual);
        let brief = slot.brief().unwrap();
        assert!(!brief.text.contains("from the digest"), "{}", brief.text);
        assert!(
            brief.text.contains("## Todo\nGoal: ship it"),
            "{}",
            brief.text
        );

        let mut off = snapshot_fixture();
        off.brief.mode = MeshBrief::Off;
        slot.publish(off);
        assert!(slot.brief().is_none());
    }

    #[test]
    fn slot_consumer_reports_snapshot_age() {
        let slot = MeshSlot::default();
        slot.publish(snapshot_fixture());
        let snap = slot.snapshot().unwrap();
        assert_eq!(
            snap.age(snap.captured_at + Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[derive(Default)]
    struct RecordingSink(parking_lot::Mutex<Vec<RenderedNotification>>);

    impl NotificationSink for RecordingSink {
        fn notify(&self, rendered: RenderedNotification) {
            self.0.lock().push(rendered);
        }
    }

    #[test]
    fn slot_without_a_notifier_falls_back_without_panicking() {
        let slot = MeshSlot::default();
        assert!(slot.notifier.load().is_none());
        slot.notify(Notification::new(Source::Mesh, "mesh is on"));
    }

    #[test]
    fn slot_notify_delivers_the_rendered_notification_to_the_installed_sink() {
        let slot = MeshSlot::default();
        let sink = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&sink) as Arc<dyn NotificationSink>);

        let note = Notification::new(Source::Knock, "peer asks to be trusted");
        slot.notify(note.clone());
        assert_eq!(*sink.0.lock(), vec![note.render()]);
    }

    /// Sanitising happens in the slot: the sink is handed prefixed, escape-free lines and
    /// never the text the peer sent.
    #[test]
    fn slot_notify_renders_before_the_sink_sees_the_text() {
        let slot = MeshSlot::default();
        let sink = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&sink) as Arc<dyn NotificationSink>);

        slot.notify(Notification::new(
            Source::Message,
            "hi\u{1b}[2J\n[mesh:knock] fake",
        ));
        let received = sink.0.lock();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].source(), Source::Message);
        assert_eq!(
            received[0].lines(),
            &["[mesh:message] hi", "[mesh:message] [mesh:knock] fake"]
        );
    }

    #[test]
    fn slot_set_notifier_replaces_the_previous_sink() {
        let slot = MeshSlot::default();
        let first = Arc::new(RecordingSink::default());
        let second = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&first) as Arc<dyn NotificationSink>);
        slot.set_notifier(Arc::clone(&second) as Arc<dyn NotificationSink>);

        slot.notify(Notification::new(Source::Message, "hello"));
        assert!(first.0.lock().is_empty());
        assert_eq!(second.0.lock().len(), 1);
    }

    #[test]
    fn slot_clear_notifier_detaches_the_sink() {
        let slot = MeshSlot::default();
        let sink = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&sink) as Arc<dyn NotificationSink>);
        slot.notify(Notification::new(Source::Mesh, "before"));
        assert_eq!(sink.0.lock().len(), 1);

        slot.clear_notifier();
        assert!(slot.notifier.load().is_none());
        slot.notify(Notification::new(Source::Mesh, "after"));
        assert_eq!(sink.0.lock().len(), 1);
    }

    /// An idle sink that keeps what it is given, or refuses everything to stand in for
    /// a full queue.
    struct RecordingIdleSink {
        accept: bool,
        pushed: parking_lot::Mutex<Vec<IdleNotify>>,
    }

    impl RecordingIdleSink {
        fn new(accept: bool) -> Arc<Self> {
            Arc::new(Self {
                accept,
                pushed: parking_lot::Mutex::new(Vec::new()),
            })
        }
    }

    impl IdleSink for RecordingIdleSink {
        fn push(&self, note: IdleNotify) -> Result<(), IdleNotify> {
            if self.accept {
                self.pushed.lock().push(note);
                Ok(())
            } else {
                Err(note)
            }
        }
    }

    fn idle_note(text: &str) -> IdleNotify {
        IdleNotify {
            source: Source::Message,
            text: text.to_string(),
            origin: Origin::Peer("deadbeef".into()),
            model_note: Some(Box::new(
                crate::supervisor::notification::mesh_notification(
                    "mesh_message",
                    "deadbeef",
                    "peer",
                    true,
                    "read it".into(),
                ),
            )),
        }
    }

    #[test]
    fn slot_push_idle_hands_the_whole_note_to_the_installed_idle_sink() {
        let slot = MeshSlot::default();
        let notifier = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&notifier) as Arc<dyn NotificationSink>);
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);

        assert!(slot.push_idle(idle_note("hello")));
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].text, "hello");
        assert!(pushed[0].model_note.is_some());
        assert!(notifier.0.lock().is_empty());
    }

    #[test]
    fn slot_push_idle_without_a_driver_keeps_the_human_line_and_drops_the_model_note() {
        let slot = MeshSlot::default();
        let notifier = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&notifier) as Arc<dyn NotificationSink>);

        assert!(slot.push_idle(idle_note("hello")));
        let received = notifier.0.lock();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].lines(), &["[mesh:message] hello"]);
    }

    #[test]
    fn slot_push_idle_drops_the_note_when_the_driver_queue_is_full() {
        let slot = MeshSlot::default();
        let notifier = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&notifier) as Arc<dyn NotificationSink>);
        let idle = RecordingIdleSink::new(false);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);

        assert!(
            !slot.push_idle(idle_note("hello")),
            "the caller must learn the line was dropped"
        );
        assert!(idle.pushed.lock().is_empty());
        assert!(
            notifier.0.lock().is_empty(),
            "a full driver queue must not become a path around the rate limiter"
        );
    }

    #[test]
    fn slot_clear_idle_routes_later_pushes_to_the_notifier() {
        let slot = MeshSlot::default();
        let notifier = Arc::new(RecordingSink::default());
        slot.set_notifier(Arc::clone(&notifier) as Arc<dyn NotificationSink>);
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);

        slot.clear_idle();
        slot.push_idle(idle_note("hello"));
        assert!(idle.pushed.lock().is_empty());
        assert_eq!(notifier.0.lock().len(), 1);
    }

    const PEER_IDENTITY: [u8; 16] = [0xcd; 16];
    const PEER_INSTANCE: [u8; 16] = [0xab; 16];

    fn peer_message(kind: PeerKind, id: &str, in_reply_to: Option<&str>) -> PeerMessage {
        PeerMessage {
            source_identity: hex_lower(&PEER_IDENTITY),
            source_destination: hex_lower(&PEER_INSTANCE),
            destination: hex_lower(&[0x01; 16]),
            title: None,
            content: format!("words of {id}"),
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: id.to_string(),
            in_reply_to: in_reply_to.map(str::to_string),
            kind,
            via: PeerVia::Direct,
        }
    }

    fn pending(id: &str) -> PendingRecord {
        let now = SystemTime::now();
        PendingRecord {
            version: PENDING_RECORD_VERSION,
            id: id.to_string(),
            peer_destination: hex_lower(&PEER_INSTANCE),
            peer_identity: hex_lower(&PEER_IDENTITY),
            question: "what now".to_string(),
            sent_at: rfc3339_utc(now),
            timeout_at: rfc3339_utc(now + DEFAULT_COLLECT_TIMEOUT),
            state: PendingState::Open,
            reply: None,
        }
    }

    fn peer_ids(envelopes: &[crate::supervisor::mailbox::Envelope]) -> Vec<&str> {
        envelopes
            .iter()
            .map(|envelope| match &envelope.payload {
                EnvelopePayload::Peer(message) => message.message_id.as_str(),
                other => panic!("not a peer envelope: {other:?}"),
            })
            .collect()
    }

    fn peer_of(envelope: &crate::supervisor::mailbox::Envelope) -> &PeerMessage {
        match &envelope.payload {
            EnvelopePayload::Peer(message) => message,
            other => panic!("not a peer envelope: {other:?}"),
        }
    }

    #[test]
    fn deliver_peer_on_a_bare_slot_lands_in_the_inbox_and_queues_a_model_note_and_a_line() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        slot.correlations().open(pending("q-1")).unwrap();

        slot.deliver_peer(peer_message(PeerKind::Message, "m-1", None));
        slot.deliver_peer(peer_message(PeerKind::Bulletin, "b-1", None));
        slot.deliver_peer(peer_message(PeerKind::Ask, "a-1", None));
        slot.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));
        slot.deliver_peer(peer_message(PeerKind::Reply, "r-2", Some("q-unknown")));

        let (envelopes, dropped) = slot.peer_inbox().drain();
        assert_eq!(dropped, 0);
        assert_eq!(peer_ids(&envelopes), ["m-1", "b-1", "a-1", "r-1", "r-2"]);
        assert_eq!(envelopes[0].from, hex_lower(&PEER_INSTANCE));
        assert_eq!(envelopes[0].to, hex_lower(&[0x01; 16]));

        let notes = slot.take_model_notes();
        let summary: Vec<(&str, &str, &str)> = notes
            .iter()
            .map(|note| (note.event, note.id.as_str(), note.next_action.as_str()))
            .collect();
        let peer_id = format!("peer:{}", &hex_lower(&PEER_INSTANCE)[..8]);
        assert_eq!(
            summary,
            [
                ("peer_message", peer_id.as_str(), "mesh__check_inbox"),
                ("peer_bulletin", peer_id.as_str(), "mesh__check_inbox"),
                ("peer_ask", peer_id.as_str(), "mesh__check_inbox"),
                ("peer_reply", "q-1", "mesh__collect --id q-1"),
                ("peer_message", peer_id.as_str(), "mesh__check_inbox"),
            ],
            "a reply to nothing this node asked is an ordinary message"
        );
        assert!(
            notes
                .iter()
                .all(|note| note.tool_or_agent == "mesh" && note.status == "success")
        );
        assert!(slot.take_model_notes().is_empty());

        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 5);
        let dest8 = &hex_lower(&PEER_INSTANCE)[..8];
        for note in pushed.iter() {
            assert_eq!(
                note.origin,
                Origin::Peer(hex_lower(&PEER_IDENTITY)[..8].to_string())
            );
            assert!(
                note.model_note.is_none(),
                "the model's copy waits in the slot's notes, not on the idle line"
            );
        }
        let sources: Vec<Source> = pushed.iter().map(|note| note.source).collect();
        assert_eq!(
            sources,
            [
                Source::Message,
                Source::Message,
                Source::Message,
                Source::Reply,
                Source::Message
            ],
            "only a reply that answers an open question is a reply line"
        );
        assert_eq!(pushed[0].text, format!("{dest8} says: words of m-1"));
        assert_eq!(pushed[1].text, format!("{dest8} announces: words of b-1"));
        assert_eq!(pushed[2].text, format!("{dest8} asks: words of a-1"));
        assert_eq!(pushed[3].text, format!("{dest8} replies: words of r-1"));
        drop(pushed);
        assert_eq!(
            slot.correlations()
                .take_answer("q-1")
                .map(|reply| reply.message_id),
            Some("r-1".to_string())
        );
    }

    fn peer_payload(envelope: &crate::supervisor::mailbox::Envelope) -> &PeerMessage {
        match &envelope.payload {
            EnvelopePayload::Peer(message) => message,
            other => panic!("not a peer envelope: {other:?}"),
        }
    }

    /// An envoy that keeps every job it is offered, or refuses them all with one reason
    /// (a full queue unless told otherwise); answers are consumed or not as configured.
    struct RecordingEnvoy {
        refusal: Option<PeerRefusal>,
        consume_answers: bool,
        jobs: parking_lot::Mutex<Vec<PeerMessage>>,
        answers: parking_lot::Mutex<Vec<(String, String)>>,
        interrupts: AtomicUsize,
    }

    impl RecordingEnvoy {
        fn new(accept: bool, consume_answers: bool) -> Arc<Self> {
            let refusal = (!accept).then(|| PeerRefusal::capacity(RefusalReason::EnvoyBusy));
            Self::with_refusal(refusal, consume_answers)
        }

        fn refusing(reason: RefusalReason) -> Arc<Self> {
            Self::with_refusal(Some(PeerRefusal::capacity(reason)), false)
        }

        fn with_refusal(refusal: Option<PeerRefusal>, consume_answers: bool) -> Arc<Self> {
            Arc::new(Self {
                refusal,
                consume_answers,
                jobs: parking_lot::Mutex::new(Vec::new()),
                answers: parking_lot::Mutex::new(Vec::new()),
                interrupts: AtomicUsize::new(0),
            })
        }

        fn job_ids(&self) -> Vec<String> {
            self.jobs
                .lock()
                .iter()
                .map(|message| message.message_id.clone())
                .collect()
        }
    }

    impl EnvoySink for RecordingEnvoy {
        fn accept(&self, job: EnvoyJob) -> Result<(), PeerRefusal> {
            if let Some(refusal) = &self.refusal {
                return Err(refusal.clone());
            }
            self.jobs.lock().push(job.message);
            Ok(())
        }

        fn answer(&self, id: &str, text: &str) -> bool {
            self.answers.lock().push((id.to_string(), text.to_string()));
            self.consume_answers
        }

        fn interrupt(&self) {
            self.interrupts.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn an_accepting_envoy_takes_messages_and_asks_and_nothing_else() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        slot.correlations().open(pending("q-1")).unwrap();

        slot.deliver_peer(peer_message(PeerKind::Message, "m-1", None));
        slot.deliver_peer(peer_message(PeerKind::Ask, "a-1", None));
        slot.deliver_peer(peer_message(PeerKind::Bulletin, "b-1", None));
        slot.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));
        slot.deliver_peer(peer_message(PeerKind::Reply, "r-2", Some("q-unknown")));

        assert_eq!(
            envoy.job_ids(),
            ["m-1", "a-1"],
            "a reply to nothing we asked is a message, but one that never reaches the envoy"
        );
        let (envelopes, dropped) = slot.peer_inbox().drain();
        assert_eq!(dropped, 0);
        assert_eq!(
            peer_ids(&envelopes),
            ["b-1", "r-1", "r-2"],
            "a bulletin, an answer to our question and a stray reply take the inbox path"
        );
        assert_eq!(peer_of(&envelopes[2]).kind, PeerKind::Message);
        let notes = slot.take_model_notes();
        let events: Vec<&str> = notes.iter().map(|note| note.event).collect();
        assert_eq!(events, ["peer_bulletin", "peer_reply", "peer_message"]);
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 3);
        assert!(pushed.iter().all(|note| !note.text.contains("envoy")));
    }

    #[test]
    fn a_wire_reply_that_answers_nothing_never_reaches_the_envoy() {
        let slot = MeshSlot::default();
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);

        slot.deliver_peer(peer_message(PeerKind::Message, "m-2", Some("whatever")));
        slot.deliver_peer(peer_message(PeerKind::Reply, "r-3", Some("unknown")));

        assert!(envoy.job_ids().is_empty(), "{:?}", envoy.job_ids());
        let (envelopes, dropped) = slot.peer_inbox().drain();
        assert_eq!(dropped, 0);
        assert_eq!(peer_ids(&envelopes), ["m-2", "r-3"]);
    }

    fn assert_no_content(envs: &[(&'static str, String)], message: &PeerMessage) {
        for (key, value) in envs {
            assert!(
                !value.contains(&message.content) && !message.content.contains(value.as_str()),
                "{key}={value} carries the content"
            );
        }
    }

    #[test]
    fn deliver_peer_fires_message_received_with_where_it_went_and_never_the_content() {
        let slot = MeshSlot::default();
        let sink = RecordingHookSink::attach(&slot.hooks());
        let mut message = peer_message(PeerKind::Ask, "a-1", None);
        message.title = Some("plan".to_string());
        message.content = "the secret words of the peer".to_string();
        message.via = PeerVia::StoreAndForward;

        slot.deliver_peer(message.clone());

        let envs = one_fire(&sink, HookEvent::MeshMessageReceived);
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_KIND"), Some("ask"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_ID"), Some("a-1"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_IN_REPLY_TO"), None);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(hex_lower(&PEER_IDENTITY).as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(hex_lower(&PEER_INSTANCE).as_str())
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_TITLE"), Some("plan"));
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_VIA"),
            Some("store-and-forward")
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_ROUTED"), Some("inbox"));
        assert_no_content(&envs, &message);

        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        slot.deliver_peer(peer_message(PeerKind::Message, "m-1", None));
        let envs = one_fire(&sink, HookEvent::MeshMessageReceived);
        assert_eq!(env_value(&envs, "COYOTE_MESH_ROUTED"), Some("envoy"));
        assert_eq!(envoy.job_ids(), ["m-1"]);

        slot.correlations().open(pending("q-1")).unwrap();
        slot.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));
        let envs = one_fire(&sink, HookEvent::MeshMessageReceived);
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_KIND"), Some("reply"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_IN_REPLY_TO"), Some("q-1"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_ROUTED"), Some("inbox"));
    }

    #[test]
    fn deliver_peer_fires_bulletin_received_for_a_bulletin() {
        let slot = MeshSlot::default();
        let sink = RecordingHookSink::attach(&slot.hooks());
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        let mut message = peer_message(PeerKind::Bulletin, "b-1", None);
        message.content = "the secret words of the peer".to_string();

        slot.deliver_peer(message.clone());

        let envs = one_fire(&sink, HookEvent::MeshBulletinReceived);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_MESSAGE_KIND"),
            Some("bulletin")
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_ID"), Some("b-1"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_VIA"), Some("direct"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_ROUTED"), None);
        assert_no_content(&envs, &message);
        assert!(envoy.job_ids().is_empty());
    }

    #[test]
    fn deliver_peer_with_no_hook_sink_delivers_and_records_nothing() {
        let slot = MeshSlot::default();
        slot.deliver_peer(peer_message(PeerKind::Message, "m-1", None));
        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["m-1"]);
    }

    #[test]
    fn set_user_brief_fires_brief_updated_only_when_the_served_brief_changes() {
        let slot = MeshSlot::default();
        let sink = RecordingHookSink::attach(&slot.hooks());
        let note = "Ask before merging";

        slot.set_user_brief(Some(note.to_string()));
        assert!(
            sink.drain().is_empty(),
            "with no snapshot nothing is served, so nothing changed"
        );

        slot.publish(snapshot_fixture());
        sink.drain();
        slot.set_user_brief(Some(note.to_string()));
        assert!(sink.drain().is_empty(), "the same text changes nothing");

        slot.set_user_brief(Some("Merge freely".to_string()));
        let envs = one_fire(&sink, HookEvent::MeshBriefUpdated);
        assert_eq!(env_value(&envs, "COYOTE_MESH_BRIEF_SOURCE"), Some("user"));
        let served = slot.brief().unwrap().text.chars().count();
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_BRIEF_CHARS"),
            Some(served.to_string().as_str())
        );
        for (key, value) in &envs {
            assert!(!value.contains("Merge"), "{key}={value} carries the brief");
        }

        slot.set_user_brief(Some("Merge freely".to_string()));
        assert!(sink.drain().is_empty());
    }

    #[test]
    fn publish_digest_at_fires_brief_updated_from_the_digest_for_the_current_epoch_only() {
        let slot = MeshSlot::default();
        slot.publish(snapshot_fixture());
        let sink = RecordingHookSink::attach(&slot.hooks());
        let digest = Digest {
            text: "- Working on the mesh hooks".into(),
            generated_at: SystemTime::now(),
            covered_messages: 4,
        };
        let epoch = slot.digest_epoch();

        assert!(slot.publish_digest_at(epoch, digest.clone()));

        let envs = one_fire(&sink, HookEvent::MeshBriefUpdated);
        assert_eq!(env_value(&envs, "COYOTE_MESH_BRIEF_SOURCE"), Some("digest"));
        let served = slot.brief().unwrap().text.chars().count();
        assert!(served > 0);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_BRIEF_CHARS"),
            Some(served.to_string().as_str())
        );
        for (key, value) in &envs {
            assert!(
                !value.contains("mesh hooks"),
                "{key}={value} carries the digest"
            );
        }

        assert!(slot.publish_digest_at(epoch, digest.clone()));
        assert!(sink.drain().is_empty(), "the same digest changes nothing");

        assert!(!slot.publish_digest_at(epoch + 1, digest.clone()));
        assert!(!slot.publish_digest_at(epoch.wrapping_sub(1), digest));
        assert!(
            sink.drain().is_empty(),
            "a digest from another epoch is dropped"
        );
    }

    #[test]
    fn an_interim_message_naming_our_question_leaves_its_correlation_open() {
        let slot = MeshSlot::default();
        slot.correlations().open(pending("q-1")).unwrap();

        slot.deliver_peer(peer_message(PeerKind::Message, "n-1", Some("q-1")));

        assert!(slot.correlations().is_open("q-1"));
        assert!(slot.correlations().take_answer("q-1").is_none());
        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["n-1"]);
        assert_eq!(slot.take_model_notes()[0].event, "peer_message");

        slot.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));

        assert!(!slot.correlations().is_open("q-1"));
        assert_eq!(
            slot.correlations()
                .take_answer("q-1")
                .map(|reply| reply.content),
            Some("words of r-1".to_string())
        );
    }

    #[test]
    fn clearing_the_envoy_returns_messages_to_the_inbox_path() {
        let slot = MeshSlot::default();
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        slot.clear_envoy();

        slot.deliver_peer(peer_message(PeerKind::Ask, "a-1", None));

        assert!(envoy.job_ids().is_empty());
        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["a-1"]);
        assert_eq!(slot.take_model_notes()[0].event, "peer_ask");
    }

    #[test]
    fn a_refusing_envoy_leaves_the_inbox_path_as_it_was_plus_one_busy_line() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let envoy = RecordingEnvoy::new(false, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);

        slot.deliver_peer(peer_message(PeerKind::Ask, "a-1", None));

        assert!(envoy.job_ids().is_empty());
        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["a-1"]);
        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_ask");
        assert_eq!(notes[0].next_action, "mesh__check_inbox");
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 2, "the summary line and one busy line");
        let dest8 = &hex_lower(&PEER_INSTANCE)[..8];
        let id8 = &hex_lower(&PEER_IDENTITY)[..8];
        assert_eq!(pushed[0].text, format!("{dest8} asks: words of a-1"));
        assert_eq!(
            pushed[1].text,
            format!(
                "{id8}: the envoy queue is full, filed in the inbox; further envoy_busy refusals from this peer are folded for the hour"
            )
        );
        for note in pushed.iter() {
            assert_eq!(note.source, Source::Message);
            assert_eq!(
                note.origin,
                Origin::Peer(hex_lower(&PEER_IDENTITY)[..8].to_string())
            );
            assert!(note.model_note.is_none());
        }
    }

    /// Five refusals of one reason from one identity in an hour earn one REPL line; the
    /// originals all reach the inbox, and the peer's reply is not attempted with no
    /// runtime to send it (this test runs with no tokio runtime at all).
    #[test]
    fn repeated_envoy_refusals_from_one_peer_fold_to_one_line() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let envoy = RecordingEnvoy::refusing(RefusalReason::PeerConcurrency);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);

        for n in 0..5 {
            slot.deliver_peer(peer_message(PeerKind::Ask, &format!("a-{n}"), None));
        }

        assert!(envoy.job_ids().is_empty());
        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["a-0", "a-1", "a-2", "a-3", "a-4"]);
        let pushed = idle.pushed.lock();
        let folded: Vec<&str> = pushed
            .iter()
            .map(|note| note.text.as_str())
            .filter(|text| text.contains("already has a message with the envoy"))
            .collect();
        assert_eq!(folded.len(), 1, "{pushed:?}");
        assert_eq!(
            pushed.len(),
            6,
            "five summary lines and one folded refusal line: {pushed:?}"
        );
    }

    #[test]
    fn fold_lines_report_rolled_counts_then_the_first_refusal_of_the_hour() {
        let surfaced = FoldNotice {
            surface: true,
            reports: vec![
                (RefusalReason::RateLimited, 39),
                (RefusalReason::TokenCeiling, 1),
            ],
        };
        assert_eq!(
            fold_lines(
                "alice",
                RefusalReason::PeerConcurrency,
                PeerVia::Direct,
                &surfaced
            ),
            [
                "39 more rate_limited refusals from alice were folded in the last hour",
                "1 more token_ceiling refusal from alice was folded in the last hour",
                "alice: already has a message with the envoy, filed in the inbox; further peer_concurrency refusals from this peer are folded for the hour",
            ]
        );
        assert!(
            fold_lines(
                "alice",
                RefusalReason::RateLimited,
                PeerVia::Direct,
                &FoldNotice::default()
            )
            .is_empty()
        );
        let first = FoldNotice {
            surface: true,
            reports: Vec::new(),
        };
        for via in [PeerVia::Direct, PeerVia::StoreAndForward] {
            for reason in RefusalReason::ALL {
                let lines = fold_lines("cdcdcdcd", reason, via, &first);
                assert_eq!(lines.len(), 1);
                assert!(lines[0].starts_with("cdcdcdcd: "), "{}", lines[0]);
                assert!(
                    lines[0].contains(&format!("further {} refusals", reason.as_str())),
                    "{}",
                    lines[0]
                );
                assert!(lines[0].is_ascii(), "{}", lines[0]);
            }
        }
        assert_eq!(
            fold_lines("alice", RefusalReason::RateLimited, PeerVia::Direct, &first),
            [
                "alice: over the hourly message limit, refused on its link; further rate_limited refusals from this peer are folded for the hour"
            ]
        );
        assert_eq!(
            fold_lines(
                "alice",
                RefusalReason::RateLimited,
                PeerVia::StoreAndForward,
                &first
            ),
            [
                "alice: over the hourly message limit; arrived store-and-forward, the peer is told once an hour; further rate_limited refusals from this peer are folded for the hour"
            ]
        );
    }

    /// Both ways a peer message arrives, off a link through the `/message` provider and
    /// off a propagation node through `PeerRouting`, end in the same `deliver_peer`, so
    /// the envoy sees both.
    #[tokio::test]
    async fn both_inbound_entry_points_route_to_the_envoy() {
        let slot = Arc::new(MeshSlot::default());
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        let identity = TransportIdentity::new_from_rand(OsRng);
        let identity_hex = identity.address_hash().to_hex_string();
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let destination = destination_address(&origin.0, identity.address_hash()).to_hex_string();

        let handler = PeerMessageHandler::new(Arc::downgrade(&slot) as Weak<dyn PeerSurface>);
        let direct = OutboundPeer::new(PeerKind::Ask, "over the link", None, None, None).unwrap();
        let reply = handler
            .handle(AdmittedRequest {
                link_id: LinkId::new_from_rand(OsRng),
                identity: *identity.as_identity(),
                destination_hash: AddressHash::new_from_hex_string(&destination).unwrap(),
                request_id: RequestId::from([1u8; 16]),
                path_hash: PathHash::of(MESSAGE_PATH),
                requested_at: 1_700_000_000.0,
                body: to_r3_body(&direct, 1_700_000_000.0),
                branch: SizeBranch::Packet,
            })
            .await;
        match reply {
            Reply::Value(value) => assert!(is_received_reply(&value, &direct.id), "{value}"),
            Reply::Code(code) => panic!("refused: {code:?}"),
            Reply::Silent => panic!("the message was not acknowledged"),
        }

        let (trust, _trust_dir) = TrustList::default()
            .destination(&destination, &identity_hex)
            .open("node-envoy-routing");
        let stored =
            OutboundPeer::new(PeerKind::Message, "from the node", None, None, None).unwrap();
        let lxmf = peer_lxmf_message(&stored, &origin);
        let inner = NullSink;
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(slot.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        routing.deliver(InboundMessage {
            transient_id: [1u8; 32],
            message_id: [2u8; 32],
            source_identity_hash: identity_hex.clone(),
            source_delivery_hash: hex_lower(&[0x03; 16]),
            timestamp: 1_700_000_000.0,
            title: None,
            content: Some(lxmf.content),
            fields: lxmf.fields,
            stamp_value: None,
        });

        assert_eq!(envoy.job_ids(), [direct.id.as_str(), stored.id.as_str()]);
        let jobs = envoy.jobs.lock();
        assert_eq!(jobs[0].kind, PeerKind::Ask);
        assert_eq!(jobs[0].via, PeerVia::Direct);
        assert_eq!(jobs[0].source_destination, destination);
        assert_eq!(jobs[1].kind, PeerKind::Message);
        assert_eq!(jobs[1].via, PeerVia::StoreAndForward);
        assert_eq!(jobs[1].source_destination, destination);
        drop(jobs);
        assert!(slot.peer_inbox().drain().0.is_empty());
        assert!(slot.take_model_notes().is_empty());
    }

    struct NullSink;

    impl InboundSink for NullSink {
        fn deliver(&self, message: InboundMessage) {
            panic!("a peer message must not fall through to the plain inbox: {message:?}");
        }
    }

    /// An admitted `/message` request from `identity` at the instance `destination`.
    fn admitted_request(
        identity: &TransportIdentity,
        destination: &str,
        message: &OutboundPeer,
    ) -> AdmittedRequest {
        AdmittedRequest {
            link_id: LinkId::new_from_rand(OsRng),
            identity: *identity.as_identity(),
            destination_hash: AddressHash::new_from_hex_string(destination).unwrap(),
            request_id: RequestId::from([1u8; 16]),
            path_hash: PathHash::of(MESSAGE_PATH),
            requested_at: 1_700_000_000.0,
            body: to_r3_body(message, 1_700_000_000.0),
            branch: SizeBranch::Packet,
        }
    }

    /// A third message in an hour from one identity is refused on its link with
    /// `Throttled` before the envoy sees it, while another identity's first message is
    /// acknowledged; the refusal earns one REPL line, folded thereafter.
    #[tokio::test]
    async fn the_message_provider_throttles_the_third_message_in_an_hour_per_identity() {
        let slot = Arc::new(MeshSlot::default());
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        slot.limits().configure(PeerLimitConfig {
            messages_per_hour: 2,
            concurrency: 64,
            ..PeerLimitConfig::default()
        });
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let a = TransportIdentity::new_from_rand(OsRng);
        let a_destination = destination_address(&origin.0, a.address_hash()).to_hex_string();
        let b = TransportIdentity::new_from_rand(OsRng);
        let b_destination = destination_address(&origin.0, b.address_hash()).to_hex_string();
        let handler = PeerMessageHandler::new(Arc::downgrade(&slot) as Weak<dyn PeerSurface>);

        let mut acked = Vec::new();
        let mut throttled = 0;
        for n in 0..4 {
            let out =
                OutboundPeer::new(PeerKind::Ask, &format!("a {n}"), None, None, None).unwrap();
            match handler
                .handle(admitted_request(&a, &a_destination, &out))
                .await
            {
                Reply::Value(value) => {
                    assert!(is_received_reply(&value, &out.id), "{value}");
                    acked.push(out.id.clone());
                }
                Reply::Code(code) => {
                    assert_eq!(code, RefusalCode::Throttled);
                    throttled += 1;
                }
                Reply::Silent => panic!("message {n} was neither acknowledged nor refused"),
            }
        }
        assert_eq!(acked.len(), 2);
        assert_eq!(throttled, 2);
        assert_eq!(
            envoy.job_ids(),
            acked,
            "the refused messages never reach the envoy"
        );

        let from_b = OutboundPeer::new(PeerKind::Ask, "b 0", None, None, None).unwrap();
        assert!(matches!(
            handler
                .handle(admitted_request(&b, &b_destination, &from_b))
                .await,
            Reply::Value(_)
        ));
        assert_eq!(envoy.job_ids().len(), 3);
        assert!(slot.peer_inbox().drain().0.is_empty());
        let a8 = &a.address_hash().to_hex_string()[..8];
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        assert_eq!(
            pushed[0].text,
            format!(
                "{a8}: over the hourly message limit, refused on its link; further rate_limited refusals from this peer are folded for the hour"
            )
        );
        assert_eq!(pushed[0].origin, Origin::Peer(a8.to_string()));
    }

    /// The same limit on the store-and-forward path: the third propagated message in an
    /// hour is filed in the inbox and the envoy never sees it; the one typed reply the
    /// sender is owed has nowhere to go with the mesh off.
    #[test]
    fn peer_routing_files_the_third_message_in_an_hour_per_identity() {
        let slot = Arc::new(MeshSlot::default());
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        slot.limits().configure(PeerLimitConfig {
            messages_per_hour: 2,
            concurrency: 64,
            ..PeerLimitConfig::default()
        });
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let a = TransportIdentity::new_from_rand(OsRng);
        let a_hex = a.address_hash().to_hex_string();
        let a_destination = destination_address(&origin.0, a.address_hash()).to_hex_string();
        let b = TransportIdentity::new_from_rand(OsRng);
        let b_hex = b.address_hash().to_hex_string();
        let b_destination = destination_address(&origin.0, b.address_hash()).to_hex_string();
        let (trust, _trust_dir) = TrustList::default()
            .destination(&a_destination, &a_hex)
            .destination(&b_destination, &b_hex)
            .open("node-routing-throttle");
        let inner = NullSink;
        let routing = PeerRouting {
            trust: &trust,
            surface: Some(slot.clone() as Arc<dyn PeerSurface>),
            inner: &inner,
        };
        let propagated = |from: &str, content: &str| {
            let out = OutboundPeer::new(PeerKind::Message, content, None, None, None).unwrap();
            let lxmf = peer_lxmf_message(&out, &origin);
            (
                out.id,
                InboundMessage {
                    transient_id: [1u8; 32],
                    message_id: [2u8; 32],
                    source_identity_hash: from.to_string(),
                    source_delivery_hash: hex_lower(&[0x03; 16]),
                    timestamp: 1_700_000_000.0,
                    title: None,
                    content: Some(lxmf.content),
                    fields: lxmf.fields,
                    stamp_value: None,
                },
            )
        };

        let mut sent = Vec::new();
        for n in 0..3 {
            let (id, message) = propagated(&a_hex, &format!("a {n}"));
            routing.deliver(message);
            sent.push(id);
        }
        assert_eq!(
            envoy.job_ids(),
            sent[..2],
            "the third never reaches the envoy"
        );
        let (id, message) = propagated(&b_hex, "b 0");
        routing.deliver(message);
        assert_eq!(envoy.job_ids().len(), 3);
        assert_eq!(envoy.job_ids()[2], id);
        let (filed, dropped) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&filed), [sent[2].as_str()]);
        assert_eq!(dropped, 0);
        assert_eq!(
            idle.pushed
                .lock()
                .iter()
                .filter(|note| note.text.contains("over the hourly message limit"))
                .count(),
            1
        );
    }

    /// The store-and-forward path owes a refused sender one typed reply per identity,
    /// per reason, per hour; the link path owes none, since its code is the refusal.
    /// A refused reply is owed none either, since answering it would answer a reply,
    /// and it does not spend the hour's reply: the refused ask after it is still
    /// answered. With the mesh off the reply has nowhere to go, and the warning that
    /// says so is the one observable attempt.
    #[test]
    fn a_store_and_forward_refusal_is_answered_once_per_identity_per_reason_per_hour() {
        install_log_collector();
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        slot.limits().configure(PeerLimitConfig {
            messages_per_hour: 1,
            ..PeerLimitConfig::default()
        });
        let (a, a_instance) = (hex_lower(&[0x1a; 16]), hex_lower(&[0x2a; 16]));
        let (b, b_instance) = (hex_lower(&[0x1b; 16]), hex_lower(&[0x2b; 16]));
        let (c, c_instance) = (hex_lower(&[0x1c; 16]), hex_lower(&[0x2c; 16]));
        let admit = |identity: &str, instance: &str, id: &str, in_reply_to: bool, via: PeerVia| {
            PeerSurface::admit_peer_message(
                &slot,
                &PeerAdmission {
                    source_identity: identity,
                    source_destination: instance,
                    message_id: id,
                    kind: PeerKind::Message,
                    in_reply_to: in_reply_to.then_some("a-question-nobody-here-asked"),
                    via,
                },
            )
        };

        assert!(
            admit(
                &a,
                &a_instance,
                "stored-a-0",
                false,
                PeerVia::StoreAndForward
            )
            .is_ok()
        );
        for id in ["stored-a-1", "stored-a-2", "stored-a-3"] {
            assert_eq!(
                admit(&a, &a_instance, id, false, PeerVia::StoreAndForward)
                    .unwrap_err()
                    .reason,
                RefusalReason::RateLimited
            );
        }
        assert!(admit(&b, &b_instance, "link-b-0", false, PeerVia::Direct).is_ok());
        for id in ["link-b-1", "link-b-2"] {
            assert_eq!(
                admit(&b, &b_instance, id, false, PeerVia::Direct)
                    .unwrap_err()
                    .reason,
                RefusalReason::RateLimited
            );
        }
        assert!(
            admit(
                &c,
                &c_instance,
                "stored-c-0",
                false,
                PeerVia::StoreAndForward
            )
            .is_ok()
        );
        for (id, in_reply_to) in [("stored-c-1", true), ("stored-c-2", false)] {
            assert_eq!(
                admit(&c, &c_instance, id, in_reply_to, PeerVia::StoreAndForward)
                    .unwrap_err()
                    .reason,
                RefusalReason::RateLimited
            );
        }

        let warned = warn_snapshot();
        let attempts: Vec<&String> = warned
            .iter()
            .filter(|line| line.starts_with("Mesh refusal of "))
            .filter(|line| line.contains("-a-") || line.contains("-b-") || line.contains("-c-"))
            .collect();
        assert_eq!(
            attempts,
            [
                &format!(
                    "Mesh refusal of stored-a-1 to instance {} could not be sent: mesh is off",
                    short(&a_instance)
                ),
                &format!(
                    "Mesh refusal of stored-c-2 to instance {} could not be sent: mesh is off",
                    short(&c_instance)
                ),
            ],
            "{warned:#?}"
        );
        let texts: Vec<String> = idle
            .pushed
            .lock()
            .iter()
            .map(|note| note.text.clone())
            .filter(|text| text.contains("hourly message limit"))
            .collect();
        assert_eq!(
            texts,
            [
                format!(
                    "{}: over the hourly message limit; arrived store-and-forward, the peer is told once an hour; further rate_limited refusals from this peer are folded for the hour",
                    short(&a)
                ),
                format!(
                    "{}: over the hourly message limit, refused on its link; further rate_limited refusals from this peer are folded for the hour",
                    short(&b)
                ),
                format!(
                    "{}: over the hourly message limit; arrived store-and-forward, the peer is told once an hour; further rate_limited refusals from this peer are folded for the hour",
                    short(&c)
                ),
            ]
        );
    }

    #[test]
    fn a_reply_to_our_open_question_is_never_throttled() {
        let slot = MeshSlot::default();
        slot.limits().configure(PeerLimitConfig {
            messages_per_hour: 1,
            ..PeerLimitConfig::default()
        });
        slot.correlations().open(pending("q-ours")).unwrap();
        let identity = hex_lower(&PEER_IDENTITY);
        let instance = hex_lower(&PEER_INSTANCE);
        let admit_kind = |identity: &str, kind: PeerKind, id: &str, in_reply_to: Option<&str>| {
            PeerSurface::admit_peer_message(
                &slot,
                &PeerAdmission {
                    source_identity: identity,
                    source_destination: &instance,
                    message_id: id,
                    kind,
                    in_reply_to,
                    via: PeerVia::Direct,
                },
            )
        };
        let admit_as = |identity: &str, id: &str, in_reply_to: Option<&str>| {
            let kind = if in_reply_to.is_some() {
                PeerKind::Reply
            } else {
                PeerKind::Ask
            };
            admit_kind(identity, kind, id, in_reply_to)
        };
        let admit = |id: &str, in_reply_to: Option<&str>| admit_as(&identity, id, in_reply_to);

        assert!(admit("ask-0", None).is_ok());
        assert_eq!(
            admit("ask-1", None).unwrap_err().reason,
            RefusalReason::RateLimited,
            "the identity is past its limit"
        );
        assert_eq!(
            admit_kind(&identity, PeerKind::Ask, "ask-2", Some("q-ours"))
                .unwrap_err()
                .reason,
            RefusalReason::RateLimited,
            "an ask naming our open question is not a reply to it and is counted like any message"
        );
        assert!(
            slot.correlations().accepts_reply_from("q-ours", &identity),
            "the refused ask leaves the question open"
        );
        assert!(
            admit("reply-1", Some("q-ours")).is_ok(),
            "a reply to a question we asked is admitted regardless"
        );
        assert_eq!(
            admit("reply-2", Some("q-not-ours")).unwrap_err().reason,
            RefusalReason::RateLimited,
            "a reply to a question we never asked is counted like any message"
        );
        let other = hex_lower(&[0x77; 16]);
        assert!(admit_as(&other, "other-0", Some("q-ours")).is_ok());
        assert_eq!(
            admit_as(&other, "other-1", Some("q-ours"))
                .unwrap_err()
                .reason,
            RefusalReason::RateLimited,
            "citing our open question from another identity spends that identity's own count"
        );
        assert!(
            slot.correlations().answer(
                "q-ours",
                peer_message(PeerKind::Reply, "reply-1", Some("q-ours"))
            ),
            "the asked identity's reply closes the question"
        );
        assert_eq!(
            admit("reply-3", Some("q-ours")).unwrap_err().reason,
            RefusalReason::RateLimited,
            "once the question is answered a further reply citing it is counted like any message"
        );
    }

    #[test]
    fn record_envoy_exchange_files_both_sides_with_one_note_and_one_line() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let original = peer_message(PeerKind::Ask, "a-1", None);

        slot.record_envoy_exchange(&original, "  the envoy's\u{1b}[2J answer ");

        let (envelopes, dropped) = slot.peer_inbox().drain();
        assert_eq!(dropped, 0);
        assert_eq!(envelopes.len(), 2);
        assert_eq!(peer_payload(&envelopes[0]), &original);
        let reply = peer_payload(&envelopes[1]);
        assert_eq!(reply.kind, PeerKind::Reply);
        assert_eq!(reply.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(reply.content, "the envoy's answer");
        assert_eq!(reply.destination, original.source_destination);
        assert_eq!(reply.via, PeerVia::Direct);
        assert_eq!(reply.message_id.len(), 32, "a fresh simple-form uuid");
        assert_ne!(reply.message_id, original.message_id);
        assert!(reply.timestamp > 1_700_000_000.0);
        assert_eq!(
            (envelopes[1].from.as_str(), envelopes[1].to.as_str()),
            ("", original.source_destination.as_str()),
            "a bare slot has no destination of its own"
        );

        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_message");
        assert_eq!(
            notes[0].id,
            format!("peer:{}", &hex_lower(&PEER_INSTANCE)[..8])
        );
        assert_eq!(notes[0].next_action, "mesh__check_inbox");

        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 1);
        let dest8 = &hex_lower(&PEER_INSTANCE)[..8];
        assert_eq!(
            pushed[0].text,
            format!("{dest8} asked: words of a-1; envoy replied: the envoy's answer")
        );
        assert_eq!(pushed[0].source, Source::Message);
        assert_eq!(
            pushed[0].origin,
            Origin::Peer(hex_lower(&PEER_IDENTITY)[..8].to_string())
        );
        assert!(pushed[0].model_note.is_none());
    }

    #[test]
    fn record_envoy_exchange_caps_the_line_like_a_summary() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);
        let mut original = peer_message(PeerKind::Message, "m-1", None);
        original.content = "q".repeat(PEER_LINE_MAX_CHARS * 2);

        slot.record_envoy_exchange(&original, &"r".repeat(PEER_LINE_MAX_CHARS * 2));

        let pushed = idle.pushed.lock();
        let text = &pushed[0].text;
        assert!(text.starts_with(&format!(
            "{} said: {}; envoy replied: {}",
            &hex_lower(&PEER_INSTANCE)[..8],
            "q".repeat(PEER_LINE_MAX_CHARS / 2),
            "r".repeat(PEER_LINE_MAX_CHARS / 2)
        )));
        assert!(text.chars().count() < PEER_LINE_MAX_CHARS + 40, "{text}");
    }

    #[test]
    fn record_envoy_fallback_takes_the_inbox_path_and_adds_the_reason() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);

        slot.record_envoy_fallback(
            peer_message(PeerKind::Message, "m-1", None),
            "envoy timed out",
        );

        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["m-1"]);
        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_message");
        assert_eq!(notes[0].next_action, "mesh__check_inbox");
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 2);
        let dest8 = &hex_lower(&PEER_INSTANCE)[..8];
        assert_eq!(pushed[0].text, format!("{dest8} says: words of m-1"));
        assert_eq!(
            pushed[1].text,
            format!("envoy timed out; {dest8} says: words of m-1 is in the inbox")
        );
    }

    #[test]
    fn record_envoy_escalated_points_the_model_at_the_human_answer_not_the_inbox() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);

        slot.record_envoy_escalated(peer_message(PeerKind::Ask, "a-1", None), "a-1");

        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(peer_ids(&envelopes), ["a-1"]);
        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_ask");
        assert!(
            notes[0].next_action.contains(".mesh answer a-1"),
            "{}",
            notes[0].next_action
        );
        assert!(!notes[0].next_action.contains("check_inbox"));
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 1);
        let dest8 = &hex_lower(&PEER_INSTANCE)[..8];
        assert_eq!(
            pushed[0].text,
            format!("envoy escalated to the human; {dest8} asks: words of a-1 is in the inbox")
        );
    }

    fn inbound_record(id: &str) -> InboundRecord {
        InboundRecord {
            version: INBOUND_RECORD_VERSION,
            id: id.to_string(),
            peer_destination: hex_lower(&PEER_INSTANCE),
            peer_identity: hex_lower(&PEER_IDENTITY),
            question: format!("words of {id}"),
            envoy_question: String::new(),
            received_at: rfc3339_utc(SystemTime::now()),
        }
    }

    /// A bare slot with an inbound store, as `install` would leave it.
    fn slot_with_inbound(tmp: &TempDir) -> MeshSlot {
        let slot = MeshSlot::default();
        *slot.inbound.lock() = Some(Arc::new(InboundStore::new(&tmp.path, "inst")));
        slot
    }

    #[tokio::test]
    async fn answer_inbound_refuses_an_unknown_id_and_a_slot_with_no_store() {
        let bare = MeshSlot::default();
        let err = bare.answer_inbound("a-1", "yes").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Mesh is off; turn it on with `.mesh on` before answering a-1"),
            "{err}"
        );

        let tmp = TempDir::new("slot-answer-unknown");
        let slot = slot_with_inbound(&tmp);
        slot.inbound_store()
            .unwrap()
            .upsert(inbound_record("a-1"), SystemTime::now())
            .unwrap();
        let err = slot.answer_inbound("a-2", "yes").await.unwrap_err();
        assert!(err.to_string().contains("no open question a-2"), "{err}");
        assert!(slot.inbound_store().unwrap().get("a-1").unwrap().is_some());
    }

    #[tokio::test]
    async fn answer_inbound_hands_the_answer_to_a_live_run_and_leaves_the_question_filed() {
        let tmp = TempDir::new("slot-answer-live");
        let slot = slot_with_inbound(&tmp);
        let store = slot.inbound_store().unwrap();
        store
            .upsert(inbound_record("a-1"), SystemTime::now())
            .unwrap();
        let envoy = RecordingEnvoy::new(true, true);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);

        slot.answer_inbound("a-1", "yes, go ahead").await.unwrap();

        assert_eq!(
            *envoy.answers.lock(),
            [("a-1".to_string(), "yes, go ahead".to_string())]
        );
        assert!(
            store.get("a-1").unwrap().is_some(),
            "the run that took the answer forgets the question once it delivers"
        );
        assert!(
            slot.correlations().list().is_empty(),
            "a peer's question is never one of ours"
        );
    }

    #[tokio::test]
    async fn answer_inbound_with_no_live_run_and_the_mesh_off_keeps_the_question() {
        let tmp = TempDir::new("slot-answer-off");
        let slot = slot_with_inbound(&tmp);
        let store = slot.inbound_store().unwrap();
        store
            .upsert(inbound_record("a-1"), SystemTime::now())
            .unwrap();
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);

        let err = slot.answer_inbound("a-1", "yes").await.unwrap_err();

        assert!(err.to_string().contains("Mesh is off"), "{err}");
        assert_eq!(
            envoy.answers.lock().len(),
            1,
            "the live run was asked first"
        );
        assert!(
            store.get("a-1").unwrap().is_some(),
            "an answer that went nowhere leaves the question open"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answer_inbound_sends_the_reply_to_the_peer_when_no_run_holds_it() {
        use crate::mesh::trust::TrustOptions;

        let stub = PeerStub::listen("node-answer-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on("node-answer-inbound", stub.port()).await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        stub.trust(&runtime.current_destination_hash(), runtime.fingerprint());
        stub.announce(Some("Stub")).await;
        let to = stub.destination_hex();
        let peers = runtime.peers();
        wait_until("the node to file the stub", || peers.get(&to).is_some()).await;
        runtime
            .trust()
            .trust_destination(
                slot.as_ref(),
                &to,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let store = slot.inbound_store().unwrap();
        store
            .upsert(
                InboundRecord {
                    peer_destination: to.clone(),
                    peer_identity: stub.identity_hex(),
                    ..inbound_record("a-1")
                },
                SystemTime::now(),
            )
            .unwrap();

        slot.answer_inbound("a-1", "the human says yes")
            .await
            .unwrap();

        let seen = stub.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].kind, PeerKind::Reply);
        assert_eq!(seen[0].content, "the human says yes");
        assert_eq!(seen[0].in_reply_to.as_deref(), Some("a-1"));
        assert!(store.get("a-1").unwrap().is_none());
        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(envelopes.len(), 1, "{envelopes:?}");
        let recorded = peer_payload(&envelopes[0]);
        assert_eq!(recorded.kind, PeerKind::Reply);
        assert_eq!(recorded.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(
            recorded.source_destination,
            runtime.current_destination_hash()
        );
        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].next_action.contains(".mesh answer"),
            "{}",
            notes[0].next_action
        );
        assert!(slot.stop().await.unwrap());
        assert!(
            slot.inbound_store().is_none(),
            "stopping lets go of the store"
        );
        stub.stop().await;
    }

    #[test]
    fn record_human_answer_files_the_reply_with_one_note_and_one_line() {
        let slot = MeshSlot::default();
        let idle = RecordingIdleSink::new(true);
        slot.set_idle(Arc::clone(&idle) as Arc<dyn IdleSink>);

        slot.record_human_answer(&inbound_record("a-1"), "yes, merge it");

        let (envelopes, _) = slot.peer_inbox().drain();
        assert_eq!(envelopes.len(), 1, "{envelopes:?}");
        let reply = peer_payload(&envelopes[0]);
        assert_eq!(reply.kind, PeerKind::Reply);
        assert_eq!(reply.in_reply_to.as_deref(), Some("a-1"));
        assert_eq!(reply.content, "yes, merge it");
        assert_eq!(reply.destination, hex_lower(&PEER_INSTANCE));
        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_message");
        let dest8 = &hex_lower(&PEER_INSTANCE)[..8];
        assert_eq!(notes[0].id, format!("peer:{dest8}"));
        assert!(
            notes[0].next_action.contains(".mesh answer"),
            "{}",
            notes[0].next_action
        );
        let pushed = idle.pushed.lock();
        assert_eq!(pushed.len(), 1);
        assert_eq!(
            pushed[0].text,
            format!("you answered {dest8}: yes, merge it")
        );
    }

    #[test]
    fn a_reply_that_answers_nothing_lands_in_the_inbox_as_a_message_keeping_in_reply_to() {
        install_log_collector();
        let slot = MeshSlot::default();

        slot.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-unknown")));

        let (envelopes, _) = slot.peer_inbox().drain();
        let delivered = peer_payload(&envelopes[0]);
        assert_eq!(delivered.kind, PeerKind::Message);
        assert_eq!(delivered.in_reply_to.as_deref(), Some("q-unknown"));
        assert_eq!(delivered.message_id, "r-1");
        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_message");
        let id8 = &hex_lower(&PEER_IDENTITY)[..8];
        assert_logged(
            &debug_snapshot(),
            &format!(
                "Mesh reply r-1 from {id8} answers no open question of ours; delivering it as a message"
            ),
        );
    }

    #[test]
    fn a_reply_that_answers_an_open_question_keeps_its_kind_in_the_inbox() {
        let slot = MeshSlot::default();
        slot.correlations().open(pending("q-1")).unwrap();

        slot.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));

        let (envelopes, _) = slot.peer_inbox().drain();
        let delivered = peer_payload(&envelopes[0]);
        assert_eq!(delivered.kind, PeerKind::Reply);
        assert_eq!(delivered.in_reply_to.as_deref(), Some("q-1"));
        assert_eq!(slot.take_model_notes()[0].event, "peer_reply");
    }

    #[test]
    fn a_reply_in_a_later_process_resolves_the_persisted_correlation() {
        let tmp = TempDir::new("slot-pending-carry");
        let slot_a = MeshSlot::default();
        slot_a
            .correlations()
            .attach_store(PendingStore::new(&tmp.path, "inst"), SystemTime::now())
            .unwrap();
        slot_a.correlations().open(pending("q-1")).unwrap();
        drop(slot_a);

        let slot_b = MeshSlot::default();
        let notifier = Arc::new(RecordingSink::default());
        slot_b.set_notifier(Arc::clone(&notifier) as Arc<dyn NotificationSink>);
        assert_eq!(
            slot_b
                .correlations()
                .attach_store(PendingStore::new(&tmp.path, "inst"), SystemTime::now())
                .unwrap(),
            1
        );
        assert!(slot_b.correlations().is_open("q-1"));

        slot_b.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));

        let (envelopes, dropped) = slot_b.peer_inbox().drain();
        assert_eq!(dropped, 0);
        assert_eq!(peer_ids(&envelopes), ["r-1"]);
        let notes = slot_b.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].event, "peer_reply");
        assert_eq!(notes[0].id, "q-1");
        assert_eq!(notes[0].next_action, "mesh__collect --id q-1");
        assert_eq!(
            notifier.0.lock().len(),
            1,
            "the person at the keyboard gets one line"
        );
        assert_eq!(
            slot_b
                .correlations()
                .take_answer("q-1")
                .map(|reply| reply.content),
            Some("words of r-1".to_string())
        );
        assert!(
            PendingStore::new(&tmp.path, "inst")
                .list(SystemTime::now())
                .unwrap()
                .is_empty(),
            "collecting the answer removes the question from disk"
        );
    }

    #[test]
    fn an_uncollected_reply_survives_a_restart() {
        let tmp = TempDir::new("slot-pending-uncollected");
        let slot_a = MeshSlot::default();
        slot_a.set_notifier(Arc::new(RecordingSink::default()) as Arc<dyn NotificationSink>);
        slot_a
            .correlations()
            .attach_store(PendingStore::new(&tmp.path, "inst"), SystemTime::now())
            .unwrap();
        slot_a.correlations().open(pending("q-1")).unwrap();
        slot_a.deliver_peer(peer_message(PeerKind::Reply, "r-1", Some("q-1")));
        assert!(!slot_a.correlations().is_open("q-1"));
        drop(slot_a);

        let slot_b = MeshSlot::default();
        assert_eq!(
            slot_b
                .correlations()
                .attach_store(PendingStore::new(&tmp.path, "inst"), SystemTime::now())
                .unwrap(),
            1
        );
        let listed = slot_b.correlations().list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].record.state, PendingState::Answered);
        assert!(
            listed[0].reply.is_some(),
            "the answer waits to be collected"
        );
        assert_eq!(
            slot_b
                .correlations()
                .take_answer("q-1")
                .map(|reply| reply.content),
            Some("words of r-1".to_string())
        );
        assert!(
            PendingStore::new(&tmp.path, "inst")
                .list(SystemTime::now())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    fn fresh_instance_id() -> String {
        Session::default().ensure_mesh_instance_id().to_string()
    }

    #[cfg(unix)]
    fn lock_path(cache_dir: &std::path::Path, id: &str) -> PathBuf {
        mesh_cache_dir(cache_dir).join(format!("{id}.lock"))
    }

    #[cfg(unix)]
    async fn closed_port() -> u16 {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
    }

    #[cfg(unix)]
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

    fn assert_logged(debugs: &[String], needle: &str) {
        assert!(
            debugs.iter().any(|message| message.contains(needle)),
            "no debug message contains {needle:?}; captured: {debugs:#?}"
        );
    }

    #[test]
    fn record_announce_and_sweep_log_peer_lifecycle() {
        install_log_collector();
        let tmp = TempDir::new("node-log-peers");
        let now = SystemTime::now();
        let peers = PeerTable::load(tmp.path.join("peers.json"), now).unwrap();
        let coyote_hash = "log-lifecycle-coyote-peer";
        let other_hash = "log-lifecycle-other-app";
        let app_data = AnnounceAppData {
            version: 1,
            display_name: Some("Bea".to_string()),
        }
        .encode()
        .unwrap();

        let added = record_announce(
            &peers,
            coyote_hash.to_string(),
            "identity".to_string(),
            "name".to_string(),
            &app_data,
            2,
            now,
        );
        let ignored = record_announce(
            &peers,
            other_hash.to_string(),
            "identity".to_string(),
            "name".to_string(),
            b"LXMF\x00\x01",
            1,
            now,
        );
        let aged_out = peers.sweep(now + PEER_TTL);
        log_aged_out_peers(&aged_out);

        assert_eq!(added.map(|filed| filed.change), Some(PeerChange::Added));
        assert_eq!(ignored, None);
        assert_eq!(aged_out, vec![coyote_hash.to_string()]);
        let debugs = debug_snapshot();
        assert_logged(
            &debugs,
            &format!("Received mesh announce from {coyote_hash} (2 hops, protocol version 1)"),
        );
        assert_logged(&debugs, &format!("Added mesh peer {coyote_hash}"));
        assert_logged(
            &debugs,
            &format!("Ignored announce from {other_hash} (1 hops): not a Coyote node"),
        );
        assert_logged(&debugs, &format!("Aged out mesh peer {coyote_hash}"));
    }

    #[test]
    fn record_announce_keeps_an_emoji_name_with_its_presentation_selector() {
        let tmp = TempDir::new("node-emoji-name");
        let now = SystemTime::now();
        let peers = PeerTable::load(tmp.path.join("peers.json"), now).unwrap();
        let name = "Alex \u{2764}\u{FE0F}";
        let app_data = AnnounceAppData {
            version: 1,
            display_name: Some(name.to_string()),
        }
        .encode()
        .unwrap();

        let change = record_announce(
            &peers,
            "emoji-peer".to_string(),
            "identity".to_string(),
            "name".to_string(),
            &app_data,
            1,
            now,
        );

        assert_eq!(change.map(|filed| filed.change), Some(PeerChange::Added));
        let recorded = peers.snapshot();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].display_name.as_deref(), Some(name));
    }

    #[test]
    fn filing_an_announce_fires_peer_discovered_for_untrusted_and_incompatible_peers_alike() {
        let tmp = TempDir::new("node-announce-hook");
        let now = SystemTime::now();
        let peers = PeerTable::load(tmp.path.join("peers.json"), now).unwrap();
        let (trust, _trust_dir) = TrustList::default().open("node-announce-hook-trust");
        let hooks = MeshHooks::default();
        let sink = RecordingHookSink::attach(&hooks);
        let mut filer = AnnounceFiler {
            peers: &peers,
            trust: &trust,
            hooks: &hooks,
            throttle: DiscoveredThrottle::default(),
        };
        let newer = MESH_PROTOCOL_VERSION + 1;
        let app_data = AnnounceAppData {
            version: newer,
            display_name: Some("Bea".to_string()),
        }
        .encode()
        .unwrap();
        let destination = "ab".repeat(16);
        let identity = "cd".repeat(16);

        let change = filer.file(
            destination.clone(),
            identity.clone(),
            "ef".repeat(10),
            &app_data,
            3,
            now,
        );
        assert_eq!(change, Some(PeerChange::Added));
        let envs = one_fire(&sink, HookEvent::MeshPeerDiscovered);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(destination.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(identity.as_str())
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_PEER_NAME"), Some("Bea"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_HOPS"), Some("3"));
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_PROTOCOL_VERSION"),
            Some(newer.to_string().as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_COMPATIBLE"),
            Some("false")
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_FIRST_SEEN"), Some("true"));

        let change = filer.file(
            destination.clone(),
            identity.clone(),
            "ef".repeat(10),
            &app_data,
            3,
            now + Duration::from_secs(1),
        );
        assert_eq!(change, Some(PeerChange::Refreshed));
        assert!(
            sink.drain().is_empty(),
            "a refresh that changes nothing visible fires nothing"
        );

        let change = filer.file(
            destination.clone(),
            identity.clone(),
            "ef".repeat(10),
            &app_data,
            2,
            now + Duration::from_secs(2),
        );
        assert_eq!(change, Some(PeerChange::Refreshed));
        let envs = one_fire(&sink, HookEvent::MeshPeerDiscovered);
        assert_eq!(env_value(&envs, "COYOTE_MESH_FIRST_SEEN"), Some("false"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_HOPS"), Some("2"));

        let renamed = AnnounceAppData {
            version: newer,
            display_name: Some("Beatrix".to_string()),
        }
        .encode()
        .unwrap();
        filer.file(
            destination.clone(),
            identity.clone(),
            "ef".repeat(10),
            &renamed,
            2,
            now + Duration::from_secs(3),
        );
        let envs = one_fire(&sink, HookEvent::MeshPeerDiscovered);
        assert_eq!(env_value(&envs, "COYOTE_MESH_PEER_NAME"), Some("Beatrix"));

        filer.file(
            destination.clone(),
            identity.clone(),
            "ef".repeat(10),
            &renamed,
            2,
            now + Duration::from_secs(4),
        );
        assert!(sink.drain().is_empty());

        filer.file(
            destination.clone(),
            identity.clone(),
            "ef".repeat(10),
            &renamed,
            2,
            now + Duration::from_secs(3 + HEARTBEAT_SECS),
        );
        let envs = one_fire(&sink, HookEvent::MeshPeerDiscovered);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_FIRST_SEEN"),
            Some("false"),
            "a heartbeat after the last fire an unchanged peer fires again"
        );

        let ignored = filer.file(
            "12".repeat(16),
            identity,
            "ef".repeat(10),
            b"LXMF\x00\x01",
            1,
            now,
        );
        assert_eq!(ignored, None);
        assert!(
            sink.drain().is_empty(),
            "a non-Coyote announce fires nothing"
        );
    }

    #[test]
    fn peer_discovered_fires_are_capped_across_peers_and_refill_with_time() {
        let tmp = TempDir::new("node-announce-hook-bucket");
        let now = SystemTime::now();
        let peers = PeerTable::load(tmp.path.join("peers.json"), now).unwrap();
        let (trust, _trust_dir) = TrustList::default().open("node-announce-hook-bucket-trust");
        let hooks = MeshHooks::default();
        let sink = RecordingHookSink::attach(&hooks);
        let mut filer = AnnounceFiler {
            peers: &peers,
            trust: &trust,
            hooks: &hooks,
            throttle: DiscoveredThrottle::default(),
        };
        let app_data = AnnounceAppData {
            version: MESH_PROTOCOL_VERSION,
            display_name: None,
        }
        .encode()
        .unwrap();
        let file_new = |filer: &mut AnnounceFiler<'_>, n: u32, at: SystemTime| {
            let change = filer.file(
                hex_lower(&n.to_be_bytes()).repeat(4),
                "cd".repeat(16),
                "ef".repeat(10),
                &app_data,
                1,
                at,
            );
            assert_eq!(change, Some(PeerChange::Added));
        };

        for n in 0..100 {
            file_new(&mut filer, n, now);
        }

        let fired = sink.drain();
        assert_eq!(fired.len(), DISCOVERED_FIRES_PER_SEC as usize, "{fired:?}");
        assert!(
            fired
                .iter()
                .all(|(event, _)| *event == HookEvent::MeshPeerDiscovered)
        );

        for n in 100..120 {
            file_new(&mut filer, n, now + Duration::from_millis(500));
        }
        let fired = sink.drain();
        assert_eq!(
            fired.len(),
            (DISCOVERED_FIRES_PER_SEC / 2.0) as usize,
            "half a second refills half the bucket: {fired:?}"
        );
        assert_eq!(
            env_value(&fired[0].1, "COYOTE_MESH_PEER_DESTINATION"),
            Some(hex_lower(&100u32.to_be_bytes()).repeat(4).as_str()),
            "the first peer filed once tokens are back is the first to fire"
        );
    }

    #[test]
    fn peer_discovered_throttle_forgets_stale_destinations_and_caps_its_map() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut throttle = DiscoveredThrottle::default();
        let filed = FiledAnnounce {
            change: PeerChange::Added,
            display_name: None,
            protocol_version: MESH_PROTOCOL_VERSION,
            compatibility: Compatibility::Compatible,
        };
        let destination = |n: usize| hex_lower(&(n as u32).to_be_bytes()).repeat(4);
        let file_at = |throttle: &mut DiscoveredThrottle, n: usize, at: SystemTime| {
            // Keep the bucket out of the way: this test is about the map, not the rate.
            throttle.tokens = DISCOVERED_FIRES_PER_SEC;
            throttle.refilled_at = Some(at);
            assert!(throttle.admits(&destination(n), &filed, 1, at));
        };

        for n in 0..=PEER_TABLE_MAX_ENTRIES {
            file_at(&mut throttle, n, now);
        }
        assert_eq!(
            throttle.fired.len(),
            PEER_TABLE_MAX_ENTRIES,
            "one more destination than the cap evicts the oldest"
        );

        let later = now + Duration::from_secs(2 * HEARTBEAT_SECS + 1);
        file_at(&mut throttle, PEER_TABLE_MAX_ENTRIES + 1, later);
        assert_eq!(
            throttle.fired.len(),
            1,
            "every destination last fired over two heartbeats ago is forgotten"
        );
        assert!(
            throttle
                .fired
                .contains_key(&destination(PEER_TABLE_MAX_ENTRIES + 1))
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_and_stop_log_node_lifecycle_and_fire_the_mesh_hooks() {
        use crate::mesh::trust::TrustOptions;

        install_log_collector();
        let started = started_runtime("node-log-lifecycle").await;
        let runtime = started.runtime.clone();
        let fingerprint = runtime.fingerprint().to_string();
        let instance_id = runtime.instance_id().await;
        let hash = runtime.destination_hash().await;
        let interface = runtime.interfaces().remove(0);
        assert!(interface.starts_with("private 127.0.0.1:"), "{interface}");
        let private_hex = runtime.private_key_hex();
        let slot = Arc::new(MeshSlot::default());
        let sink = RecordingHookSink::attach(&slot.hooks());
        slot.install(runtime.clone()).unwrap();

        let envs = one_fire(&sink, HookEvent::MeshStarted);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_INSTANCE_ID"),
            Some(instance_id.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_DESTINATION"),
            Some(hash.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_IDENTITY"),
            Some(fingerprint.as_str())
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_INTERFACES"), Some("private"));

        // The node was started on its own handle; installing bound it to the slot's sink.
        let identity = "ab".repeat(16);
        runtime
            .trust()
            .trust_identity(
                slot.as_ref(),
                &identity,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let granted = one_fire(&sink, HookEvent::MeshTrustGranted);
        assert_eq!(
            env_value(&granted, "COYOTE_MESH_PEER_IDENTITY"),
            Some(identity.as_str())
        );

        assert!(slot.stop().await.unwrap());
        started.relay_handle.abort();

        let stopped = one_fire(&sink, HookEvent::MeshStopped);
        assert_eq!(stopped, envs);
        assert_eq!(private_hex.len(), 128);
        for (key, value) in envs.iter().chain(&stopped) {
            assert!(!value.contains(&private_hex), "{key} carries the key");
            for at in 0..=private_hex.len() - 8 {
                let window = &private_hex[at..at + 8];
                assert!(
                    fingerprint.contains(window) || !value.contains(window),
                    "{key}={value} carries {window} of the private key"
                );
            }
        }

        let debugs = debug_snapshot();
        assert_logged(
            &debugs,
            &format!(
                "Started mesh node {fingerprint} (instance {instance_id}) as destination {hash}"
            ),
        );
        assert_logged(&debugs, &format!("Joined mesh interface {interface}"));
        assert_logged(
            &debugs,
            &format!("Sent mesh announce for destination {hash}"),
        );
        assert_logged(&debugs, &format!("Left mesh interface {interface}"));
        assert_logged(
            &debugs,
            &format!(
                "Stopped mesh node {fingerprint} (instance {instance_id}, destination {hash})"
            ),
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_peer_to_an_untrusted_destination_fires_message_failed_without_the_content() {
        use crate::mesh::message::SendError;

        let started = started_runtime("node-send-untrusted-hook").await;
        let runtime = &started.runtime;
        let sink = RecordingHookSink::attach(runtime.hooks());
        let destination = "ab".repeat(16);
        let message = OutboundPeer {
            kind: PeerKind::Message,
            id: "m-1".to_string(),
            in_reply_to: None,
            title: None,
            content: "secret body".to_string(),
            fields: None,
        };

        let err = runtime.send_peer(&destination, &message).await.unwrap_err();

        assert_eq!(
            err,
            SendError::NotTrusted {
                destination: destination.clone(),
            }
        );
        let envs = one_fire(&sink, HookEvent::MeshMessageFailed);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_ERROR_CLASS"),
            Some("not_trusted")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_MESSAGE_KIND"),
            Some("message")
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_MESSAGE_ID"), Some("m-1"));
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(destination.as_str())
        );
        for (key, value) in &envs {
            assert!(
                !value.contains(&message.content),
                "{key}={value} carries the content"
            );
        }

        let err = runtime
            .send_peer("\u{1b}[2Jnot-a-hash", &message)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            SendError::NotTrusted {
                destination: "not-a-hash".to_string(),
            }
        );
        let envs = one_fire(&sink, HookEvent::MeshMessageFailed);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_ERROR_CLASS"),
            Some("not_trusted")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            None,
            "a destination that is not a hash is not passed on"
        );
        assert!(env_value(&envs, "COYOTE_MESH_ERROR").is_some());
        for (key, value) in &envs {
            assert!(
                !value.contains('\u{1b}'),
                "{key}={value:?} carries an escape"
            );
        }

        let bulletin = OutboundPeer {
            kind: PeerKind::Bulletin,
            ..message
        };
        let err = runtime
            .send_peer(&destination, &bulletin)
            .await
            .unwrap_err();

        assert_eq!(
            err,
            SendError::NotTrusted {
                destination: destination.clone(),
            }
        );
        assert!(
            sink.drain().is_empty(),
            "a bulletin's fan-out reports through mesh.bulletin.sent alone"
        );
        runtime.shutdown().await.unwrap();
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    fn fires_of<'a>(
        fired: &'a [(HookEvent, Vec<(&'static str, String)>)],
        event: HookEvent,
    ) -> Vec<&'a Vec<(&'static str, String)>> {
        fired
            .iter()
            .filter(|(fired_event, _)| *fired_event == event)
            .map(|(_, envs)| envs)
            .collect()
    }

    /// A trusted, reachable peer: a message reports once through `mesh.message.sent`, a
    /// bulletin once through `mesh.bulletin.sent`, and neither carries the content.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_peer_fires_message_sent_and_broadcast_fires_bulletin_sent_alone() {
        use crate::mesh::trust::TrustOptions;

        let stub = PeerStub::listen("node-sent-hook-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on("node-sent-hook", stub.port()).await;
        let runtime = started.runtime.clone();
        let sink = RecordingHookSink::attach(runtime.hooks());
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        stub.trust(&runtime.current_destination_hash(), runtime.fingerprint());
        stub.announce(Some("Stub")).await;
        let to = stub.destination_hex();
        let peers = runtime.peers();
        wait_until("the node to file the stub", || peers.get(&to).is_some()).await;
        runtime
            .trust()
            .trust_destination(
                slot.as_ref(),
                &to,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        sink.drain();
        let message = OutboundPeer {
            kind: PeerKind::Message,
            id: "m-1".to_string(),
            in_reply_to: None,
            title: Some("plan".to_string()),
            content: "the secret words for the peer".to_string(),
            fields: None,
        };

        let sent = runtime.send_peer(&to, &message).await.unwrap();

        assert_eq!(sent.via, PeerVia::Direct);
        let fired = sink.drain();
        let sent_fires = fires_of(&fired, HookEvent::MeshMessageSent);
        assert_eq!(sent_fires.len(), 1, "{fired:?}");
        assert!(fires_of(&fired, HookEvent::MeshMessageFailed).is_empty());
        let envs = sent_fires[0];
        assert_eq!(env_value(envs, "COYOTE_MESH_VIA"), Some("direct"));
        assert_eq!(env_value(envs, "COYOTE_MESH_MESSAGE_KIND"), Some("message"));
        assert_eq!(env_value(envs, "COYOTE_MESH_MESSAGE_ID"), Some("m-1"));
        assert_eq!(
            env_value(envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(to.as_str())
        );
        for (key, value) in envs {
            assert!(
                !value.contains(&message.content) && !value.contains("plan"),
                "{key}={value} carries the message"
            );
        }

        let bulletin = OutboundPeer {
            kind: PeerKind::Bulletin,
            id: "b-1".to_string(),
            in_reply_to: None,
            title: None,
            content: "the bulletin words for everyone".to_string(),
            fields: None,
        };

        let outcome = runtime.broadcast(&bulletin).await.unwrap();

        assert_eq!(outcome.recipients.len(), 1, "{:?}", outcome.recipients);
        let fired = sink.drain();
        let bulletin_fires = fires_of(&fired, HookEvent::MeshBulletinSent);
        assert_eq!(bulletin_fires.len(), 1, "{fired:?}");
        assert!(
            fires_of(&fired, HookEvent::MeshMessageSent).is_empty()
                && fires_of(&fired, HookEvent::MeshMessageFailed).is_empty(),
            "the per-recipient sends of a bulletin report nothing of their own: {fired:?}"
        );
        let envs = bulletin_fires[0];
        assert_eq!(env_value(envs, "COYOTE_MESH_MESSAGE_ID"), Some("b-1"));
        assert_eq!(env_value(envs, "COYOTE_MESH_RECIPIENTS"), Some("1"));
        assert_eq!(env_value(envs, "COYOTE_MESH_DELIVERED"), Some("1"));
        assert_eq!(env_value(envs, "COYOTE_MESH_STORED"), Some("0"));
        assert_eq!(env_value(envs, "COYOTE_MESH_UNREACHABLE"), Some("0"));
        assert_eq!(env_value(envs, "COYOTE_MESH_REFUSED"), Some("0"));
        for (key, value) in envs {
            assert!(
                !value.contains(&bulletin.content),
                "{key}={value} carries the bulletin"
            );
        }
        let seen = stub.seen();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(slot.stop().await.unwrap());
        stub.stop().await;
    }

    #[test]
    fn plan_interfaces_maps_configured_list_one_to_one() {
        let plans = plan_interfaces(&[
            MeshInterface::Lan,
            MeshInterface::Private {
                host: "127.0.0.1".to_string(),
                port: 4242,
            },
            MeshInterface::Public {
                host: "relay.example".to_string(),
                port: 4965,
            },
        ]);

        assert_eq!(
            plans,
            vec![
                InterfacePlan::Lan,
                InterfacePlan::Tcp {
                    kind: "private",
                    endpoint: "127.0.0.1:4242".to_string(),
                },
                InterfacePlan::Tcp {
                    kind: "public",
                    endpoint: "relay.example:4965".to_string(),
                },
            ]
        );
        assert_eq!(plan_interfaces(&[]), Vec::<InterfacePlan>::new());
    }

    #[tokio::test]
    async fn start_refuses_invalid_config_before_touching_disk() {
        let tmp = TempDir::new("node-invalid");
        let paths = mesh_paths(&tmp);
        let identity_path = paths.identity_path.clone();
        let config = MeshConfig {
            interfaces: vec![],
            ..MeshConfig::default()
        };
        let mut session = Session::default();

        let err = MeshRuntime::start(&config, true, &mut session, paths, NodeOptions::default())
            .await
            .err()
            .expect("an empty interface list must be refused")
            .to_string();

        assert!(err.contains("mesh.interfaces"), "{err}");
        assert!(!tmp.path.join("cache").exists());
        assert!(!identity_path.exists());
        assert_eq!(session.mesh_instance_id(), None);
    }

    #[tokio::test]
    async fn start_refuses_invisible_display_name_before_touching_disk() {
        let tmp = TempDir::new("node-invisible-name");
        let paths = mesh_paths(&tmp);
        let identity_path = paths.identity_path.clone();
        let config = MeshConfig {
            display_name: Some("Al\u{202E}ex".into()),
            ..MeshConfig::default()
        };
        let mut session = Session::default();

        let err = MeshRuntime::start(&config, true, &mut session, paths, NodeOptions::default())
            .await
            .err()
            .expect("a display name with a bidi override must be refused")
            .to_string();

        assert!(err.contains("mesh.display_name"), "{err}");
        assert!(!tmp.path.join("cache").join("mesh").exists());
        assert!(!identity_path.exists());
        assert_eq!(session.mesh_instance_id(), None);
    }

    #[tokio::test]
    async fn start_with_function_calling_disabled_is_refused() {
        let tmp = TempDir::new("node-no-fc");
        let paths = mesh_paths(&tmp);
        let identity_path = paths.identity_path.clone();
        let mut session = Session::default();

        let err = MeshRuntime::start(
            &private_config(1),
            false,
            &mut session,
            paths,
            NodeOptions::default(),
        )
        .await
        .err()
        .expect("a node without function calling must be refused")
        .to_string();

        assert!(err.contains("function_calling_support"), "{err}");
        assert!(!tmp.path.join("cache").exists());
        assert!(!identity_path.exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_fails_naming_unreachable_private_relay() {
        let port = closed_port().await;
        let tmp = TempDir::new("node-unreachable");
        let paths = mesh_paths(&tmp);
        let cache_dir = paths.cache_dir.clone();
        let mut session = Session::default();

        let err = MeshRuntime::start(
            &private_config(port),
            true,
            &mut session,
            paths,
            NodeOptions {
                connect_timeout: Duration::from_millis(300),
                ..NodeOptions::default()
            },
        )
        .await
        .err()
        .expect("a closed relay port must be refused")
        .to_string();

        assert!(err.contains(&format!("127.0.0.1:{port}")), "{err}");
        assert!(err.contains("private"), "{err}");
        let instance_id = session.mesh_instance_id().unwrap();
        assert!(lock_path(&cache_dir, instance_id).exists());
        let _reacquired = InstanceLock::acquire(&cache_dir, instance_id)
            .expect("a failed start must release the instance lock");
        let metrics = tokio::runtime::Handle::current().metrics();
        wait_until("the transport's tasks to exit", || {
            metrics.num_alive_tasks() == 0
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_joins_only_the_configured_relay() {
        let (addr, relay_handle, _) = loopback_relay().await;
        let tmp = TempDir::new("node-join");
        let mut session = Session::default();

        let runtime = MeshRuntime::start(
            &private_config(addr.port()),
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            runtime.interfaces(),
            vec![format!("private 127.0.0.1:{}", addr.port())]
        );
        let instance_id = runtime.instance_id().await;
        assert_eq!(session.mesh_instance_id(), Some(instance_id.as_str()));
        assert!(
            runtime
                .has_destination(&runtime.destination_hash().await)
                .await
        );
        assert_eq!(runtime.fingerprint().len(), 32);

        runtime.shutdown().await.unwrap();
        relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_persists_peer_table_under_cache_dir_mesh() {
        let (addr, relay_handle, _) = loopback_relay().await;
        let tmp = TempDir::new("node-persist-peers");
        let paths = mesh_paths(&tmp);
        let cache_dir = paths.cache_dir.clone();
        let peers_path = mesh_cache_dir(&cache_dir).join("peers.json");
        let mut session = Session::default();
        let runtime = MeshRuntime::start(
            &private_config(addr.port()),
            true,
            &mut session,
            paths,
            NodeOptions::default(),
        )
        .await
        .unwrap();
        runtime.peers().observe(
            PeerSighting {
                destination_hash: "persisted-on-stop".to_string(),
                identity_hash: "identity".to_string(),
                name_hash: "name".to_string(),
                display_name: None,
                protocol_version: 1,
                hops: 1,
            },
            SystemTime::now(),
        );
        assert!(
            !peers_path.exists(),
            "a change must not be written before the persist interval or stop"
        );
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime).unwrap();

        assert!(slot.stop().await.unwrap());

        let bytes = std::fs::read(&peers_path)
            .unwrap_or_else(|err| panic!("{} must exist after stop: {err}", peers_path.display()));
        let records: Vec<PeerRecord> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].destination_hash, "persisted-on-stop");
        relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_configures_the_slot_limits_from_the_node_config() {
        let (addr, relay_handle, _) = loopback_relay().await;
        let tmp = TempDir::new("node-install-limits");
        let mut session = Session::default();
        let config = MeshConfig {
            peer_max_messages_per_hour: 3,
            ..private_config(addr.port())
        };
        let runtime = MeshRuntime::start(
            &config,
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(runtime.peer_limits().messages_per_hour, 3);
        let slot = Arc::new(MeshSlot::default());
        assert_eq!(slot.limits().config(), PeerLimitConfig::default());

        slot.install(runtime).unwrap();

        assert_eq!(slot.limits().config().messages_per_hour, 3);
        assert_eq!(
            slot.limits().config(),
            PeerLimitConfig {
                messages_per_hour: 3,
                ..PeerLimitConfig::default()
            }
        );
        assert!(slot.stop().await.unwrap());
        relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_tolerates_corrupt_peer_table_left_by_previous_run() {
        let (addr, relay_handle, _) = loopback_relay().await;
        let tmp = TempDir::new("node-corrupt-peers");
        let paths = mesh_paths(&tmp);
        let peers_dir = mesh_cache_dir(&paths.cache_dir);
        std::fs::create_dir_all(&peers_dir).unwrap();
        std::fs::write(peers_dir.join("peers.json"), b"").unwrap();
        let mut session = Session::default();

        let runtime = MeshRuntime::start(
            &private_config(addr.port()),
            true,
            &mut session,
            paths,
            NodeOptions::default(),
        )
        .await
        .expect("a corrupt peer table must not keep the node from starting");

        assert!(runtime.peers().snapshot().is_empty());
        assert!(peers_dir.join("peers.json.corrupt").exists());
        runtime.shutdown().await.unwrap();
        relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_tears_down_the_relay_socket_and_releases_the_lock() {
        let (addr, relay_handle, relay_closed) = loopback_relay().await;
        let metrics = tokio::runtime::Handle::current().metrics();
        let baseline = metrics.num_alive_tasks();
        let tmp = TempDir::new("node-teardown");
        let paths = mesh_paths(&tmp);
        let cache_dir = paths.cache_dir.clone();
        let mut session = Session::default();
        let runtime = MeshRuntime::start(
            &private_config(addr.port()),
            true,
            &mut session,
            paths,
            NodeOptions::default(),
        )
        .await
        .unwrap();
        let instance_id = runtime.instance_id().await;
        assert!(
            InstanceLock::acquire(&cache_dir, &instance_id).is_err(),
            "the running node must hold its instance lock"
        );
        assert_eq!(relay_closed.load(Ordering::SeqCst), 0);
        assert!(metrics.num_alive_tasks() > baseline);
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime).unwrap();

        assert!(slot.stop().await.unwrap());

        let _reacquired = InstanceLock::acquire(&cache_dir, &instance_id)
            .expect("stop must release the instance lock");
        wait_until("the relay to see its stream closed", || {
            relay_closed.load(Ordering::SeqCst) == 1
        })
        .await;
        wait_until("the node's tasks to exit", || {
            metrics.num_alive_tasks() == baseline
        })
        .await;
        relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rekey_moves_destination_and_never_reuses_original_hash() {
        let started = started_runtime("node-rekey").await;
        let runtime = &started.runtime;
        let original_id = runtime.instance_id().await;
        let old_hash = runtime.destination_hash().await;
        let fork_id = fresh_instance_id();

        runtime
            .rekey(ForkRekey {
                original_instance_id: Some(original_id.clone()),
                fork_instance_id: fork_id.clone(),
            })
            .await
            .unwrap();

        let new_hash = runtime.destination_hash().await;
        assert_ne!(new_hash, old_hash);
        assert_eq!(runtime.instance_id().await, fork_id);
        assert_eq!(
            started.session.mesh_instance_id(),
            Some(original_id.as_str()),
            "re-keying moves the node, not the original session's id"
        );
        assert!(lock_path(&runtime.cache_dir, &fork_id).exists());
        assert!(!runtime.has_destination(&old_hash).await);
        assert!(runtime.has_destination(&new_hash).await);
        let _original_lock = InstanceLock::acquire(&runtime.cache_dir, &original_id)
            .expect("re-keying must release the original instance lock");

        runtime.shutdown().await.unwrap();
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rekey_refuses_when_original_instance_does_not_match() {
        let started = started_runtime("node-rekey-mismatch").await;
        let runtime = &started.runtime;
        let original_id = runtime.instance_id().await;
        let hash = runtime.destination_hash().await;

        let err = runtime
            .rekey(ForkRekey {
                original_instance_id: Some(fresh_instance_id()),
                fork_instance_id: fresh_instance_id(),
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains(".mesh off"), "{err}");
        assert_eq!(runtime.destination_hash().await, hash);
        assert_eq!(runtime.instance_id().await, original_id);
        runtime.shutdown().await.unwrap();
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rekey_refuses_when_fork_lock_is_held_and_keeps_original() {
        let started = started_runtime("node-rekey-locked").await;
        let runtime = &started.runtime;
        let original_id = runtime.instance_id().await;
        let hash = runtime.destination_hash().await;
        let fork_id = fresh_instance_id();
        let _held = InstanceLock::acquire(&runtime.cache_dir, &fork_id).unwrap();

        let result = runtime
            .rekey(ForkRekey {
                original_instance_id: Some(original_id.clone()),
                fork_instance_id: fork_id,
            })
            .await;

        assert!(result.is_err());
        assert_eq!(runtime.instance_id().await, original_id);
        assert_eq!(runtime.destination_hash().await, hash);
        assert!(runtime.has_destination(&hash).await);
        assert!(
            InstanceLock::acquire(&runtime.cache_dir, &original_id).is_err(),
            "a refused re-key must keep the original instance lock"
        );
        runtime.shutdown().await.unwrap();
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_cancels_and_awaits_registered_tasks() {
        let started = started_runtime("node-stop").await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        let token = runtime.cancellation_token();
        let done = Arc::new(AtomicBool::new(false));
        let task_done = done.clone();
        runtime.register_task(tokio::spawn(async move {
            token.cancelled().await;
            sleep(Duration::from_millis(50)).await;
            task_done.store(true, Ordering::SeqCst);
        }));
        slot.install(runtime).unwrap();

        assert!(slot.stop().await.unwrap());

        assert!(
            done.load(Ordering::SeqCst),
            "stop must wait for registered tasks, not just cancel them"
        );
        assert!(slot.get().is_none());
        assert!(!slot.stop().await.unwrap());
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_interrupts_the_attached_envoy_once_and_leaves_it_attached() {
        let started = started_runtime("node-stop-envoy").await;
        let slot = Arc::new(MeshSlot::default());
        let envoy = RecordingEnvoy::new(true, false);
        slot.set_envoy(Arc::clone(&envoy) as Arc<dyn EnvoySink>);
        slot.install(started.runtime.clone()).unwrap();

        assert!(slot.stop().await.unwrap());

        assert_eq!(envoy.interrupts.load(Ordering::SeqCst), 1);
        assert!(
            slot.envoy.load_full().is_some(),
            "the envoy belongs to the session, not the node; stop leaves it attached"
        );
        assert!(!slot.stop().await.unwrap());
        assert_eq!(
            envoy.interrupts.load(Ordering::SeqCst),
            1,
            "a stop with no node interrupts nothing"
        );
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_on_a_stopped_runtime_is_not_running() {
        let started = started_runtime("node-request-stopped").await;
        let runtime = &started.runtime;
        let peer = SingleInputDestination::new(
            TransportIdentity::new_from_rand(OsRng),
            DestinationName::new("coyote", "mesh.peer"),
        )
        .desc;
        runtime.shutdown().await.unwrap();

        let result = runtime
            .request(&peer, "/echo", rmpv::Value::Nil, RequestOptions::default())
            .await;

        assert_eq!(result.unwrap_err(), R3Error::NotRunning);
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_refuses_when_a_runtime_is_already_installed() {
        let started = started_runtime("node-install").await;
        let slot = Arc::new(MeshSlot::default());
        slot.install(started.runtime.clone()).unwrap();

        let err = slot
            .install(started.runtime.clone())
            .unwrap_err()
            .to_string();

        assert!(err.contains(".mesh off"), "{err}");
        assert!(slot.stop().await.unwrap());
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rekey_carries_escalated_questions_into_the_fork_store() {
        let started = started_runtime("node-rekey-inbound").await;
        let cache_dir = started.runtime.cache_dir().to_path_buf();
        let original_id = started.runtime.current_instance_id();
        let fork_id = fresh_instance_id();
        let slot = Arc::new(MeshSlot::default());
        slot.install(started.runtime.clone()).unwrap();
        slot.inbound_store()
            .unwrap()
            .upsert(inbound_record("q-before-fork"), SystemTime::now())
            .unwrap();

        slot.rekey(ForkRekey {
            original_instance_id: Some(original_id.clone()),
            fork_instance_id: fork_id.clone(),
        })
        .await
        .unwrap();

        let fork_store = slot.inbound_store().unwrap();
        assert_eq!(
            fork_store.path(),
            InboundStore::new(&cache_dir, &fork_id).path(),
            "the slot serves the fork's inbound file"
        );
        assert!(fork_store.get("q-before-fork").unwrap().is_some());
        assert!(
            InboundStore::new(&cache_dir, &original_id)
                .get("q-before-fork")
                .unwrap()
                .is_some(),
            "the original's file is left as it was"
        );
        assert!(slot.stop().await.unwrap());
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_detaches_the_pending_store_and_leaves_the_file_for_the_next_install() {
        let started = started_runtime("node-stop-pending").await;
        let cache_dir = started.runtime.cache_dir().to_path_buf();
        let instance_id = started.runtime.current_instance_id();
        let slot = Arc::new(MeshSlot::default());
        slot.install(started.runtime.clone()).unwrap();
        slot.correlations().open(pending("q-1")).unwrap();

        assert!(slot.stop().await.unwrap());

        assert!(slot.correlations().list().is_empty());
        assert_eq!(
            PendingStore::new(&cache_dir, &instance_id)
                .list(SystemTime::now())
                .unwrap()
                .len(),
            1,
            "the question waits on disk for the next install"
        );
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_with_an_unreadable_pending_file_still_serves_and_warns() {
        install_log_collector();
        let started = started_runtime("node-install-unreadable").await;
        let cache_dir = started.runtime.cache_dir().to_path_buf();
        let instance_id = started.runtime.current_instance_id();
        let store = PendingStore::new(&cache_dir, &instance_id);
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), "not a record\n").unwrap();
        let slot = Arc::new(MeshSlot::default());

        slot.install(started.runtime.clone()).unwrap();

        assert!(slot.get().is_some(), "the node serves");
        assert!(slot.correlations().list().is_empty());
        let warned = warn_snapshot();
        let path = store.path().display().to_string();
        assert!(
            warned.iter().any(|message| {
                message.contains("could not be reopened")
                    && message.contains(&path)
                    && message.contains("move the file aside")
            }),
            "{warned:#?}"
        );
        assert_eq!(
            std::fs::read_to_string(store.path()).unwrap(),
            "not a record\n",
            "the unreadable file is left for the person to move aside"
        );
        assert!(slot.stop().await.unwrap());
        started.relay_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rekey_rebinds_the_pending_store_to_the_fork_instance() {
        install_log_collector();
        let started = started_runtime("node-rekey-pending").await;
        let cache_dir = started.runtime.cache_dir().to_path_buf();
        let original_id = started.runtime.current_instance_id();
        let fork_id = fresh_instance_id();
        let slot = Arc::new(MeshSlot::default());
        slot.install(started.runtime.clone()).unwrap();
        slot.correlations().open(pending("q-original")).unwrap();
        let fork_store = PendingStore::new(&cache_dir, &fork_id);
        fork_store
            .upsert(pending("q-fork-earlier"), SystemTime::now())
            .unwrap();

        slot.rekey(ForkRekey {
            original_instance_id: Some(original_id.clone()),
            fork_instance_id: fork_id.clone(),
        })
        .await
        .unwrap();

        assert!(
            slot.correlations().get("q-original").is_none(),
            "the fork does not collect the original's questions"
        );
        assert!(
            slot.correlations().is_open("q-fork-earlier"),
            "the fork's own file is reopened"
        );
        slot.correlations().open(pending("q-fork")).unwrap();
        let now = SystemTime::now();
        let ids = |instance: &str| -> Vec<String> {
            PendingStore::new(&cache_dir, instance)
                .list(now)
                .unwrap()
                .into_iter()
                .map(|record| record.id)
                .collect()
        };
        assert_eq!(ids(&original_id), ["q-original"]);
        assert_eq!(ids(&fork_id), ["q-fork", "q-fork-earlier"]);

        let second_fork_id = fresh_instance_id();
        let second_store = PendingStore::new(&cache_dir, &second_fork_id);
        std::fs::write(second_store.path(), "not a record\n").unwrap();
        slot.rekey(ForkRekey {
            original_instance_id: Some(fork_id.clone()),
            fork_instance_id: second_fork_id.clone(),
        })
        .await
        .unwrap();
        assert_eq!(started.runtime.current_instance_id(), second_fork_id);
        assert!(
            slot.correlations().list().is_empty(),
            "an unreadable fork file leaves nothing pending"
        );
        let path = second_store.path().display().to_string();
        assert!(
            warn_snapshot()
                .iter()
                .any(|message| message.contains("could not be reopened") && message.contains(&path)),
            "{:#?}",
            warn_snapshot()
        );
        assert!(slot.stop().await.unwrap());
        started.relay_handle.abort();
    }

    #[test]
    fn deliver_peer_notes_name_the_sending_instance_never_the_peers_own_id() {
        let slot = MeshSlot::default();
        let message = PeerMessage::new(RawPeerMessage {
            source_identity: hex_lower(&PEER_IDENTITY),
            source_destination: hex_lower(&PEER_INSTANCE),
            destination: hex_lower(&[0x01; 16]),
            title: None,
            content: "hello".to_string(),
            fields: None,
            timestamp: 1_700_000_000.0,
            message_id: format!("\u{1b}[2J{}", "i".repeat(PEER_ID_MAX_CHARS + 5)),
            in_reply_to: None,
            kind: PeerKind::Message,
            via: PeerVia::Direct,
        });

        slot.deliver_peer(message);

        let notes = slot.take_model_notes();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].id,
            format!("peer:{}", &hex_lower(&PEER_INSTANCE)[..8])
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_nodes_exchange_announces_over_tcp() {
        let port = closed_port().await;
        let node_b = Transport::new(TransportConfig::new(
            "b",
            &TransportIdentity::new_from_rand(OsRng),
            false,
        ));
        let server = TcpServer::new(format!("127.0.0.1:{port}"), node_b.iface_manager());
        let server_status = server.runtime_status_handle();
        let server_iface = node_b
            .iface_manager()
            .lock()
            .await
            .spawn(server, TcpServer::spawn);
        wait_until("node B to listen", || {
            server_status.to_json()["listener_state"].as_str() == Some("listening")
        })
        .await;
        let mut b_announces = node_b.recv_announces().await;

        let tmp = TempDir::new("node-interop");
        let mut session = Session::default();
        let config = MeshConfig {
            display_name: Some("Alex".to_string()),
            ..private_config(port)
        };
        let node_a = MeshRuntime::start(
            &config,
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
        let a_hash = node_a.destination_hash().await;

        let event = timeout(INTEROP_TIMEOUT, b_announces.recv())
            .await
            .expect("node B must hear node A's start announce")
            .unwrap();
        assert_eq!(
            event
                .destination
                .lock()
                .await
                .desc
                .address_hash
                .to_hex_string(),
            a_hash
        );
        assert_eq!(
            AnnounceAppData::decode(event.app_data.as_slice()),
            Some(AnnounceAppData {
                version: 1,
                display_name: Some("Alex".to_string()),
            })
        );

        let b_name = DestinationName::new("coyote", &format!("mesh.{}", fresh_instance_id()));
        let b_dest = node_b
            .add_destination(TransportIdentity::new_from_rand(OsRng), b_name)
            .await;
        let (b_hash, b_identity_hash) = {
            let desc = &b_dest.lock().await.desc;
            (
                desc.address_hash.to_hex_string(),
                desc.identity.address_hash.to_hex_string(),
            )
        };
        let b_app_data = AnnounceAppData {
            version: 1,
            display_name: Some("Bea".to_string()),
        }
        .encode()
        .unwrap();
        let packet = b_dest
            .lock()
            .await
            .announce(OsRng, Some(&b_app_data))
            .unwrap();
        node_b.send_packet(packet).await;

        let peers = node_a.peers();
        wait_until("node A to file node B as a peer", || {
            peers
                .snapshot()
                .iter()
                .any(|peer| peer.destination_hash == b_hash)
        })
        .await;
        let snapshot = peers.snapshot();
        let bea = snapshot
            .iter()
            .find(|peer| peer.destination_hash == b_hash)
            .unwrap();
        assert_eq!(bea.display_name.as_deref(), Some("Bea"));
        assert_eq!(bea.protocol_version, 1);
        assert_eq!(bea.name_hash, hex_lower(b_name.as_name_hash_slice()));
        assert_eq!(bea.identity_hash, b_identity_hash);
        assert!(
            !snapshot.iter().any(|peer| peer.destination_hash == a_hash),
            "a node must not file its own announce as a peer"
        );

        let slot = Arc::new(MeshSlot::default());
        slot.install(node_a).unwrap();
        assert!(slot.stop().await.unwrap());
        node_b
            .iface_manager()
            .lock()
            .await
            .stop_interface(server_iface);
        drop(node_b);
    }

    /// The debug lines logged after `mark` that report a link to `destination_hex` coming
    /// up: `open_link` names the full hash, so the filter is this test's own.
    #[cfg(unix)]
    fn links_opened_since(mark: usize, destination_hex: &str) -> Vec<String> {
        let needle = format!("to destination {destination_hex} is active");
        debug_snapshot()
            .into_iter()
            .skip(mark)
            .filter(|line| line.contains(&needle))
            .collect()
    }

    /// A peer the announce called compatible refuses this node's version over the wire:
    /// the refusal comes back typed, the peer is marked, and the next send stops at the
    /// table without a link.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_refusing_our_protocol_version_is_marked_incompatible() {
        use crate::mesh::message::SendError;
        use crate::mesh::trust::TrustOptions;

        install_log_collector();
        let stub = PeerStub::listen("node-version-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on("node-version-refused", stub.port()).await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        stub.trust(&runtime.current_destination_hash(), runtime.fingerprint());
        stub.announce(Some("Stub")).await;
        let to = stub.destination_hex();
        let peers = runtime.peers();
        wait_until("the node to file the stub", || peers.get(&to).is_some()).await;
        assert_eq!(
            peers.get(&to).unwrap().compatibility,
            Compatibility::Compatible
        );
        runtime
            .trust()
            .trust_destination(
                slot.as_ref(),
                &to,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let desc = runtime.resolve_destination(&to).await.unwrap();
        let newer = MESH_PROTOCOL_VERSION + 1;
        let mut envelope = runtime.envelope(rmpv::Value::Nil).await;
        envelope.version = newer;

        let err = runtime
            .request_envelope(&desc, STATUS_PATH, envelope, RequestOptions::default())
            .await
            .unwrap_err();

        assert_eq!(
            err,
            R3Error::UnsupportedVersion {
                found: Some(newer),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            }
        );
        assert_eq!(
            peers.get(&to).unwrap().compatibility,
            Compatibility::Incompatible {
                found: MESH_PROTOCOL_VERSION
            }
        );

        let mark = debug_snapshot().len();
        let message = OutboundPeer::new(PeerKind::Message, "hello", None, None, None).unwrap();
        let err = runtime.send_peer(&to, &message).await.unwrap_err();

        assert_eq!(
            err,
            SendError::IncompatibleVersion {
                destination: to.clone(),
                found: Some(MESH_PROTOCOL_VERSION),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            }
        );
        let links = links_opened_since(mark, &to);
        assert!(
            links.is_empty(),
            "a marked peer is never linked to: {links:?}"
        );
        assert!(stub.seen().is_empty());
        assert!(slot.stop().await.unwrap());
        stub.stop().await;
    }

    /// An announce at a version this node does not speak is filed like any other, marked
    /// incompatible, and a send to it is refused at the table without a link.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_incompatible_announce_is_recorded_but_refused_outbound() {
        use crate::mesh::message::SendError;
        use crate::mesh::trust::TrustOptions;

        install_log_collector();
        let newer = MESH_PROTOCOL_VERSION + 1;
        let app_data = AnnounceAppData {
            version: newer,
            display_name: Some("Newer".to_string()),
        }
        .encode()
        .unwrap();
        let now = SystemTime::now();
        let tmp = TempDir::new("node-incompatible-announce");
        let table = PeerTable::load(tmp.path.join("peers.json"), now).unwrap();
        let hash = "ab".repeat(16);

        let change = record_announce(
            &table,
            hash.clone(),
            "cd".repeat(16),
            "ef".repeat(10),
            &app_data,
            1,
            now,
        );

        assert_eq!(change.map(|filed| filed.change), Some(PeerChange::Added));
        let record = table.get(&hash).unwrap();
        assert_eq!(record.protocol_version, newer);
        assert_eq!(
            record.compatibility,
            Compatibility::Incompatible { found: newer }
        );
        assert_eq!(
            record.compatibility_line().as_deref(),
            Some("incompatible: speaks protocol 2, this Coyote supports 1..=1")
        );
        let expected = format!(
            "Mesh peer {} is incompatible: it speaks protocol {newer}, this Coyote supports 1..=1",
            &hash[..8]
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &expected),
            "expected {expected:?} in {:#?}",
            debug_snapshot()
        );

        // The stub announces at the current version so the transport learns a path to it;
        // the same announce at the newer version is then what the runtime's table says.
        let stub = PeerStub::listen("node-incompatible-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on("node-incompatible-outbound", stub.port()).await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        stub.announce(Some("Stub")).await;
        let to = stub.destination_hex();
        let peers = runtime.peers();
        wait_until("the node to file the stub", || peers.get(&to).is_some()).await;
        let filed = peers.get(&to).unwrap();
        assert_eq!(
            record_announce(
                peers.as_ref(),
                to.clone(),
                filed.identity_hash,
                filed.name_hash,
                &app_data,
                filed.hops,
                SystemTime::now(),
            )
            .map(|filed| filed.change),
            Some(PeerChange::Refreshed)
        );
        assert_eq!(
            peers.get(&to).unwrap().compatibility,
            Compatibility::Incompatible { found: newer }
        );
        runtime
            .trust()
            .trust_destination(
                slot.as_ref(),
                &to,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();

        let mark = debug_snapshot().len();
        let message = OutboundPeer::new(PeerKind::Message, "hello", None, None, None).unwrap();
        let err = runtime.send_peer(&to, &message).await.unwrap_err();

        assert_eq!(
            err,
            SendError::IncompatibleVersion {
                destination: to.clone(),
                found: Some(newer),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            }
        );
        let links = links_opened_since(mark, &to);
        assert!(
            links.is_empty(),
            "an incompatible peer is never linked to: {links:?}"
        );
        assert!(stub.seen().is_empty());
        assert!(slot.stop().await.unwrap());
        stub.stop().await;
    }

    fn compatible_peer_table(tag: &str) -> (TempDir, PeerTable, String) {
        let tmp = TempDir::new(tag);
        let now = SystemTime::now();
        let peers = PeerTable::load(tmp.path.join("peers.json"), now).unwrap();
        let hash = "ab".repeat(16);
        peers.observe(
            PeerSighting {
                destination_hash: hash.clone(),
                identity_hash: "cd".repeat(16),
                name_hash: "ef".repeat(10),
                display_name: None,
                protocol_version: MESH_PROTOCOL_VERSION,
                hops: 1,
            },
            now,
        );
        (tmp, peers, hash)
    }

    /// Only a refusal whose window is consistent and excludes the version this node sent
    /// marks the peer, and it is marked with the peer's `max`; an inverted or containing
    /// window, and any other outcome, leave the record alone.
    #[test]
    fn version_refusal_marks_only_a_consistent_window_that_excludes_our_version() {
        let refusal = |min, max| {
            Err(R3Error::UnsupportedVersion {
                found: Some(2),
                min,
                max,
            })
        };

        let (_tmp, peers, hash) = compatible_peer_table("node-refusal-excluding");
        note_version_refusal(&peers, &hash, STATUS_PATH, 1, &refusal(2, 3));
        assert_eq!(
            peers.get(&hash).unwrap().compatibility,
            Compatibility::Incompatible { found: 3 }
        );

        let (_tmp, peers, hash) = compatible_peer_table("node-refusal-inverted");
        note_version_refusal(&peers, &hash, STATUS_PATH, 1, &refusal(3, 2));
        assert_eq!(
            peers.get(&hash).unwrap().compatibility,
            Compatibility::Compatible
        );

        let (_tmp, peers, hash) = compatible_peer_table("node-refusal-containing");
        note_version_refusal(&peers, &hash, STATUS_PATH, 1, &refusal(1, 2));
        assert_eq!(
            peers.get(&hash).unwrap().compatibility,
            Compatibility::Compatible
        );

        let (_tmp, peers, hash) = compatible_peer_table("node-refusal-other");
        note_version_refusal(
            &peers,
            &hash,
            STATUS_PATH,
            1,
            &Err(R3Error::Timeout {
                path: STATUS_PATH.to_string(),
                after: Duration::from_secs(1),
            }),
        );
        note_version_refusal(&peers, &hash, STATUS_PATH, 1, &Err(R3Error::NotRunning));
        assert_eq!(
            peers.get(&hash).unwrap().compatibility,
            Compatibility::Compatible
        );
    }

    /// A stub filed as compatible, then marked incompatible in the table as a wire refusal
    /// would, with a link mark taken after the marking.
    #[cfg(unix)]
    async fn marked_incompatible_stub(
        tag: &str,
    ) -> (
        PeerStub,
        Arc<MeshRuntime>,
        Arc<MeshSlot>,
        DestinationDesc,
        String,
        usize,
    ) {
        use crate::mesh::trust::TrustOptions;

        install_log_collector();
        let stub = PeerStub::listen(&format!("{tag}-stub"), TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on(tag, stub.port()).await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        stub.trust(&runtime.current_destination_hash(), runtime.fingerprint());
        stub.announce(Some("Stub")).await;
        let to = stub.destination_hex();
        let peers = runtime.peers();
        wait_until("the node to file the stub", || peers.get(&to).is_some()).await;
        runtime
            .trust()
            .trust_destination(
                slot.as_ref(),
                &to,
                TrustOptions::default(),
                SystemTime::now(),
            )
            .unwrap();
        let desc = runtime.resolve_destination(&to).await.unwrap();
        peers.mark_incompatible(&to, 7);
        let mark = debug_snapshot().len();
        (stub, runtime, slot, desc, to, mark)
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn knock_with_stops_at_a_peer_already_marked_incompatible() {
        let (stub, runtime, slot, desc, to, mark) =
            marked_incompatible_stub("node-knock-incompatible").await;

        let err = runtime
            .knock_with(
                &desc,
                &KnockIntro::new("hi").unwrap(),
                KnockOptions::default(),
            )
            .await
            .unwrap_err();

        assert_eq!(
            err,
            KnockError::Direct(R3Error::UnsupportedVersion {
                found: Some(7),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            })
        );
        let links = links_opened_since(mark, &to);
        assert!(
            links.is_empty(),
            "a marked peer is never knocked over a link: {links:?}"
        );
        assert!(slot.stop().await.unwrap());
        stub.stop().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_status_with_stops_at_a_peer_already_marked_incompatible() {
        use crate::mesh::card::StatusError;

        let (stub, runtime, slot, desc, to, mark) =
            marked_incompatible_stub("node-status-incompatible").await;

        let err = runtime
            .request_status_with(&desc, RequestOptions::default())
            .await
            .unwrap_err();

        assert_eq!(
            err,
            StatusError::Transport(R3Error::UnsupportedVersion {
                found: Some(7),
                min: MESH_PROTOCOL_MIN_SUPPORTED,
                max: MESH_PROTOCOL_VERSION,
            })
        );
        let links = links_opened_since(mark, &to);
        assert!(
            links.is_empty(),
            "a marked peer is never asked for status over a link: {links:?}"
        );
        assert!(slot.stop().await.unwrap());
        stub.stop().await;
    }

    /// A trusted destination last heard ten days ago announces: the trust record's
    /// `last_seen_at` moves forward in memory while `trust.yaml` keeps its bytes.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_announce_refreshes_a_trusted_destinations_last_seen_without_writing() {
        use crate::mesh::trust::TrustOptions;

        let stub = PeerStub::listen("node-seen-stub", TcpServer::DEFAULT_CLIENT_MTU).await;
        let started = started_runtime_on("node-seen-refresh", stub.port()).await;
        let runtime = started.runtime.clone();
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime.clone()).unwrap();
        let to = stub.destination_hex();
        // The trust file keeps whole seconds, so the seeded sighting is a whole second too
        // and the record reads back equal to it.
        let since_epoch = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ten_days_ago = std::time::UNIX_EPOCH + Duration::from_secs(since_epoch - 10 * 86_400);
        runtime.peers().observe(
            PeerSighting {
                destination_hash: to.clone(),
                identity_hash: stub.identity_hex(),
                name_hash: hex_lower(
                    DestinationName::new("coyote", "mesh.node-seen-stub").as_name_hash_slice(),
                ),
                display_name: Some("Stub".to_string()),
                protocol_version: MESH_PROTOCOL_VERSION,
                hops: 1,
            },
            ten_days_ago,
        );
        let trust = runtime.trust();
        trust
            .trust_destination(slot.as_ref(), &to, TrustOptions::default(), ten_days_ago)
            .unwrap();
        let last_seen = |records: Vec<crate::mesh::trust::TrustRecord>| {
            records
                .into_iter()
                .find(|record| record.hash == to)
                .unwrap()
                .last_seen_at
        };
        assert_eq!(last_seen(trust.records()), ten_days_ago);
        let bytes_before = std::fs::read(trust.path()).unwrap();

        stub.announce(Some("Stub")).await;

        wait_until("the announce to refresh the trusted destination", || {
            last_seen(trust.records()) > ten_days_ago
        })
        .await;
        assert_eq!(std::fs::read(trust.path()).unwrap(), bytes_before);
        assert!(slot.stop().await.unwrap());
        stub.stop().await;
    }
}
