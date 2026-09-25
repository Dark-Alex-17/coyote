#![deny(unsafe_code)]

mod announce;
pub(crate) mod card;
mod identity;
pub(crate) mod idle;
pub(crate) mod knock;
pub(crate) mod knocks;
mod lock;
mod node;
pub(crate) mod notify;
mod peers;
mod propagation;
mod propagation_fetch;
mod propagation_nodes;
mod r3;
pub(crate) mod snapshot;
pub(crate) mod trust;

pub(crate) use node::MeshSlot;

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

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
/// every other control character a space, the invisible formatting characters and
/// variation selectors dropped, trimmed, and cut to `max_chars` characters on a character
/// boundary with no trailing whitespace. Blank text is `None`. Escapes go first so a
/// sequence's own bytes never survive as spaces.
pub(crate) fn display_text(text: &str, max_chars: usize) -> Option<String> {
    let cleaned: String = sanitize_display_text(text)
        .chars()
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
    use super::node::MeshPaths;
    #[cfg(unix)]
    use super::node::{MeshRuntime, NodeOptions};
    #[cfg(unix)]
    use super::r3::{R3Client, R3Server, RequestHandler};
    use super::snapshot::{BriefState, MeshSnapshot, SessionInfo, TurnState};
    use super::trust::TrustStore;
    use super::{mesh_config_dir, rfc3339_utc};
    use crate::config::MeshConfig;
    #[cfg(unix)]
    use crate::config::Session;
    use crate::config::mesh_config::{MeshBrief, MeshInterface};
    use crate::config::todo::TodoList;

    #[cfg(unix)]
    use rand_core::OsRng;
    #[cfg(unix)]
    use rns_transport::destination::{DestinationDesc, DestinationName, SingleInputDestination};
    #[cfg(unix)]
    use rns_transport::hash::AddressHash;
    #[cfg(unix)]
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    #[cfg(unix)]
    use rns_transport::iface::tcp_client::TcpClient;
    #[cfg(unix)]
    use rns_transport::iface::tcp_server::TcpServer;
    #[cfg(unix)]
    use rns_transport::transport::{AnnounceEvent, Transport, TransportConfig};
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
    /// starting a runtime mints an owner-only identity file, which only unix implements.
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
            let mut text = String::from("version: 1\n");
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
        let tmp = TempDir::new(tag);
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
    use super::test_support::{TempDir, rust_sources};
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
}
