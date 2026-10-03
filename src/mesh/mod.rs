#![deny(unsafe_code)]

pub(crate) mod access;
mod announce;
pub(crate) mod brief;
pub(crate) mod card;
#[cfg(test)]
mod conformance;
pub(crate) mod envoy;
pub(crate) mod events;
pub(crate) mod fetch;
#[cfg(test)]
mod fuzz;
pub(crate) mod grants;
pub(crate) mod identity;
pub(crate) mod idle;
pub(crate) mod inbox;
pub(crate) mod knock;
pub(crate) mod knocks;
pub(crate) mod limits;
mod lock;
pub(crate) mod message;
mod node;
pub(crate) mod notify;
mod peers;
pub(crate) mod pending;
mod propagation;
mod propagation_fetch;
mod propagation_nodes;
mod protocol;
mod r3;
// pub(crate): the REPL pins the shared refusal wording in its tests.
pub(crate) mod schema;
pub(crate) mod shares;
pub(crate) mod snapshot;
#[cfg(test)]
mod spec_pins;
pub(crate) mod trust;
pub(crate) mod wire_path;

#[cfg(test)]
pub(crate) use node::session_destination_name;
pub(crate) use node::{
    MESH_ALREADY_ON, MeshPaths, MeshRuntime, MeshSlot, NodeOptions, refusal_reply,
};
pub(crate) use peers::PeerRecord;
pub(crate) use propagation_fetch::{
    FetchError, FetchReport, LoggingInboundSink, MAX_WANTS_PER_FETCH,
};
pub(crate) use propagation_nodes::PropagationNodeRecord;
pub(crate) use r3::{RequestOptions, redact_hashes, short};

use crate::config::sanitize_display_text;
use anyhow::{Context, Result};
use rns_transport::hash::{AddressHash, Hash};
use sha2::Digest;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where mesh state that is safe to lose lives: instance locks, the peer table, the knock
/// cache and the propagation fetch store.
pub(crate) fn mesh_cache_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join("mesh")
}

/// Where mesh state the user curates lives: the identity key and the trust list.
pub(crate) fn mesh_config_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("mesh")
}

/// RFC 3339 UTC to the second, the timestamp form every human-readable mesh file uses.
pub(crate) fn rfc3339_utc(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(SystemTime::from)
}

/// How long ago `then` was, in the coarsest unit that fits: `12s ago`, `3m ago`, `2h ago`,
/// `5d ago`. A `then` in the future reads as `0s ago`.
pub(crate) fn age_text(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn decode_hex(text: &str) -> Option<Vec<u8>> {
    // `from_str_radix` would take a sign, so the digits are checked first.
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Reticulum's destination derivation: the address hash is the truncated SHA-256 of the
/// name hash followed by the identity's address hash.
pub(crate) fn destination_address(
    name_hash: &[u8; r3::NAME_HASH_LEN],
    identity: &AddressHash,
) -> AddressHash {
    AddressHash::new_from_hash(&Hash::new(
        Hash::generator()
            .chain_update(name_hash)
            .chain_update(identity.as_slice())
            .finalize()
            .into(),
    ))
}

/// Lower-cased `text` when it is exactly 32 ASCII hex digits. Upstream's hex parser checks
/// byte length only and slices by byte, so it must never see anything this has not passed.
pub(crate) fn canonical_hash(text: &str) -> Option<String> {
    (text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

/// Peer or local text as this node may show or send it: terminal escape sequences stripped,
/// every other control character and the line and paragraph separators a space, the
/// invisible formatting characters and variation selectors dropped, trimmed, and cut to
/// `max_chars` characters on a character boundary with no trailing whitespace. Blank text
/// is `None`. Escapes go first so a sequence's own bytes never survive as spaces.
pub(crate) fn display_text(text: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = sanitize_display_text(text)
        .chars()
        .map(|c| match c {
            '\u{2028}' | '\u{2029}' => ' ',
            c => c,
        })
        .filter(|c| !announce::is_control_or_invisible(*c) && !announce::is_variation_selector(*c))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let capped = match trimmed.char_indices().nth(max_chars) {
        Some((cut, _)) => &trimmed[..cut],
        None => trimmed,
    };
    Some(capped.trim_end().to_string())
}

/// Writes `bytes` to `<path>.tmp` beside `path`, syncs it, then renames it into place, so a
/// crash mid-write cannot leave a half-written file for the next load to refuse.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
    }
    let tmp = path.with_added_extension("tmp");
    File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path))
        .with_context(|| format!("Failed to write '{}'", path.display()))
}

#[cfg(test)]
pub(crate) mod test_support {
    #[cfg(unix)]
    use super::announce::AnnounceAppData;
    #[cfg(unix)]
    use super::hex_lower;
    #[cfg(unix)]
    use super::message::{PeerBody, from_r3_body, received_reply};
    use super::node::MeshPaths;
    #[cfg(unix)]
    use super::node::{MeshRuntime, NodeOptions};
    #[cfg(unix)]
    pub(crate) use super::peers::PeerSighting;
    pub(crate) use super::propagation::PropagationNode;
    #[cfg(unix)]
    use super::propagation::pn_announce_app_data;
    #[cfg(unix)]
    use super::propagation_fetch::FETCH_TRANSFER_LIMIT_KB;
    pub(crate) use super::propagation_fetch::{InboundMessage, InboundSink};
    pub(crate) use super::protocol::Compatibility;
    #[cfg(unix)]
    pub(crate) use super::r3::{ACCESS_PATH, FETCH_PATH, LIST_PATH};
    #[cfg(unix)]
    use super::r3::{
        Admission, Dispatcher, InboundRequest, LoggingKnockSink, R3Client, R3Server, RequestHandler,
    };
    pub(crate) use super::r3::{
        AdmittedRequest, Handler, MESSAGE_PATH, NAME_HASH_LEN, OriginName, PathHash, RefusalCode,
        Reply, RequestId, SizeBranch,
    };
    #[cfg(unix)]
    use super::session_destination_name;
    use super::snapshot::{BriefState, MeshSnapshot, SessionInfo, TurnState};
    use super::trust::TrustStore;
    use super::{mesh_config_dir, rfc3339_utc};
    use crate::config::MeshConfig;
    #[cfg(unix)]
    use crate::config::Session;
    use crate::config::mesh_config::{MeshBrief, MeshInterface};
    use crate::config::todo::TodoList;

