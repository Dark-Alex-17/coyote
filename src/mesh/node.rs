use crate::config::mesh_config::{MeshConfig, MeshInterface};
use crate::config::{ForkRekey, Session, paths};
use crate::mesh::announce::{
    AnnounceAppData, HEARTBEAT_SECS, REANNOUNCE_FLOOR_SECS, announce_app_data,
};
use crate::mesh::card::{CardSource, StatusHandler};
use crate::mesh::idle::{IdleNotify, IdleSink};
use crate::mesh::knock::{
    ChannelKnockSink, KNOCK_LINK_TIMEOUT, KNOCK_QUEUE_CAPACITY, KNOCK_REQUEST_TIMEOUT, KnockError,
    KnockGate, KnockIntro, KnockOutcome, KnockRouting, KnockSurface, KnockVia, drain_knocks,
    knock_message,
};
use crate::mesh::knocks::KnockCache;
use crate::mesh::lock::InstanceLock;
use crate::mesh::notify::{Notification, NotificationSink};
use crate::mesh::peers::{PeerChange, PeerSighting, PeerTable};
use crate::mesh::propagation::{self, PropagationError, PropagationOptions};
use crate::mesh::propagation_fetch::{self, FetchError, FetchOptions, FetchReport, InboundSink};
use crate::mesh::propagation_nodes::PropagationNodeTable;
#[cfg(test)]
use crate::mesh::r3::RequestHandler;
use crate::mesh::r3::{
    Dispatcher, Envelope, KNOCK_PATH, OriginName, R3Client, R3Error, R3Server, RefusalCode,
    RequestOptions, RequestOutcome, RequestReceipt, STATUS_PATH, short,
};
use crate::mesh::snapshot::MeshSnapshot;
use crate::mesh::trust::TrustStore;
use crate::mesh::{hex_lower, identity, mesh_cache_dir};

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwapOption;
use parking_lot::RwLock;
use rand_core::OsRng;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::{DestinationName, SingleInputDestination};
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity as TransportIdentity;
use rns_transport::identity_bridge::{to_core_private_identity, to_transport_private_identity};
use rns_transport::iface::auto::{AutoInterfaceConfig, AutoInterfaceDeviceFilter};
use rns_transport::iface::auto_runtime::{
    AutoDiscoveryRuntime, AutoInterfaceTransportRuntime, AutoRuntimePlan,
};
use rns_transport::iface::tcp_client::TcpClient;
use rns_transport::iface::{IfaceRole, InterfaceMode};
use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
use std::path::PathBuf;
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

