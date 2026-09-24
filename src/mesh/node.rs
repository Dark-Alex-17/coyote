use crate::config::mesh_config::{MeshConfig, MeshInterface};
use crate::config::{ForkRekey, Session, paths};
use crate::mesh::announce::{
    AnnounceAppData, HEARTBEAT_SECS, REANNOUNCE_FLOOR_SECS, announce_app_data,
};
use crate::mesh::lock::InstanceLock;
use crate::mesh::peers::{PeerChange, PeerSighting, PeerTable};
use crate::mesh::r3::{
    R3Client, R3Error, R3Server, RequestHandler, RequestOptions, RequestOutcome,
};
use crate::mesh::{identity, mesh_cache_dir};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::RwLock;
use rand_core::OsRng;
use rns_transport::destination::DestinationDesc;
use rns_transport::destination::{DestinationName, SingleInputDestination};
use rns_transport::hash::AddressHash;
use rns_transport::identity::PrivateIdentity as TransportIdentity;
use rns_transport::identity_bridge::to_transport_private_identity;
use rns_transport::iface::auto::{AutoInterfaceConfig, AutoInterfaceDeviceFilter};
use rns_transport::iface::auto_runtime::{
    AutoDiscoveryRuntime, AutoInterfaceTransportRuntime, AutoRuntimePlan,
};
use rns_transport::iface::tcp_client::TcpClient;
use rns_transport::iface::{IfaceRole, InterfaceMode};
use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
use std::path::PathBuf;
use std::sync::Arc;
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

/// Where a node keeps its identity and its disposable state.
pub(crate) struct MeshPaths {
    pub identity_path: PathBuf,
    pub cache_dir: PathBuf,
}

impl MeshPaths {
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn from_env() -> Self {
        Self {
            identity_path: identity::identity_path(),
            cache_dir: paths::cache_dir(),
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
    cache_dir: PathBuf,
    interface_labels: Vec<String>,
    /// `None` once `shutdown` has released this owner. Requests and the server loop hold
    /// clones, so the upstream `Drop` that cancels the transport's tasks runs when the last
    /// of those finishes, not when this lock is emptied.
    transport: Mutex<Option<Arc<Transport>>>,
    interfaces: Mutex<Vec<JoinedInterface>>,
    destination: Mutex<DestinationState>,
    peers: Arc<PeerTable>,
    r3_client: Arc<R3Client>,
    r3_server: Arc<R3Server>,
    cancel: CancellationToken,
    tasks: parking_lot::Mutex<Vec<JoinHandle<()>>>,
}

impl MeshRuntime {
    /// Brings a node up for `session` and returns it running. Validation comes first so a bad
    /// config touches nothing on disk; the instance lock is taken before the identity is minted
    /// so a refused start never creates a key.
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
        let (dest, hash) = match register_destination(
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
        let runtime = Arc::new(Self {
            fingerprint,
            transport_identity,
            app_data,
            announce: config.announce,
            cache_dir: paths.cache_dir,
            interface_labels: plans.iter().map(InterfacePlan::label).collect(),
            transport: Mutex::new(Some(transport.clone())),
            interfaces: Mutex::new(joined),
            destination: Mutex::new(DestinationState {
                dest,
                hash,
                instance_id,
                lock: Some(lock),
                last_announce,
            }),
            peers,
            r3_client: Arc::new(R3Client::new()),
            r3_server: Arc::new(R3Server::new()),
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

    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn peers(&self) -> Arc<PeerTable> {
        self.peers.clone()
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

    /// Sends one request to `destination` over a link, proving this node's identity first.
    // Reached by the mesh dispatcher once it lands.
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
        let request = self.r3_client.request(
            &transport,
            &self.transport_identity,
            destination,
            path,
            data,
            options,
        );
        tokio::select! {
            () = self.cancel.cancelled() => Err(R3Error::Shutdown),
            outcome = request => outcome,
        }
    }

    /// Installs the sink for inbound requests; until then they are dropped.
    // Reached by the mesh dispatcher once it lands.
    #[allow(dead_code)]
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
        let (dest, hash) = register_destination(
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
) -> Result<(Arc<Mutex<SingleInputDestination>>, AddressHash)> {
    let name = DestinationName::new("coyote", &format!("mesh.{instance_id}"));
    let destination = SingleInputDestination::new(identity.clone(), name);
    let hash = destination.desc.address_hash;
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
    Ok((dest, hash))
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
        let (destination_hash, identity_hash) = {
            let destination = event.destination.lock().await;
            (
                destination.desc.address_hash.to_hex_string(),
                destination.desc.identity.address_hash.to_hex_string(),
            )
        };
        record_announce(
            &peers,
            destination_hash,
            identity_hash,
            event.app_data.as_slice(),
            event.hops,
            SystemTime::now(),
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
#[derive(Default)]
pub(crate) struct MeshSlot {
    inner: RwLock<Option<Arc<MeshRuntime>>>,
}

impl MeshSlot {
    pub(crate) fn get(&self) -> Option<Arc<MeshRuntime>> {
        self.inner.read().clone()
    }

    /// Refuses while a node is already running: two nodes in one process would fight over
    /// the same instance lock and identity.
    // Reached by the REPL mesh commands once they land.
    #[allow(dead_code)]
    pub(crate) fn install(&self, runtime: Arc<MeshRuntime>) -> Result<()> {
        let mut slot = self.inner.write();
        if slot.is_some() {
            bail!(
                "Mesh is already on in this process. Run `.mesh off` first, then `.mesh on` to start it again with the current settings."
            );
        }
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
}

// Tests that start a runtime are unix-only because identity minting writes an owner-only
// file, which is implemented for unix alone so far.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::peers::PEER_TTL;
    #[cfg(unix)]
    use crate::mesh::peers::PeerRecord;
    use crate::mesh::test_support::{TempDir, mesh_paths, private_config};
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
            &app_data,
            2,
            now,
        );
        let ignored = record_announce(
            &peers,
            other_hash.to_string(),
            "identity".to_string(),
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
        let slot = MeshSlot::default();
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
        let slot = MeshSlot::default();
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
        let slot = MeshSlot::default();
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
        let slot = MeshSlot::default();
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
        let slot = MeshSlot::default();
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

        let b_dest = node_b
            .add_destination(
                TransportIdentity::new_from_rand(OsRng),
                DestinationName::new("coyote", &format!("mesh.{}", fresh_instance_id())),
            )
            .await;
        let b_hash = b_dest.lock().await.desc.address_hash.to_hex_string();
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
        assert!(
            !snapshot.iter().any(|peer| peer.destination_hash == a_hash),
            "a node must not file its own announce as a peer"
        );

        let slot = MeshSlot::default();
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