    #[cfg(unix)]
    use async_trait::async_trait;
    #[cfg(unix)]
    use parking_lot::Mutex;
    #[cfg(unix)]
    use rand_core::OsRng;
    #[cfg(unix)]
    use rmpv::Value;
    #[cfg(unix)]
    use rns_transport::destination::link::LinkId;
    #[cfg(unix)]
    use rns_transport::destination::{DestinationDesc, DestinationName, SingleInputDestination};
    #[cfg(unix)]
    use rns_transport::hash::AddressHash;
    #[cfg(unix)]
    use rns_transport::identity::{Identity, PrivateIdentity as TransportIdentity};
    #[cfg(unix)]
    use rns_transport::iface::tcp_client::TcpClient;
    #[cfg(unix)]
    use rns_transport::iface::tcp_server::TcpServer;
    #[cfg(unix)]
    use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
    #[cfg(unix)]
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs};
    #[cfg(unix)]
    use tokio::io::AsyncReadExt;
    #[cfg(unix)]
    use tokio::net::TcpListener;
    #[cfg(unix)]
    use tokio::sync::broadcast;
    #[cfg(unix)]
    use tokio::task::JoinHandle;
    #[cfg(unix)]
    use tokio::time::{sleep, timeout};
    #[cfg(unix)]
    use tokio_util::sync::CancellationToken;

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
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
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
        (addr, handle, closed)
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

    #[cfg(unix)]
    pub(crate) async fn closed_port() -> u16 {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        port
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

    /// A listening node: a bare transport with a `TcpServer`, one destination and an
    /// `R3Server` answering on it, the shape `MeshConfig` cannot express since it only
    /// connects outward.
    #[cfg(unix)]
    pub(crate) struct Listener {
        pub(crate) transport: Arc<Transport>,
        pub(crate) server: Arc<R3Server>,
        pub(crate) dest: Arc<tokio::sync::Mutex<SingleInputDestination>>,
        pub(crate) desc: DestinationDesc,
        pub(crate) iface: AddressHash,
        pub(crate) cancel: CancellationToken,
        pub(crate) port: u16,
    }

    #[cfg(unix)]
    impl Listener {
        pub(crate) async fn listen(
            server: Arc<R3Server>,
            handler: Arc<dyn RequestHandler>,
            client_mtu: usize,
            identity: TransportIdentity,
            name: DestinationName,
        ) -> Self {
            let port = closed_port().await;
            let transport = Arc::new(Transport::new(TransportConfig::new(
                "b",
                &TransportIdentity::new_from_rand(OsRng),
                false,
            )));
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
            wait_until("the listener to listen", || {
                status.to_json()["listener_state"].as_str() == Some("listening")
            })
            .await;
            let dest = transport.add_destination(identity, name).await;
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

        pub(crate) async fn announce(&self, app_data: Option<&[u8]>) {
            let packet = self.dest.lock().await.announce(OsRng, app_data).unwrap();
            self.transport.send_packet(packet).await;
        }

        pub(crate) async fn stop(self) {
            self.cancel.cancel();
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
        pub(crate) request_id: String,
        pub(crate) identity: Option<AddressHash>,
        pub(crate) branch: SizeBranch,
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
                request_id: request.request_id.to_hex_string(),
                identity: request.identity.map(|identity| identity.address_hash),
                branch: request.branch,
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
                Arc::new(R3Server::new()),
                script.clone(),
                client_mtu,
                TransportIdentity::new_from_rand(OsRng),
                DestinationName::new("lxmf", "propagation"),
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
            match from_r3_body(&request.body) {
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
    }

    #[cfg(unix)]
    impl PeerStub {
        pub(crate) async fn listen(tag: &str, client_mtu: usize) -> Self {
            let identity = TransportIdentity::new_from_rand(OsRng);
            let recorder = Arc::new(MessageRecorder::default());
            let trust_dir = TempDir::new(tag);
            let handler = Self::gate(&TrustList::default(), &trust_dir, recorder.clone(), &[]);
            let listener = Listener::listen(
                Arc::new(R3Server::new()),
                handler,
                client_mtu,
                identity.clone(),
                session_destination_name(tag),
            )
            .await;
            Self {
                listener,
                identity,
                recorder,
                list: Mutex::new(TrustList::default()),
                extra: Mutex::new(Vec::new()),
                trust_dir,
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
            self.listener.server.set_handler(Self::gate(
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
            self.listener.announce(Some(&app_data)).await;
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
            display_name: display_name.map(str::to_string),
            protocol_version: 1,
            hops: 1,
        }
    }

    /// A connecting node: a bare transport with a `TcpClient` and an `R3Client`, and an
    /// identity to prove on links.
    #[cfg(unix)]
    pub(crate) struct Connector {
        pub(crate) transport: Arc<Transport>,
        pub(crate) identity: TransportIdentity,
        pub(crate) client: Arc<R3Client>,
        pub(crate) client_task: JoinHandle<()>,
        pub(crate) announces: broadcast::Receiver<AnnounceEvent>,
        pub(crate) iface: AddressHash,
        pub(crate) iface_task: JoinHandle<()>,
        pub(crate) cancel: CancellationToken,
    }

    #[cfg(unix)]
    impl Connector {
        pub(crate) async fn connect(port: u16, mtu: usize) -> Self {
            let identity = TransportIdentity::new_from_rand(OsRng);
            let transport = Arc::new(Transport::new(TransportConfig::new("a", &identity, false)));
            let announces = transport.recv_announces().await;
            let client = Arc::new(R3Client::new());
            let cancel = CancellationToken::new();
            let client_task = tokio::spawn(client.clone().run(
                transport.out_link_events(),
                transport.resource_events(),
                cancel.clone(),
            ));
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
                client,
                client_task,
                announces,
                iface,
                iface_task,
                cancel,
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
            self.cancel.cancel();
            // Bounded because a connector a test has wedged with a response-size limit has
            // its transport's handler lock held for good.
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
            plan: None,
            todo: TodoList::default(),
            brief: BriefState {
                mode: MeshBrief::Auto,
                text: None,
                digest_generated_at: None,
            },
            cwd: PathBuf::new(),
            captured_at: SystemTime::now(),
            session: SessionInfo {
                name: None,
                model: "test".into(),
                role: None,
            },
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
        _tmp: TempDir,
    }

    /// A runtime joined to a loopback relay, with its identity and cache under a temp dir.
    #[cfg(unix)]
    pub(crate) async fn started_runtime(tag: &str) -> StartedRuntime {
        let (addr, relay_handle, _) = loopback_relay().await;
        started_runtime_at(tag, addr.port(), relay_handle).await
    }

    /// A runtime joined to whatever listens on `port`, such as a `PeerStub`. There is no
    /// relay task, so `relay_handle` is a task already finished and aborting it does
    /// nothing.
    #[cfg(unix)]
    pub(crate) async fn started_runtime_on(tag: &str, port: u16) -> StartedRuntime {
        started_runtime_at(tag, port, tokio::spawn(std::future::ready(()))).await
    }

    #[cfg(unix)]
    async fn started_runtime_at(
        tag: &str,
        port: u16,
        relay_handle: JoinHandle<()>,
    ) -> StartedRuntime {
        let tmp = TempDir::new(tag);
        let mut session = Session::default();
        let runtime = MeshRuntime::start(
            &private_config(port),
            true,
            &mut session,
            mesh_paths(&tmp),
            NodeOptions::default(),
        )
        .await
        .unwrap();
        StartedRuntime {
            runtime,
            session,
            relay_handle,
            _tmp: tmp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TempDir, read_source, rust_sources};
    use super::*;

    #[test]
    fn mesh_cache_dir_is_mesh_under_cache_dir() {
        assert_eq!(
            mesh_cache_dir(Path::new("/tmp/cache")),
            PathBuf::from("/tmp/cache/mesh")
        );
    }

    #[test]
    fn mesh_config_dir_is_mesh_under_config_dir() {
        assert_eq!(
            mesh_config_dir(Path::new("/tmp/config")),
            PathBuf::from("/tmp/config/mesh")
        );
    }

    #[test]
    fn display_text_drops_the_unicode_line_and_paragraph_separators() {
        assert_eq!(
            display_text("one\u{2028}two\u{2029}three", 100).as_deref(),
            Some("one two three")
        );
    }

    /// `untrusted_content::wrap` keeps its own invisible-character list rather than
    /// importing this module's; this pins the two together so a character
    /// `display_text` drops can never lead a marker line past the fence's quote.
    #[test]
    fn every_invisible_character_display_text_drops_cannot_lead_a_marker_past_the_fence() {
        let dropped = ('\0'..=char::MAX).filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{2028}' | '\u{2029}')
                && (announce::is_control_or_invisible(*c) || announce::is_variation_selector(*c))
        });
        for c in dropped {
            let fenced = crate::utils::untrusted_content::wrap("peer ab12", &format!("{c}=== x"));
            assert!(
                fenced.contains(&format!("\n> {c}=== x\n")),
                "U+{:04X} led an unquoted marker line: {fenced}",
                c as u32
            );
        }
    }

    #[test]
    fn rfc3339_round_trips_to_the_second() {
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        let text = rfc3339_utc(time);
        assert_eq!(text, "2026-09-21T14:13:20Z");
        assert_eq!(parse_rfc3339(&text), Some(time));
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn hex_lower_is_lowercase_zero_padded() {
        assert_eq!(hex_lower(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn decode_hex_takes_hex_digit_pairs_and_nothing_else() {
        assert_eq!(decode_hex("00abFF"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(decode_hex(""), Some(vec![]));
        for text in ["abc", "+1", "-1", "0x", "zz", "a b", "é1"] {
            assert_eq!(decode_hex(text), None, "{text:?}");
        }
    }

    #[test]
    fn write_atomically_leaves_no_temp_file_and_replaces_content() {
        let tmp = TempDir::new("write-atomically");
        let path = tmp.path.join("nested").join("state.json");

        write_atomically(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        write_atomically(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        let blocked = tmp.path.join("file-not-dir");
        fs::write(&blocked, b"").unwrap();
        let err = write_atomically(&blocked.join("x.yaml"), b"x").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains(&blocked.display().to_string()), "{text}");
    }

    #[test]
    fn mesh_module_never_names_the_request_ctx() {
        let sources = rust_sources();
        // Assembled at runtime so this test's own text does not match the probes.
        let needles = [
            ["Request", "Context"].concat(),
            ["request", "_context"].concat(),
            ["crate::config::", "request", "_context"].concat(),
        ];
        for path in &sources {
            let source = fs::read_to_string(path).unwrap();
            for needle in &needles {
                assert!(
                    !source.contains(needle),
                    "{} must not reference {needle}",
                    path.display()
                );
            }
        }
        assert!(
            sources.len() >= 6,
            "expected the mesh sources, found {}",
            sources.len()
        );
        for name in ["snapshot.rs", "node.rs"] {
            assert!(
                sources
                    .iter()
                    .any(|path| path.file_name().is_some_and(|file| file == name)),
                "{name} must be among the scanned mesh sources"
            );
        }
    }

    /// The wire identifiers before the SCOPE rename (announce magic, destination
    /// application and aspect, LXMF type tags) must not survive anywhere under `src/mesh`
    /// nor in the mesh-facing files outside it that the redaction scan covers, the one
    /// permitted form being the foreign-magic decode vector that asserts they are refused.
    /// A status-card repo named after this program is not a wire identifier.
    #[test]
    fn no_source_under_mesh_spells_the_pre_scope_wire_identifiers() {
        let mut sources = rust_sources();
        let mesh = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("mesh");
        sources.extend(
            redaction_scan_sources()
                .into_iter()
                .filter(|path| !path.starts_with(&mesh)),
        );
        /// Whether a line spelling the needle is one of the permitted forms.
        type Allowed = fn(&str) -> bool;
        fn never(_: &str) -> bool {
            false
        }
        fn refused_foreign_magic(line: &str) -> bool {
            line.contains("::decode(") && line.contains("), None)")
        }
        fn card_repo_name(line: &str) -> bool {
            line.contains("\"repo\"") || line.contains("repo(") || line.contains("(\"name\"")
        }
        // Needles assembled at runtime so this test's own text does not match them.
        let needles: [(String, Allowed); 5] = [
            (["COY", "M"].concat(), refused_foreign_magic),
            (["\"coy", "ote\""].concat(), card_repo_name),
            (["coy", "ote.mesh"].concat(), never),
            (["coy", "ote.peer/"].concat(), never),
            (["coy", "ote.knock/"].concat(), never),
        ];
        let mut hits = Vec::new();
        for path in &sources {
            for (index, line) in fs::read_to_string(path).unwrap().lines().enumerate() {
                for (needle, allowed) in &needles {
                    if line.contains(needle.as_str()) && !allowed(line) {
                        hits.push(format!(
                            "{}:{}: spells {needle:?}",
                            path.display(),
                            index + 1
                        ));
                    }
                }
            }
        }
        assert!(hits.is_empty(), "{}", hits.join("\n"));
        for expected in ["announce.rs", "conformance/interop.rs", "repl/mesh.rs"] {
            assert!(
                sources.iter().any(|path| path.ends_with(expected)),
                "{expected} must be among the scanned sources"
            );
        }
    }

    /// A staged file is the peer's and stays until a person removes it: no production line
    /// under `src/mesh`, nor in the mesh-facing tool and REPL files, removes a file or
    /// directory it names through the inbox. The inbox's own removal is of its temp file.
    #[test]
    fn no_mesh_source_removes_an_inbox_path() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<PathBuf> = rust_sources()
            .into_iter()
            .filter(|path| {
                !path.components().any(|c| c.as_os_str() == "conformance")
                    && path.file_name().is_some_and(|name| name != "tests.rs")
            })
            .collect();
        sources.push(src.join("function").join("mesh.rs"));
        sources.push(src.join("repl").join("mesh.rs"));
        let removals = ["remove_file(", "remove_dir_all(", "remove_dir("];
        let inbox_tokens = ["inbox", "Inbox", ".staged", "Part::File"];
        let mut hits = Vec::new();
        let mut inbox_removals = Vec::new();
        for path in &sources {
            for (index, line) in production_code(&read_source(path)).iter().enumerate() {
                if !removals.iter().any(|call| line.contains(call)) {
                    continue;
                }
                if path.ends_with("inbox.rs") {
                    inbox_removals.push(line.trim().to_string());
                } else if inbox_tokens.iter().any(|token| line.contains(token)) {
                    hits.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
        assert_eq!(hits, Vec::<String>::new());
        assert_eq!(
            inbox_removals,
            ["let _ = fs::remove_file(&tmp);"],
            "inbox.rs removes only its own temp file"
        );
    }

    #[test]
    fn announce_doc_comments_name_the_scope_node() {
        let announce = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("mesh")
            .join("announce.rs");
        let text = fs::read_to_string(&announce).unwrap();
        for expected in ["SCOPE node", "SCOPE announce"] {
            assert!(text.contains(expected), "announce.rs must say {expected:?}");
        }
        for stale in ["Coyote node", "Coyote announce"] {
            assert!(!text.contains(stale), "announce.rs still says {stale:?}");
        }
    }

    /// Words a log line may not interpolate or pass as an argument. The list mirrors
    /// MESH-LOG-001: "a peer's message content or title, its `fields`, a knock introduction,
    /// an envoy question or answer, a status card or any text of one, nor the human's brief,
    /// objective or session name".
    const UNLOGGABLE_WORDS: [&str; 15] = [
        "content",
        "body",
        "brief",
        "objective",
        "session_name",
        "title",
        "fields",
        "intro",
        "question",
        "card",
        "hint",
        "text",
        "answer",
        "reply",
        "reply_text",
    ];
    /// Identifiers that carry a full identity or destination hash; each must be inside
    /// `short(` where it is logged.
    const HASH_WORDS: [&str; 11] = [
        "fingerprint",
        "hash",
        "destination_hash",
        "destination_hex",
        "identity_hex",
        "identity_hash",
        "address_hash",
        "source_identity",
        "source_destination",
        "identity",
        "destination",
    ];
    /// Calls that render a hash in full; the receiver or first argument decides.
    const HASH_CALLS: [&str; 2] = [".to_hex_string()", "hex_lower("];
    /// Receivers whose hex is a per-link, per-request or per-message identifier, which
    /// section 17 lets a log line carry in full.
    const FULL_HEX_RECEIVERS: [&str; 6] = [
        "link_id",
        "link",
        "request_id",
        "transient",
        "transient_id",
        "message_id",
    ];
    /// Names an error value goes by. The scan cannot look through an error's Display, so
    /// where one of these is interpolated or passed it must sit inside `redact_hashes(`,
    /// which is what section 17 promises of every sink that logs an error's text.
    const ERROR_WORDS: [&str; 6] = ["err", "e", "error", "why", "cause", "failure"];
    const LOG_MACROS: [&str; 5] = ["debug!(", "trace!(", "info!(", "warn!(", "error!("];

    /// Files the scan reads selectively: a file listed here is scanned only at the log
    /// invocations whose string literal mentions one of its markers, so a shared file's
    /// unrelated sinks are not swept in. The REPL completion file under config hosts the
    /// `.mesh` completion helpers among sinks section 17 does not govern; its name is
    /// assembled at runtime because the mesh module must not spell it out.
    fn literal_filtered_sources() -> Vec<(PathBuf, &'static [&'static str])> {
        let completion_file = Path::new("config").join(["request", "_context.rs"].concat());
        vec![(completion_file, &[".mesh", "mesh_completion"])]
    }

    /// The literal-filter markers of `path`, if it is a literal-filtered file.
    fn literal_markers(path: &Path) -> Option<&'static [&'static str]> {
        literal_filtered_sources()
            .into_iter()
            .find_map(|(suffix, markers)| path.ends_with(&suffix).then_some(markers))
    }

    /// The files section 17 governs: the mesh module without its test-only files, and the
    /// mesh-facing files under config, function and repl, plus the literal-filtered files.
    fn redaction_scan_sources() -> Vec<PathBuf> {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources: Vec<PathBuf> = rust_sources()
            .into_iter()
            .filter(|path| {
                !path.components().any(|c| c.as_os_str() == "conformance")
                    && path.file_name().is_some_and(|name| name != "tests.rs")
            })
            .collect();
        for entry in fs::read_dir(src.join("config")).unwrap() {
            let path = entry.unwrap().path();
            let is_mesh_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mesh_") && name.ends_with(".rs"));
            if is_mesh_file {
                sources.push(path);
            }
        }
        sources.push(src.join("function").join("mesh.rs"));
        sources.push(src.join("repl").join("mesh.rs"));
        for (suffix, _) in literal_filtered_sources() {
            sources.push(src.join(suffix));
        }
        sources
    }

    /// Whether the scan reads `invocation` found in `path`: every invocation of an
    /// unfiltered file, and of a literal-filtered file only those whose string literal
    /// mentions one of the file's markers.
    fn in_scan(path: &Path, invocation: &str) -> bool {
        let Some(markers) = literal_markers(path) else {
            return true;
        };
        split_literals(invocation)
            .iter()
            .any(|(_, literal)| markers.iter().any(|marker| literal.contains(marker)))
    }

    /// `source` line for line, so line numbers hold, with every `#[cfg(test)]`-attributed
    /// inline module blanked and `//` comments removed, a `//` inside a string literal kept.
    fn production_code(source: &str) -> Vec<String> {
        let mut lines: Vec<String> = source
            .lines()
            .map(|line| strip_line_comment(line).to_string())
            .collect();
        let mut at = 0;
        while at + 1 < lines.len() {
            let opener = &lines[at + 1];
            let opens_test_module = lines[at] == "#[cfg(test)]"
                && opener.trim_end().ends_with('{')
                && (opener.starts_with("mod ") || opener.starts_with("pub(crate) mod "));
            if !opens_test_module {
                at += 1;
                continue;
            }
            let rest = lines[at + 1..].join("\n");
            let open = rest.find('{').unwrap();
            let close = matching_bracket(&rest, open, (b'{', b'}'))
                .unwrap_or_else(|| panic!("line {}: no closing brace for the test module", at + 2));
            let end = at + 1 + rest[..=close].matches('\n').count();
            for line in &mut lines[at..=end] {
                line.clear();
            }
            at = end + 1;
        }
        lines
    }

    /// `line` up to its first `//` outside a string literal; a `'"'` char literal does not
    /// open one.
    fn strip_line_comment(line: &str) -> &str {
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if in_string => at += 1,
                b'\'' if !in_string && bytes.get(at + 2) == Some(&b'\'') => at += 2,
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(at + 1) == Some(&b'/') => return &line[..at],
                _ => {}
            }
            at += 1;
        }
        line
    }

    /// Every log macro invocation in `lines` as (1-based line, text from the macro name to
    /// its closing paren), string literals kept. An invocation whose closing paren is not
    /// found is a scanner fault, so it panics rather than scanning a guess.
    fn log_invocations(lines: &[String]) -> Vec<(usize, String)> {
        let joined = lines.join("\n");
        let mut found = Vec::new();
        let mut from = 0;
        while let Some((start, macro_name)) = LOG_MACROS
            .iter()
            .filter_map(|name| joined[from..].find(name).map(|at| (from + at, *name)))
            .min()
        {
            let boundary_before = start == 0
                || !joined.as_bytes()[start - 1].is_ascii_alphanumeric()
                    && joined.as_bytes()[start - 1] != b'_';
            if !boundary_before {
                from = start + macro_name.len();
                continue;
            }
            let open = start + macro_name.len() - 1;
            let line = joined[..start].matches('\n').count() + 1;
            let close = matching_paren(&joined, open)
                .unwrap_or_else(|| panic!("line {line}: no closing paren for `{macro_name}`"));
            found.push((line, joined[start..=close].to_string()));
            from = close + 1;
        }
        found
    }

    /// The index of the `)` closing the `(` at `open`.
    fn matching_paren(text: &str, open: usize) -> Option<usize> {
        matching_bracket(text, open, (b'(', b')'))
    }

    /// The index of the `pair.1` closing the `pair.0` at `open`, skipping brackets inside
    /// string and char literals.
    fn matching_bracket(text: &str, open: usize, pair: (u8, u8)) -> Option<usize> {
        let bytes = text.as_bytes();
        let mut depth = 0;
        let mut in_string = false;
        let mut at = open;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if in_string => at += 1,
                b'\'' if !in_string && bytes.get(at + 2) == Some(&b'\'') => at += 2,
                b'"' => in_string = !in_string,
                byte if !in_string && byte == pair.0 => depth += 1,
                byte if !in_string && byte == pair.1 => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ => {}
            }
            at += 1;
        }
        None
    }

    /// (code, string literal) pairs of an invocation; the last pair's literal is empty.
    fn split_literals(text: &str) -> Vec<(String, String)> {
        let mut parts = Vec::new();
        let mut code = String::new();
        let mut literal = String::new();
        let mut in_string = false;
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' if in_string => {
                    literal.push(c);
                    literal.extend(chars.next());
                }
                '"' if in_string => {
                    parts.push((std::mem::take(&mut code), std::mem::take(&mut literal)));
                    in_string = false;
                }
                '"' => in_string = true,
                _ if in_string => literal.push(c),
                _ => code.push(c),
            }
        }
        parts.push((code, String::new()));
        parts
    }

    fn is_ident_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    fn word_positions(text: &str, word: &str) -> Vec<usize> {
        let bytes = text.as_bytes();
        text.match_indices(word)
            .filter(|(at, _)| {
                at.checked_sub(1)
                    .is_none_or(|before| !is_ident_byte(bytes[before]))
                    && bytes
                        .get(at + word.len())
                        .is_none_or(|after| !is_ident_byte(*after))
            })
            .map(|(at, _)| at)
            .collect()
    }

    /// The argument expression starting at `at`: up to the next `,` or `)` at its own depth.
    fn argument_from(code: &str, at: usize) -> &str {
        let bytes = code.as_bytes();
        let mut depth = 0;
        let mut end = at;
        while end < bytes.len() {
            match bytes[end] {
                b'(' => depth += 1,
                b')' if depth > 0 => depth -= 1,
                b')' => break,
                b',' if depth == 0 => break,
                _ => {}
            }
            end += 1;
        }
        code[at..end].trim()
    }

    /// A tag, an id, a length or a party hash taken from a peer value is a count or a key,
    /// not its text; the hash rules hold the party hash to `short(`.
    fn is_tag_id_or_length(argument: &str) -> bool {
        [
            ".kind",
            ".id",
            ".message_id",
            ".source_identity",
            ".source_destination",
            ".len()",
            "len)",
        ]
        .iter()
        .any(|suffix| argument.ends_with(suffix))
    }

    /// `{name}` and `{name:?}` placeholders of a format string.
    fn placeholders(literal: &str) -> Vec<&str> {
        literal
            .split('{')
            .skip(1)
            .filter_map(|rest| {
                let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
                (rest[end..].starts_with('}') || rest[end..].starts_with(':')).then(|| &rest[..end])
            })
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// Whether any unclosed `(` before `at` is a call of `callee`.
    fn inside_call(code: &str, at: usize, callee: &str) -> bool {
        let bytes = code.as_bytes();
        let mut depth = 0;
        for index in (0..at).rev() {
            match bytes[index] {
                b')' => depth += 1,
                b'(' if depth > 0 => depth -= 1,
                b'(' if code[..index].ends_with(callee) => return true,
                _ => {}
            }
        }
        false
    }

    /// The last identifier segment of the receiver before `.to_hex_string()` at `at`, or of
    /// the first argument after `hex_lower(` at `at`.
    fn hex_subject(code: &str, at: usize, call: &str) -> String {
        let bytes = code.as_bytes();
        if call.starts_with('.') {
            let start = (0..at)
                .rev()
                .find(|index| !(is_ident_byte(bytes[*index]) || bytes[*index] == b'.'))
                .map_or(0, |index| index + 1);
            code[start..at].rsplit('.').next().unwrap_or("").to_string()
        } else {
            code[at + call.len()..]
                .trim_start_matches(['&', ' '])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
                .collect::<String>()
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_string()
        }
    }

    /// The section 17 violations in one log invocation.
    fn redaction_violations(invocation: &str) -> Vec<String> {
        let mut violations = Vec::new();
        let mut code_so_far = String::new();
        for (code, literal) in split_literals(invocation) {
            code_so_far.push_str(&code);
            let base = code_so_far.len() - code.len();
            let literal_is_redacted = inside_call(&code_so_far, code_so_far.len(), "redact_hashes");
            for name in placeholders(&literal) {
                if UNLOGGABLE_WORDS.contains(&name) {
                    violations.push(format!("interpolates `{{{name}}}`"));
                }
                if HASH_WORDS.contains(&name) {
                    violations.push(format!("interpolates the full hash `{{{name}}}`"));
                }
                if ERROR_WORDS.contains(&name) && !literal_is_redacted {
                    violations.push(format!(
                        "interpolates the error `{{{name}}}` outside redact_hashes()"
                    ));
                }
            }
            for word in UNLOGGABLE_WORDS {
                for at in word_positions(&code, word) {
                    if !is_tag_id_or_length(argument_from(&code, at)) {
                        violations.push(format!("passes `{word}`"));
                    }
                }
            }
            for word in HASH_WORDS {
                for at in word_positions(&code, word) {
                    if !inside_call(&code, at, "short") {
                        violations.push(format!("passes `{word}` outside short()"));
                    }
                }
            }
            for word in ERROR_WORDS {
                for at in word_positions(&code, word) {
                    let is_macro_name = code.as_bytes().get(at + word.len()) == Some(&b'!');
                    if !is_macro_name && !inside_call(&code_so_far, base + at, "redact_hashes") {
                        violations.push(format!("passes `{word}` outside redact_hashes()"));
                    }
                }
            }
            for call in HASH_CALLS {
                for (at, _) in code.match_indices(call) {
                    let subject = hex_subject(&code, at, call);
                    if !FULL_HEX_RECEIVERS.contains(&subject.as_str())
                        && !inside_call(&code, at, "short")
                    {
                        violations
                            .push(format!("renders `{subject}` with `{call}` outside short()"));
                    }
                }
            }
        }
        violations
    }

    #[test]
    fn redaction_scanner_flags_each_rule_and_passes_the_permitted_forms() {
        let flagged: [(&str, &[&str]); 19] = [
            ("debug!(\"got {content}\")", &["interpolates `{content}`"]),
            ("debug!(\"{content:?}\")", &["interpolates `{content}`"]),
            ("debug!(\"got {}\", message.title)", &["passes `title`"]),
            ("debug!(\"sent {}\", answer)", &["passes `answer`"]),
            ("debug!(\"{reply_text}\")", &["interpolates `{reply_text}`"]),
            (
                "warn!(\"knock from {fingerprint}\")",
                &["interpolates the full hash `{fingerprint}`"],
            ),
            (
                "debug!(\"aged out {hash}\")",
                &["interpolates the full hash `{hash}`"],
            ),
            (
                "debug!(\"peer {}\", identity_hex)",
                &["passes `identity_hex` outside short()"],
            ),
            (
                "info!(\"peer {}\", destination.address_hash.to_hex_string())",
                &[
                    "passes `address_hash` outside short()",
                    "passes `destination` outside short()",
                    "renders `address_hash` with `.to_hex_string()` outside short()",
                ],
            ),
            (
                "debug!(\"peer {}\", identity.to_hex_string())",
                &[
                    "passes `identity` outside short()",
                    "renders `identity` with `.to_hex_string()` outside short()",
                ],
            ),
            (
                "debug!(\"id {}\", hex_lower(advertised))",
                &["renders `advertised` with `hex_lower(` outside short()"],
            ),
            (
                "debug!(\n    \"multi {}\",\n    peer.destination_hash\n)",
                &["passes `destination_hash` outside short()"],
            ),
            (
                "debug!(\"{} // {}\", body.kind, message.content)",
                &["passes `content`"],
            ),
            (
                "debug!(\"from {}\", reply.source_identity)",
                &["passes `source_identity` outside short()"],
            ),
            (
                "warn!(\"knock was not cached: {err:#}\")",
                &["interpolates the error `{err}` outside redact_hashes()"],
            ),
            (
                "debug!(\"unreadable: {e:?}\")",
                &["interpolates the error `{e}` outside redact_hashes()"],
            ),
            (
                "error!(\"run failed: {why}\")",
                &["interpolates the error `{why}` outside redact_hashes()"],
            ),
            (
                "warn!(\"could not persist: {}\", err)",
                &["passes `err` outside redact_hashes()"],
            ),
            (
                "warn!(\"{}: {}\", short(&identity_hex), format!(\"{:#}\", cause))",
                &["passes `cause` outside redact_hashes()"],
            ),
        ];
        for (text, violations) in flagged {
            let invocations = log_invocations(&production_code(text));
            assert_eq!(invocations.len(), 1, "{text}");
            assert_eq!(
                redaction_violations(&invocations[0].1),
                violations,
                "{text}"
            );
        }
        let permitted = [
            "debug!(\"peer {}\", short(&identity_hex))",
            "debug!(\"peer {}\", short(&destination.address_hash.to_hex_string()))",
            "debug!(\"resource {}\", short(&crate::mesh::hex_lower(hash.as_slice())))",
            "debug!(\"link {}\", link_id.to_hex_string())",
            "debug!(\"request {} on link {}\", request_id.to_hex_string(), event.link_id.to_hex_string())",
            "debug!(\"transient {}\", hex_lower(transient))",
            "debug!(\"({} bytes) body-free text: {}\", count, len)",
            "debug!(\"{} {}\", body.kind, body.id)",
            "debug!(\"reply {} from {}\", reply.message_id, short(&reply.source_identity))",
            "debug!(\"{} bytes\", message.content.as_ref().map_or(0, Vec::len))",
            "debug!(\"{} chars\", intro.len())",
            "warn!(\"was not cached: {}\", redact_hashes(&format!(\"{err:#}\")))",
            "warn!(\"not persisted: {}\", redact_hashes(&err.to_string()))",
            "error!(\"run for {id} failed: {}\", redact_hashes(err))",
            "warn!(\"refused: {}\", redact_hashes(&why))",
            "debug!(\"{} {}\", short(&identity_hex), redact_hashes(&format!(\"{}: {e:?}\", label)))",
            "warn!(\"{}\", redact_hashes(&format!(\"{}: {}\", label, err)))",
        ];
        for text in permitted {
            let lines = vec![text.to_string()];
            for (_, invocation) in log_invocations(&lines) {
                assert_eq!(
                    redaction_violations(&invocation),
                    Vec::<String>::new(),
                    "{text}"
                );
            }
        }
        for opener in ["mod tests {", "pub(crate) mod x {"] {
            let source = format!(
                "fn a() {{\n    debug!(\"x\");\n}}\n#[cfg(test)]\n{opener}\n    debug!(\"{{content}}\");\n}}\n"
            );
            assert_eq!(
                log_invocations(&production_code(&source)).len(),
                1,
                "{opener}"
            );
        }
        let after_helpers = "#[cfg(test)]\nmod helpers {\n    fn a() {\n        let _ = '{';\n        debug!(\"{content} }}\");\n    }\n}\n\nfn b() {\n    debug!(\"{content}\");\n}\n";
        let invocations = log_invocations(&production_code(after_helpers));
        assert_eq!(
            invocations
                .iter()
                .map(|(line, invocation)| (*line, redaction_violations(invocation)))
                .collect::<Vec<_>>(),
            [(10, vec!["interpolates `{content}`".to_string()])]
        );
        assert_eq!(
            production_code("debug!(\"a // b\"); // debug!(\"{content}\")"),
            ["debug!(\"a // b\"); "]
        );
        assert_eq!(
            production_code("x.replace('\"', \"'\"); // debug!(\"{content}\")"),
            ["x.replace('\"', \"'\"); "]
        );
    }

    #[test]
    #[should_panic(expected = "no closing paren for `debug!(`")]
    fn redaction_scanner_refuses_an_unclosed_log_macro() {
        log_invocations(&["debug!(\"open".to_string()]);
    }

    #[test]
    #[should_panic(expected = "line 2: no closing brace for the test module")]
    fn redaction_scanner_refuses_an_unclosed_test_module() {
        production_code("#[cfg(test)]\nmod helpers {\n    fn a() {}\n");
    }

    #[test]
    fn redaction_scan_holds_the_completion_file_to_its_mesh_completion_sinks() {
        let (filtered, _) = literal_filtered_sources().remove(0);
        let filtered = Path::new("src").join(filtered);
        let unfiltered = Path::new("src/mesh/node.rs");
        let source = "fn a() {\n    warn!(\"failed to compute effective role: {err}\");\n    debug!(\"knock cache unreadable while completing `.mesh`: {err:#}\");\n    debug!(\"{}\", mesh_completion_label(err));\n    debug!(\"mesh_completion list unreadable: {}\", err);\n}\n";
        let invocations = log_invocations(&production_code(source));
        assert_eq!(invocations.len(), 4);
        let scanned = |path: &Path| -> Vec<(usize, Vec<String>)> {
            invocations
                .iter()
                .filter(|(_, invocation)| in_scan(path, invocation))
                .map(|(line, invocation)| (*line, redaction_violations(invocation)))
                .collect()
        };
        // In the completion file the non-mesh `{err}` sink (line 2) and the sink whose
        // marker sits in code rather than in a literal (line 4) are outside the scan; the
        // `.mesh` and `mesh_completion` literals (lines 3 and 5) are inside it and flagged.
        assert_eq!(
            scanned(&filtered),
            [
                (
                    3,
                    vec!["interpolates the error `{err}` outside redact_hashes()".to_string()]
                ),
                (5, vec!["passes `err` outside redact_hashes()".to_string()]),
            ]
        );
        // An unfiltered file is scanned at every sink.
        assert_eq!(scanned(unfiltered).len(), 4);
        assert!(
            scanned(unfiltered)
                .iter()
                .all(|(_, violations)| !violations.is_empty())
        );
    }

    #[test]
    fn mesh_log_lines_never_carry_peer_text_or_a_full_hash() {
        let sources = redaction_scan_sources();
        let mut hits = Vec::new();
        let mut repl_invocations = 0;
        let mut completion_file_invocations = 0;
        for path in &sources {
            let source = fs::read_to_string(path).unwrap();
            let invocations: Vec<(usize, String)> = log_invocations(&production_code(&source))
                .into_iter()
                .filter(|(_, invocation)| in_scan(path, invocation))
                .collect();
            if path.ends_with("repl/mesh.rs") {
                repl_invocations = invocations.len();
            }
            if literal_markers(path).is_some() {
                completion_file_invocations = invocations.len();
            }
            for (line, invocation) in invocations {
                for violation in redaction_violations(&invocation) {
                    hits.push(format!("{}:{line}: {violation}", path.display()));
                }
            }
        }
        assert!(
            repl_invocations > 0,
            "the scan must reach the production log line in src/repl/mesh.rs"
        );
        assert!(
            sources
                .iter()
                .any(|path| path.ends_with("config/mesh_envoy.rs")),
            "the scan must reach the mesh-facing config files"
        );
        assert!(
            completion_file_invocations > 0,
            "the scan must reach the `.mesh` completion sinks in the REPL completion file"
        );
        assert_eq!(hits, Vec::<String>::new(), "log lines violating section 17");
    }
}