/// Where a node keeps its identity, the user's trust list and its disposable state.
pub(crate) struct MeshPaths {
    pub identity_path: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl MeshPaths {
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
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
}

impl Default for NodeOptions {
    fn default() -> Self {
        Self {
            connect_timeout: TcpClient::DEFAULT_CONNECT_TIMEOUT,
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

/// One in-process Reticulum node: a transport over the configured interfaces, one destination
/// derived from the session's mesh instance id, and the tasks that announce it and track peers.
pub(crate) struct MeshRuntime {
    fingerprint: String,
    transport_identity: TransportIdentity,
    app_data: Vec<u8>,
    announce: bool,
    display_name: Option<String>,
    cache_dir: PathBuf,
    interface_labels: Vec<String>,
    /// `None` once `shutdown` has released this owner. Requests and the server loop hold
    /// clones, so the upstream `Drop` that cancels the transport's tasks runs when the last
    /// of those finishes, not when this lock is emptied.
    transport: Mutex<Option<Arc<Transport>>>,
    interfaces: Mutex<Vec<JoinedInterface>>,
    destination: Mutex<DestinationState>,
    peers: Arc<PeerTable>,
    propagation_nodes: Arc<PropagationNodeTable>,
    trust: Arc<TrustStore>,
    r3_client: Arc<R3Client>,
    r3_server: Arc<R3Server>,
    dispatcher: Arc<Dispatcher>,
    knock_gate: Arc<KnockGate>,
    knock_sink: Arc<ChannelKnockSink>,
    /// Held for the length of one propagation fetch; a second caller is refused, never
    /// queued behind the first.
    fetching: Mutex<()>,
    /// Held across one knock's propagation-node post. `propagate` needs calls for the same
    /// node serialised, so a second knocker waits here rather than sharing the out-link.
    posting: Mutex<()>,
    cancel: CancellationToken,
    tasks: parking_lot::Mutex<Vec<JoinHandle<()>>>,
}

impl MeshRuntime {
    /// Brings a node up for `session` and returns it running. Validation comes first so a bad
    /// config touches nothing on disk; the instance lock is taken before the identity is minted
    /// so a refused start never creates a key. A trust list that does not load refuses the
    /// start outright: the node never serves against a partial list.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
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
        ));
        let runtime = Arc::new(Self {
            fingerprint,
            transport_identity,
            app_data,
            announce: config.announce,
            display_name: config.display_name.clone(),
            cache_dir: paths.cache_dir,
            interface_labels: plans.iter().map(InterfacePlan::label).collect(),
            transport: Mutex::new(Some(transport.clone())),
            interfaces: Mutex::new(joined),
            destination: Mutex::new(DestinationState {
                dest,
                hash,
                origin,
                instance_id,
                lock: Some(lock),
                last_announce,
            }),
            peers,
            propagation_nodes: Arc::new(PropagationNodeTable::new()),
            trust,
            r3_client: Arc::new(R3Client::new()),
            r3_server,
            dispatcher,
            knock_gate,
            knock_sink,
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
            runtime.propagation_nodes.clone(),
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

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The configured display name, as the status card carries it to trusted peers.
    /// `display_name_on_public` gates announces only; the card is exempt because it
    /// reaches trusted destinations and nobody else.
    pub(crate) fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn destination_hash(&self) -> String {
        self.destination.lock().await.hash.to_hex_string()
    }

    /// Human labels of the joined interfaces, in config order.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn interfaces(&self) -> Vec<String> {
        self.interface_labels.clone()
    }

    pub(crate) fn peers(&self) -> Arc<PeerTable> {
        self.peers.clone()
    }

    /// The LXMF propagation nodes heard so far, as `fetch_propagated` chooses among them.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn propagation_nodes(&self) -> Arc<PropagationNodeTable> {
        self.propagation_nodes.clone()
    }

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
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
        let transport = self
            .transport
            .lock()
            .await
            .clone()
            .ok_or(R3Error::NotRunning)?;
        let envelope = self.envelope(data).await;
        let request = self.r3_client.request(
            &transport,
            &self.transport_identity,
            destination,
            path,
            envelope,
            options,
        );
        tokio::select! {
            () = self.cancel.cancelled() => Err(R3Error::Shutdown),
            outcome = request => outcome,
        }
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
        let transport = self
            .transport
            .lock()
            .await
            .clone()
            .ok_or(R3Error::NotRunning)?;
        let envelope = self.envelope(data).await;
        Ok(self.r3_client.request_with_receipt(
            transport,
            self.transport_identity.clone(),
            *destination,
            path.to_string(),
            envelope,
            options,
            self.cancellation_token(),
        ))
    }

    /// `body` in the envelope naming the instance this node speaks for right now, read per
    /// request so a rekeyed node claims its new instance and never a cached one.
    async fn envelope(&self, body: rmpv::Value) -> Envelope {
        Envelope {
            origin: self.destination.lock().await.origin,
            body,
        }
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
            Err(err) => {
                debug!("Mesh knock to {dest8} was not filed over the link: {err}");
                return Err(KnockError::Direct(err));
            }
        };
        // Selected before queueing behind another post: a knocker with no node to fall
        // back on is told so at once rather than after someone else's transfer.
        let node = self
            .propagation_nodes
            .select()
            .map_err(|_| KnockError::NoPropagationNode)?;
        let _posting = self.posting.lock().await;
        let node_hex = node.destination.address_hash.to_hex_string();
        debug!(
            "Mesh knock to {dest8} could not be delivered over a link ({unreachable}); storing it with propagation node {}",
            short(&node_hex)
        );
        let transport = self
            .transport
            .lock()
            .await
            .clone()
            .ok_or(KnockError::NotRunning)?;
        let sender = to_core_private_identity(&self.transport_identity);
        let origin = self.destination.lock().await.origin;
        propagation::propagate(
            &transport,
            &sender,
            &destination.identity,
            &node,
            &knock_message(intro, &origin),
            self.cancellation_token(),
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
    /// handing the ones that pass every check to `sink`, knocks excepted: those go to the
    /// knock gate and never reach `sink`. The dedup store is read from disk
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
        let routing = KnockRouting {
            gate: &self.knock_gate,
            inner: sink,
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

/// Files one received announce in the peer table; `None` when it is not a Coyote announce.
fn record_announce(
    peers: &PeerTable,
    destination_hash: String,
    identity_hash: String,
    name_hash: String,
    app_data: &[u8],
    hops: u8,
    now: SystemTime,
) -> Option<PeerChange> {
    let Some(decoded) = AnnounceAppData::decode(app_data) else {
        debug!("Ignored announce from {destination_hash} ({hops} hops): not a Coyote node");
        return None;
    };
    debug!(
        "Received mesh announce from {destination_hash} ({hops} hops, protocol version {})",
        decoded.version
    );
    let change = peers.observe(
        PeerSighting {
            destination_hash: destination_hash.clone(),
            identity_hash,
            name_hash,
            display_name: decoded.display_name,
            protocol_version: decoded.version,
            hops,
        },
        now,
    );
    match change {
        PeerChange::Added => debug!("Added mesh peer {destination_hash}"),
        PeerChange::Refreshed => debug!("Refreshed mesh peer {destination_hash}"),
    }
    Some(change)
}

async fn receive_announces(
    mut announces: broadcast::Receiver<AnnounceEvent>,
    peers: Arc<PeerTable>,
    propagation_nodes: Arc<PropagationNodeTable>,
    cancel: CancellationToken,
) {
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
        record_announce(
            &peers,
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
/// The snapshot, objective override and brief live in lock-free slots: the REPL holds the
/// session context write-locked for a whole turn, so anything that serves peers must be
/// readable without it.
///
/// `snapshot()` is the picture taken at the last turn boundary. `brief_text()` and
/// `objective_override()` are live and may be newer than the snapshot's `brief.text` and
/// `objective`. Consumers overlay `objective_override()` on `snapshot().objective` when it is
/// `Some`. The live `brief_text()` is authoritative, including `None`: a cleared brief must
/// not fall back to the snapshot's copy. `snapshot().brief.text` is the value at capture,
/// kept so a snapshot is self-describing.
///
/// The notifier slot is where lines meant for the person at the keyboard go; it is filled by
/// whichever front end owns the terminal, so mesh code never needs to know which one is
/// running.
///
/// The idle slot is where events that also concern the model go; the interactive REPL's
/// idle-time driver fills it. Without one, `push_idle` keeps the human line and drops the
/// model's copy, since a headless run has no transcript for it to reach.
#[derive(Default)]
pub(crate) struct MeshSlot {
    inner: RwLock<Option<Arc<MeshRuntime>>>,
    snapshot: ArcSwapOption<MeshSnapshot>,
    objective_override: ArcSwapOption<String>,
    brief_text: ArcSwapOption<String>,
    notifier: ArcSwapOption<Arc<dyn NotificationSink>>,
    idle: ArcSwapOption<Arc<dyn IdleSink>>,
}

impl MeshSlot {
    pub(crate) fn get(&self) -> Option<Arc<MeshRuntime>> {
        self.inner.read().clone()
    }

    /// Refuses while a node is already running: two nodes in one process would fight over
    /// the same instance lock and identity. Installing also puts this slot behind the
    /// node's `/status` provider and knock gate, held weakly since the slot owns the node.
    /// A knock the gate admits between `MeshRuntime::start` and this call is cached but
    /// not surfaced, and not marked as surfaced either, so a repeat from that identity
    /// still earns its one line.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn install(self: &Arc<Self>, runtime: Arc<MeshRuntime>) -> Result<()> {
        // Nothing may `.await` while this guard is held: the status handler reads the same
        // lock for the display name.
        let mut slot = self.inner.write();
        if slot.is_some() {
            bail!(
                "Mesh is already on in this process. Run `.mesh off` first, then `.mesh on` to start it again with the current settings."
            );
        }
        let source = Arc::downgrade(self) as Weak<dyn CardSource>;
        runtime
            .dispatcher()
            .register(STATUS_PATH, Arc::new(StatusHandler::new(source)))?;
        runtime
            .knock_gate()
            .attach(Arc::downgrade(self) as Weak<dyn KnockSurface>);
        *slot = Some(runtime);
        Ok(())
    }

    /// Takes the node out of the slot and shuts it down. `Ok(false)` when nothing was running.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) async fn stop(&self) -> Result<bool> {
        let taken = self.inner.write().take();
        match taken {
            Some(runtime) => {
                runtime.shutdown().await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Re-keys the running node for a forked session; a no-op while the mesh is off.
    pub(crate) async fn rekey(&self, rekey: ForkRekey) -> Result<()> {
        match self.get() {
            Some(runtime) => runtime.rekey(rekey).await,
            None => Ok(()),
        }
    }

    pub(crate) fn publish(&self, snapshot: MeshSnapshot) {
        self.snapshot.store(Some(Arc::new(snapshot)));
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

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn set_objective_override(&self, objective: Option<String>) {
        self.objective_override.store(non_blank(objective));
    }

    pub(crate) fn objective_override(&self) -> Option<Arc<String>> {
        self.objective_override.load_full()
    }

    // Reached by the brief generator once it lands.
    #[allow(dead_code)]
    pub(crate) fn publish_brief(&self, text: Option<String>) {
        self.brief_text.store(non_blank(text));
    }

    pub(crate) fn brief_text(&self) -> Option<Arc<String>> {
        self.brief_text.load_full()
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
}

fn non_blank(value: Option<String>) -> Option<Arc<String>> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(Arc::new)
}

impl KnockSurface for MeshSlot {
    fn surface(&self, note: IdleNotify) -> bool {
        self.push_idle(note)
    }
}

// Tests that start a runtime are unix-only because identity minting writes an owner-only
// file, which is implemented for unix alone so far.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::idle::Origin;
    use crate::mesh::notify::{RenderedNotification, Source};
    use crate::mesh::peers::PEER_TTL;
    #[cfg(unix)]
    use crate::mesh::peers::PeerRecord;
    use crate::mesh::test_support::{TempDir, mesh_paths, private_config, snapshot_fixture};
    #[cfg(unix)]
    use crate::mesh::test_support::{loopback_relay, started_runtime};
    use crate::testing::{debug_snapshot, install_log_collector};
    #[cfg(unix)]
    use rns_transport::iface::tcp_server::TcpServer;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};
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
        assert!(slot.brief_text().is_none());

        slot.set_objective_override(Some("review the mesh".into()));
        assert_eq!(
            slot.objective_override().unwrap().as_str(),
            "review the mesh"
        );
        slot.set_objective_override(None);
        assert!(slot.objective_override().is_none());

        slot.publish_brief(Some("Working on the mesh".into()));
        assert_eq!(slot.brief_text().unwrap().as_str(), "Working on the mesh");
        slot.publish_brief(None);
        assert!(slot.brief_text().is_none());
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

        slot.publish_brief(Some("\tWorking on the mesh  ".into()));
        assert_eq!(slot.brief_text().unwrap().as_str(), "Working on the mesh");
        slot.publish_brief(Some(" \n".into()));
        assert!(slot.brief_text().is_none());
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

        assert_eq!(added, Some(PeerChange::Added));
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

        assert_eq!(change, Some(PeerChange::Added));
        let recorded = peers.snapshot();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].display_name.as_deref(), Some(name));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_and_stop_log_node_lifecycle() {
        install_log_collector();
        let started = started_runtime("node-log-lifecycle").await;
        let runtime = started.runtime.clone();
        let fingerprint = runtime.fingerprint().to_string();
        let instance_id = runtime.instance_id().await;
        let hash = runtime.destination_hash().await;
        let interface = runtime.interfaces().remove(0);
        assert!(interface.starts_with("private 127.0.0.1:"), "{interface}");
        let slot = Arc::new(MeshSlot::default());
        slot.install(runtime).unwrap();

        assert!(slot.stop().await.unwrap());
        started.relay_handle.abort();

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
}
