//! How many other hosts the node's LAN interfaces reach, for `.mesh status`, and whether
//! the local rnsd sits on a TCP relay, for the silent-LAN note under an empty result.
//! The in-process `lan` interface reads its discovery table through the
//! transport; a `local` rnsd is asked over the RPC listener its shared instance opens,
//! as rnstatus asks it, and its AutoInterfaces are summed. The same rnsd answer says
//! whether rnsd sits on a TCP relay.

use crate::mesh::redact_hashes;
use async_trait::async_trait;
use log::debug;
use rns_transport::hash::AddressHash;
use rns_transport::transport::Transport;
use rns_transport::transport::shared_instance_rpc::{self, RpcEndpoint, RpcError, RpcKey};
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

/// Budget for one stats call, connect and handshake included.
const RPC_BUDGET: Duration = Duration::from_secs(5);
/// Budget for the one call a command waiting on the count makes.
const QUICK_RPC_BUDGET: Duration = Duration::from_secs(2);
/// Calls one read makes before it leaves the count unknown for that read.
const RPC_ATTEMPTS: u32 = 3;
const RPC_RETRY_WAIT: Duration = Duration::from_secs(1);
/// rnsd's `instance_control_port` when its config sets none.
const DEFAULT_CONTROL_PORT: u16 = 37429;
/// The class rnsd names an AutoInterface by. The per-host interfaces it spawns report
/// another class, so only the parent entries are counted.
const AUTO_INTERFACE_KIND: &str = "AutoInterface";
/// The class rnsd names an outbound TCP connection by, a relay it dials.
const TCP_CLIENT_INTERFACE_KIND: &str = "TCPClientInterface";
/// The system-wide config dir rnsd and scripts/mesh-relay.sh look in first.
const ETC_CONFIG_DIR: &str = "/etc/reticulum";

/// What one LAN interface reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LanReading {
    /// Other hosts its discovery currently holds.
    Count(u64),
    /// The count could not be read this time.
    Unknown,
}

/// One read of every LAN interface: each one's reading, and whether the local rnsd
/// reported a TCP client interface online.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LanReadings {
    pub readings: Vec<LanReading>,
    pub relay_online: bool,
}

/// One read of the local rnsd: its AutoInterfaces' summed count, `None` when it runs
/// none, and whether any of its TCP client interfaces is online.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RnsdReading {
    pub lan: Option<LanReading>,
    pub relay_online: bool,
}

impl RnsdReading {
    const UNKNOWN: Self = Self {
        lan: Some(LanReading::Unknown),
        relay_online: false,
    };
}

/// How hard one read of the local rnsd tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadEffort {
    /// The silent-LAN check: every attempt, each on the full budget, waiting its turn.
    Patient,
    /// A command the user waits on: one call on a short budget, or the last count read
    /// when another read is asking rnsd at that moment.
    Quick,
}

/// The hosts every readable LAN interface reaches together; `None` when no reading is a
/// count.
pub(crate) fn known_host_total(readings: &[LanReading]) -> Option<u64> {
    readings
        .iter()
        .filter_map(|reading| match reading {
            LanReading::Count(hosts) => Some(*hosts),
            LanReading::Unknown => None,
        })
        .reduce(u64::saturating_add)
}

/// Each LAN interface's reading: every in-process `lan` host channel in `lan_hosts`,
/// then the local rnsd's AutoInterfaces when `rnsd` is set and runs any. A stopped
/// transport reads nothing for its channels, since a stopped discovery leaves its last
/// count behind. `relay_online` is the rnsd's, false without one.
pub(crate) async fn lan_readings(
    transport: Option<&Transport>,
    lan_hosts: &[AddressHash],
    rnsd: Option<&RnsdLanReader>,
    effort: ReadEffort,
) -> LanReadings {
    let mut readings = Vec::new();
    if let Some(transport) = transport {
        for host in lan_hosts {
            readings.push(match transport.interface_peer_count(host).await {
                Some(hosts) => LanReading::Count(hosts),
                None => LanReading::Unknown,
            });
        }
    }
    let mut relay_online = false;
    if let Some(reader) = rnsd {
        let rnsd = match effort {
            ReadEffort::Patient => reader.read().await,
            ReadEffort::Quick => reader.read_quick().await,
        };
        readings.extend(rnsd.lan);
        relay_online = rnsd.relay_online;
    }
    LanReadings {
        readings,
        relay_online,
    }
}

/// Where the local rnsd's config dir is. `dir_override` is for tests, which point it at
/// an rnsd of their own; it is never read from the user's config or environment.
#[derive(Debug, Clone, Default)]
pub(crate) struct RnsdLocator {
    pub dir_override: Option<PathBuf>,
}

impl RnsdLocator {
    fn config_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.dir_override {
            return Some(dir.clone());
        }
        // A test that names no rnsd of its own never reads the developer's.
        if cfg!(test) {
            return None;
        }
        dirs::home_dir().map(|home| default_config_dir(Path::new(ETC_CONFIG_DIR), &home))
    }
}

/// The order rnsd and scripts/mesh-relay.sh look in: `etc` when it holds a `config`,
/// then `~/.config/reticulum` when that does, else `~/.reticulum`.
fn default_config_dir(etc: &Path, home: &Path) -> PathBuf {
    if etc.join("config").is_file() {
        return etc.to_path_buf();
    }
    let user = home.join(".config").join("reticulum");
    if user.join("config").is_file() {
        return user;
    }
    home.join(".reticulum")
}

/// The `[reticulum]` keys that say where rnsd's RPC listener is and what key it takes.
#[derive(Debug, Default, PartialEq, Eq)]
struct RnsdSettings {
    rpc_key: Option<String>,
    control_port: Option<u16>,
    /// `shared_instance_type = tcp`, which keeps the listener on TCP where rnsd would
    /// otherwise use an abstract socket.
    tcp_instance: bool,
}

/// Reads the keys `RnsdSettings` holds from an rnsd config, ConfigObj style: `[section]`
/// headers, `[[subsection]]` headers whose keys belong to the subsection, `key = value`
/// lines and `#` comments.
fn parse_settings(config: &str) -> RnsdSettings {
    let mut settings = RnsdSettings::default();
    let mut in_reticulum = false;
    for line in config.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            in_reticulum = !line.starts_with("[[")
                && line.trim_start_matches('[').trim_end_matches(']').trim() == "reticulum";
            continue;
        }
        if !in_reticulum {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "rpc_key" => settings.rpc_key = Some(value.to_string()),
            "instance_control_port" => settings.control_port = value.parse().ok(),
            "shared_instance_type" => settings.tcp_instance = value.eq_ignore_ascii_case("tcp"),
            _ => {}
        }
    }
    settings
}

/// Where rnsd's RPC listener is. rnsd binds the abstract socket `rns/<instance>/rpc` on
/// Linux and Android unless its shared instance is `tcp`, and loopback TCP on its
/// control port everywhere else.
fn rpc_endpoint(abstract_sockets: bool, settings: &RnsdSettings, instance: &str) -> RpcEndpoint {
    if abstract_sockets && !settings.tcp_instance {
        return RpcEndpoint::for_instance(instance);
    }
    RpcEndpoint::Tcp(SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        settings.control_port.unwrap_or(DEFAULT_CONTROL_PORT),
    )))
}

/// The endpoint and key for the rnsd configured in `dir`, or why it cannot be asked. A
/// dir without a `config` file runs on rnsd's defaults. The key is the configured
/// `rpc_key`, else the one rnsd derives from its transport identity; a missing identity
/// file is refused here so the key read never logs it as unreadable.
fn resolve_target(
    dir: &Path,
    instance: &str,
    abstract_sockets: bool,
) -> Result<(RpcEndpoint, RpcKey), String> {
    if !dir.is_dir() {
        return Err(format!("no rnsd config directory at {}", dir.display()));
    }
    let config = dir.join("config");
    let settings = fs::read_to_string(&config)
        .map(|text| parse_settings(&text))
        .unwrap_or_default();
    let key = match &settings.rpc_key {
        Some(hex) => RpcKey::from_hex(hex)
            .map_err(|err| format!("the rpc_key in {} is not usable: {err}", config.display()))?,
        None => {
            let identity = dir.join("storage").join("transport_identity");
            if !identity.is_file() {
                return Err(format!("no transport identity at {}", identity.display()));
            }
            RpcKey::from_transport_identity_file(&identity).map_err(|err| {
                format!(
                    "the transport identity at {} is not usable: {err}",
                    identity.display()
                )
            })?
        }
    };
    Ok((rpc_endpoint(abstract_sockets, &settings, instance), key))
}

/// The interface kind, host count and whether it is up, of one entry of rnsd's stats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterfaceHosts {
    pub kind: String,
    pub hosts: Option<u64>,
    pub online: bool,
}

/// Where rnsd's interface stats come from: its RPC listener, or a test's script.
#[async_trait]
pub(crate) trait LanStatsSource: Send + Sync {
    async fn interface_hosts(
        &self,
        endpoint: &RpcEndpoint,
        key: &RpcKey,
        budget: Duration,
    ) -> Result<Vec<InterfaceHosts>, RpcError>;
}

struct SharedInstanceRpc;

#[async_trait]
impl LanStatsSource for SharedInstanceRpc {
    async fn interface_hosts(
        &self,
        endpoint: &RpcEndpoint,
        key: &RpcKey,
        budget: Duration,
    ) -> Result<Vec<InterfaceHosts>, RpcError> {
        let stats = shared_instance_rpc::interface_stats(endpoint, key, budget).await?;
        Ok(stats
            .interfaces
            .into_iter()
            .map(|stat| InterfaceHosts {
                kind: stat.kind,
                hosts: stat.peers,
                online: stat.online,
            })
            .collect())
    }
}

enum RpcTarget {
    Unresolved,
    Ready {
        dir: PathBuf,
        endpoint: RpcEndpoint,
        key: RpcKey,
    },
    /// Asking again would fail the same way, so nothing is asked for the rest of the run.
    Unavailable,
}

/// The local rnsd's LAN host count. Reads are serialised on `target`, each opens one
/// connection per call, and the config is resolved on the first read rather than at
/// node start, by when rnsd has written its transport identity.
pub(crate) struct RnsdLanReader {
    locator: RnsdLocator,
    instance: String,
    source: Arc<dyn LanStatsSource>,
    retry_wait: Duration,
    target: Mutex<RpcTarget>,
    /// What the latest read that reached rnsd returned, for a quick read that finds
    /// another read under way.
    last: parking_lot::Mutex<RnsdReading>,
}

impl RnsdLanReader {
    /// A reader for the rnsd whose shared instance is named `instance`, the `local`
    /// entry's.
    pub(crate) fn new(locator: RnsdLocator, instance: &str) -> Self {
        Self::with_source(
            locator,
            instance,
            Arc::new(SharedInstanceRpc),
            RPC_RETRY_WAIT,
        )
    }

    fn with_source(
        locator: RnsdLocator,
        instance: &str,
        source: Arc<dyn LanStatsSource>,
        retry_wait: Duration,
    ) -> Self {
        Self {
            locator,
            instance: instance.to_string(),
            source,
            retry_wait,
            target: Mutex::new(RpcTarget::Unresolved),
            last: parking_lot::Mutex::new(RnsdReading::default()),
        }
    }

    /// The summed count of rnsd's AutoInterfaces, `None` when rnsd runs none, so it has
    /// no LAN to count, and whether a TCP client interface is online. A connect, a
    /// timeout or a dropped connection is tried again `RPC_ATTEMPTS` times in all before
    /// this read is `Unknown` with no relay online; a key rnsd refuses, or a config that
    /// names none, makes every read of the run read so without a call, and is told once
    /// at debug.
    pub(crate) async fn read(&self) -> RnsdReading {
        self.read_with(ReadEffort::Patient).await
    }

    /// `read` for a command the user waits on: one call on `QUICK_RPC_BUDGET`, and the
    /// last read's result without a call while another read is asking rnsd.
    pub(crate) async fn read_quick(&self) -> RnsdReading {
        self.read_with(ReadEffort::Quick).await
    }

    async fn read_with(&self, effort: ReadEffort) -> RnsdReading {
        let (attempts, budget) = match effort {
            ReadEffort::Patient => (RPC_ATTEMPTS, RPC_BUDGET),
            ReadEffort::Quick => (1, QUICK_RPC_BUDGET),
        };
        let mut target = match effort {
            ReadEffort::Patient => self.target.lock().await,
            ReadEffort::Quick => match self.target.try_lock() {
                Ok(target) => target,
                Err(_) => return *self.last.lock(),
            },
        };
        if matches!(*target, RpcTarget::Unresolved) {
            let dir = self.locator.config_dir();
            let resolved = dir
                .ok_or_else(|| "no home directory to look for rnsd's config in".to_string())
                .and_then(|dir| {
                    let abstract_sockets = cfg!(any(target_os = "linux", target_os = "android"));
                    resolve_target(&dir, &self.instance, abstract_sockets)
                        .map(|(endpoint, key)| (dir, endpoint, key))
                });
            *target = match resolved {
                Ok((dir, endpoint, key)) => RpcTarget::Ready { dir, endpoint, key },
                Err(reason) => {
                    tell_unavailable(&reason);
                    RpcTarget::Unavailable
                }
            };
        }
        let RpcTarget::Ready { dir, endpoint, key } = &*target else {
            return RnsdReading::UNKNOWN;
        };
        let mut attempt = 1;
        let err = loop {
            match self.source.interface_hosts(endpoint, key, budget).await {
                Ok(entries) => {
                    let reading = RnsdReading {
                        lan: auto_interface_hosts(&entries),
                        relay_online: entries
                            .iter()
                            .any(|entry| entry.kind == TCP_CLIENT_INTERFACE_KIND && entry.online),
                    };
                    *self.last.lock() = reading;
                    return reading;
                }
                Err(err) if attempt < attempts && retryable(&err) => {
                    attempt += 1;
                    sleep(self.retry_wait).await;
                }
                Err(err) => break err,
            }
        };
        if refuses_for_the_run(&err) {
            tell_unavailable(&format!(
                "{err} at the rnsd configured in {}",
                dir.display()
            ));
            *target = RpcTarget::Unavailable;
        } else {
            debug!(
                "Mesh read no LAN count from the rnsd configured in {} this time: {}",
                dir.display(),
                redact_hashes(&err.to_string())
            );
        }
        RnsdReading::UNKNOWN
    }

    /// Whether the latest read that reached rnsd had a TCP client online, with no call;
    /// `false` before any read reached it.
    pub(crate) fn relay_online_seen(&self) -> bool {
        self.last.lock().relay_online
    }

    /// Whether the latest read that reached rnsd found an AutoInterface, with no call;
    /// `false` before any read reached it.
    pub(crate) fn lan_seen(&self) -> bool {
        self.last.lock().lan.is_some()
    }

    #[cfg(test)]
    pub(crate) async fn unavailable(&self) -> bool {
        matches!(*self.target.lock().await, RpcTarget::Unavailable)
    }
}

fn tell_unavailable(reason: &str) {
    debug!(
        "Mesh could not read the local rnsd's interface stats: {}; the LAN row reads no count there and a TCP client of that rnsd is not counted as a relay",
        redact_hashes(reason)
    );
}

/// What goes away by itself: rnsd not listening yet, slow, or dropping the connection.
fn retryable(err: &RpcError) -> bool {
    match err {
        RpcError::Connect(_) | RpcError::Timeout => true,
        RpcError::Protocol(message) => message.to_ascii_lowercase().contains("reset"),
        _ => false,
    }
}

/// What every later call would fail with too: the key, or the digest rnsd asks for.
fn refuses_for_the_run(err: &RpcError) -> bool {
    matches!(
        err,
        RpcError::Authentication | RpcError::InvalidKeyMaterial | RpcError::UnsupportedDigest(_)
    )
}

/// The hosts rnsd's AutoInterfaces hold together: `None` without an AutoInterface,
/// `Unknown` when one reports no count.
fn auto_interface_hosts(entries: &[InterfaceHosts]) -> Option<LanReading> {
    let mut auto = entries
        .iter()
        .filter(|entry| entry.kind == AUTO_INTERFACE_KIND)
        .peekable();
    auto.peek()?;
    Some(
        auto.map(|entry| entry.hosts)
            .try_fold(0u64, |total, hosts| {
                hosts.map(|hosts| total.saturating_add(hosts))
            })
            .map_or(LanReading::Unknown, LanReading::Count),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_support::TempDir;
    use std::collections::VecDeque;

    const KEY_HEX: &str = "00112233445566778899aabbccddeeff";

    type Reply = Result<Vec<InterfaceHosts>, RpcError>;

    /// Answers each call with the next scripted reply and records each call's budget and
    /// when it came.
    struct Scripted {
        replies: parking_lot::Mutex<VecDeque<Reply>>,
        budgets: parking_lot::Mutex<Vec<Duration>>,
        asked_at: parking_lot::Mutex<Vec<tokio::time::Instant>>,
        /// Held by a test to keep a call in flight until the test lets it answer.
        gate: Mutex<()>,
    }

    impl Scripted {
        fn new(replies: impl IntoIterator<Item = Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: parking_lot::Mutex::new(replies.into_iter().collect()),
                budgets: parking_lot::Mutex::new(Vec::new()),
                asked_at: parking_lot::Mutex::new(Vec::new()),
                gate: Mutex::new(()),
            })
        }

        fn calls(&self) -> usize {
            self.budgets.lock().len()
        }

        /// The time between each call and the next, in call order.
        fn gaps(&self) -> Vec<Duration> {
            let asked_at = self.asked_at.lock();
            asked_at.windows(2).map(|pair| pair[1] - pair[0]).collect()
        }
    }

    #[async_trait]
    impl LanStatsSource for Scripted {
        async fn interface_hosts(
            &self,
            _endpoint: &RpcEndpoint,
            _key: &RpcKey,
            budget: Duration,
        ) -> Reply {
            self.budgets.lock().push(budget);
            self.asked_at.lock().push(tokio::time::Instant::now());
            let _open = self.gate.lock().await;
            self.replies
                .lock()
                .pop_front()
                .expect("a reply scripted for every call")
        }
    }

    fn entry(kind: &str, hosts: Option<u64>) -> InterfaceHosts {
        InterfaceHosts {
            kind: kind.to_string(),
            hosts,
            online: true,
        }
    }

    fn auto(hosts: u64) -> InterfaceHosts {
        entry(AUTO_INTERFACE_KIND, Some(hosts))
    }

    fn tcp_client(online: bool) -> InterfaceHosts {
        InterfaceHosts {
            online,
            ..entry(TCP_CLIENT_INTERFACE_KIND, None)
        }
    }

    fn refused() -> RpcError {
        RpcError::Connect(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    }

    fn rnsd_dir(tag: &str, config: &str) -> TempDir {
        let dir = TempDir::new(tag);
        fs::write(dir.path.join("config"), config).unwrap();
        dir
    }

    fn keyed_dir(tag: &str) -> TempDir {
        rnsd_dir(tag, &format!("[reticulum]\n  rpc_key = {KEY_HEX}\n"))
    }

    fn reader_at(dir: &Path, source: Arc<Scripted>) -> RnsdLanReader {
        RnsdLanReader::with_source(
            RnsdLocator {
                dir_override: Some(dir.to_path_buf()),
            },
            "default",
            source,
            Duration::ZERO,
        )
    }

    fn unavailable_lines(dir: &Path) -> usize {
        let dir = dir.display().to_string();
        crate::testing::debug_snapshot()
            .iter()
            .filter(|line| {
                line.starts_with("Mesh could not read the local rnsd's interface stats")
                    && line.contains(&dir)
            })
            .count()
    }

    #[tokio::test]
    async fn an_auto_interface_that_holds_no_host_reads_zero() {
        let dir = keyed_dir("lan-count-zero");
        let source = Scripted::new([Ok(vec![entry("LocalServerInterface", None), auto(0)])]);
        let reader = reader_at(&dir.path, source.clone());
        assert_eq!(reader.read().await.lan, Some(LanReading::Count(0)));
        assert_eq!(source.calls(), 1);
    }

    /// rnsd on a TCP relay says so in the same stats: a `TCPClientInterface` online sets
    /// the flag, one offline or a TCP server does not, and a quick read that finds
    /// another read under way answers with the flag the last read saw.
    #[tokio::test]
    async fn the_reader_reports_whether_a_tcp_client_interface_is_online() {
        let dir = keyed_dir("lan-count-tcp-client");
        let source = Scripted::new([
            Ok(vec![auto(0), tcp_client(true)]),
            Ok(vec![
                auto(0),
                tcp_client(false),
                entry("TCPServerInterface", None),
            ]),
            Ok(vec![tcp_client(true)]),
        ]);
        let reader = Arc::new(reader_at(&dir.path, source.clone()));
        assert_eq!(
            reader.read().await,
            RnsdReading {
                lan: Some(LanReading::Count(0)),
                relay_online: true,
            }
        );
        let held = source.gate.lock().await;
        let patient = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read().await }
        });
        while source.calls() < 2 {
            tokio::task::yield_now().await;
        }
        assert!(reader.read_quick().await.relay_online, "the cached flag");
        drop(held);
        assert_eq!(
            patient.await.unwrap(),
            RnsdReading {
                lan: Some(LanReading::Count(0)),
                relay_online: false,
            }
        );
        let readings = lan_readings(None, &[], Some(&*reader), ReadEffort::Patient).await;
        assert_eq!(
            readings,
            LanReadings {
                readings: Vec::new(),
                relay_online: true,
            },
            "an rnsd with no AutoInterface still reports its relay"
        );
    }

    /// The rnsd's counts reach the `.mesh status` LAN row through `lan_readings`, and the
    /// relay flag each read saw is kept for the silent-LAN note, read with no call.
    #[tokio::test]
    async fn the_rnsd_counts_are_read_and_the_relay_flag_is_kept() {
        let dir = keyed_dir("lan-count-rnsd-verdict");
        let source = Scripted::new([Ok(vec![auto(0)]), Ok(vec![auto(1), tcp_client(true)])]);
        let reader = reader_at(&dir.path, source);
        assert!(
            !reader.relay_online_seen() && !reader.lan_seen(),
            "nothing read yet"
        );

        let readings = lan_readings(None, &[], Some(&reader), ReadEffort::Patient).await;
        assert!(!readings.relay_online);
        assert_eq!(readings.readings, [LanReading::Count(0)]);
        assert!(!reader.relay_online_seen());
        assert!(reader.lan_seen(), "an AutoInterface was read");

        let readings = lan_readings(None, &[], Some(&reader), ReadEffort::Patient).await;
        assert_eq!(readings.readings, [LanReading::Count(1)]);
        assert!(readings.relay_online);
        assert!(reader.relay_online_seen());
    }

    /// The quick read a `.mesh people list` takes keeps the relay flag the note reads:
    /// a TCP client online reads as a relay, one offline does not.
    #[tokio::test]
    async fn a_quick_read_keeps_the_relay_flag_the_note_reads() {
        for relay_online in [true, false] {
            let dir = keyed_dir("lan-count-tick-quiet");
            let source = Scripted::new([Ok(vec![auto(0), tcp_client(relay_online)])]);
            let reader = reader_at(&dir.path, source);
            lan_readings(None, &[], Some(&reader), ReadEffort::Quick).await;
            assert_eq!(reader.relay_online_seen(), relay_online);
        }
    }

    /// A reader on the real retry wait, at `dir`.
    fn paced_reader_at(dir: &Path, source: Arc<Scripted>) -> RnsdLanReader {
        RnsdLanReader::with_source(
            RnsdLocator {
                dir_override: Some(dir.to_path_buf()),
            },
            "default",
            source,
            RPC_RETRY_WAIT,
        )
    }

    /// The reads the silent-LAN note rests on, on a paused clock, counting rnsd calls:
    /// the one patient read at join makes at most `RPC_ATTEMPTS` calls, `RPC_RETRY_WAIT`
    /// apart, and stops at the first answer; nothing reads rnsd again until the user asks.
    /// Each `.mesh people list` (or `.mesh status`) makes one call on `QUICK_RPC_BUDGET`,
    /// or none while another read is under way, answering with the last reading then. A
    /// key rnsd refuses ends the calls for the run.
    #[tokio::test(start_paused = true)]
    async fn the_reads_behind_the_silent_lan_note_bound_their_rnsd_calls() {
        let dir = keyed_dir("lan-count-tick-told");
        let source = Scripted::new([
            Err(refused()),
            Err(RpcError::Timeout),
            Err(refused()),
            Ok(vec![auto(0), tcp_client(true)]),
            Ok(vec![auto(0), tcp_client(false)]),
        ]);
        let reader = paced_reader_at(&dir.path, source.clone());
        assert_eq!(
            reader.read().await,
            RnsdReading::UNKNOWN,
            "the read at join gave up after its attempts"
        );
        assert_eq!(source.calls(), RPC_ATTEMPTS as usize);
        assert_eq!(source.gaps(), [RPC_RETRY_WAIT; 2]);
        assert_eq!(*source.budgets.lock(), [RPC_BUDGET; 3]);
        assert!(!reader.relay_online_seen());

        let idle = Duration::from_secs(3600);
        tokio::time::advance(idle).await;
        assert_eq!(source.calls(), 3, "nothing reads rnsd while nobody asks");
        assert!(reader.read_quick().await.relay_online, "the first ask");
        assert!(reader.relay_online_seen());
        assert_eq!(source.calls(), 4, "one call per ask");
        assert_eq!(source.gaps()[2], idle);
        assert!(!reader.read_quick().await.relay_online, "the second ask");
        assert!(!reader.relay_online_seen());
        assert_eq!(source.calls(), 5);
        assert_eq!(
            *source.budgets.lock(),
            [
                RPC_BUDGET,
                RPC_BUDGET,
                RPC_BUDGET,
                QUICK_RPC_BUDGET,
                QUICK_RPC_BUDGET
            ]
        );

        let dir = keyed_dir("lan-count-tick-refused");
        let source = Scripted::new([Err(RpcError::Authentication)]);
        let reader = paced_reader_at(&dir.path, source.clone());
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        for _ in 0..3 {
            tokio::time::advance(idle).await;
            assert_eq!(reader.read_quick().await, RnsdReading::UNKNOWN);
        }
        assert_eq!(source.calls(), 1, "no call after the key was refused");
        assert!(!reader.relay_online_seen());

        let dir = keyed_dir("lan-count-tick-status");
        let source = Scripted::new([Err(refused()), Ok(vec![auto(3)]), Ok(vec![auto(1)])]);
        let reader = Arc::new(paced_reader_at(&dir.path, source.clone()));
        assert_eq!(reader.read_quick().await, RnsdReading::UNKNOWN);
        assert_eq!(
            *source.budgets.lock(),
            [QUICK_RPC_BUDGET],
            "one call, no retry"
        );
        assert_eq!(reader.read_quick().await.lan, Some(LanReading::Count(3)));
        let held = source.gate.lock().await;
        let tick = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read().await }
        });
        while source.calls() < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            reader.read_quick().await.lan,
            Some(LanReading::Count(3)),
            "the cached count while a tick's read is under way"
        );
        assert_eq!(source.calls(), 3);
        drop(held);
        assert_eq!(tick.await.unwrap().lan, Some(LanReading::Count(1)));
    }

    #[tokio::test]
    async fn the_hosts_of_every_auto_interface_are_summed_and_their_spawned_entries_are_not() {
        let dir = keyed_dir("lan-count-sum");
        let source = Scripted::new([Ok(vec![
            auto(2),
            entry("AutoInterfacePeer", Some(5)),
            auto(1),
            entry("TCPServerInterface", Some(9)),
        ])]);
        let reader = reader_at(&dir.path, source);
        assert_eq!(reader.read().await.lan, Some(LanReading::Count(3)));
    }

    #[tokio::test]
    async fn an_rnsd_without_an_auto_interface_has_no_lan_to_count() {
        let dir = keyed_dir("lan-count-no-auto");
        let source = Scripted::new([Ok(vec![
            entry("LocalServerInterface", None),
            entry("TCPServerInterface", None),
        ])]);
        let reader = reader_at(&dir.path, source);
        assert_eq!(reader.read().await, RnsdReading::default());
    }

    #[tokio::test]
    async fn an_auto_interface_reported_without_a_count_reads_unknown() {
        let dir = keyed_dir("lan-count-no-count");
        let source = Scripted::new([Ok(vec![auto(2), entry(AUTO_INTERFACE_KIND, None)])]);
        let reader = reader_at(&dir.path, source);
        assert_eq!(reader.read().await.lan, Some(LanReading::Unknown));
    }

    #[tokio::test]
    async fn a_refused_connect_is_tried_again_within_the_read() {
        let dir = keyed_dir("lan-count-retry");
        let source = Scripted::new([Err(refused()), Err(refused()), Ok(vec![auto(2)])]);
        let reader = reader_at(&dir.path, source.clone());
        assert_eq!(reader.read().await.lan, Some(LanReading::Count(2)));
        assert_eq!(source.calls(), 3);
        assert_eq!(*source.budgets.lock(), [RPC_BUDGET; 3]);
        assert!(!reader.unavailable().await);
    }

    /// `.mesh status` waits on a quick read: one call on the short budget, no retry, and
    /// the next quick read asks again.
    #[tokio::test]
    async fn a_quick_read_calls_once_on_the_short_budget() {
        let dir = keyed_dir("lan-count-quick");
        let source = Scripted::new([Err(refused()), Ok(vec![auto(2)])]);
        let reader = reader_at(&dir.path, source.clone());
        assert_eq!(reader.read_quick().await, RnsdReading::UNKNOWN);
        assert_eq!(*source.budgets.lock(), [QUICK_RPC_BUDGET]);
        assert_eq!(reader.read_quick().await.lan, Some(LanReading::Count(2)));
        assert_eq!(source.calls(), 2);
        assert!(!reader.unavailable().await);
    }

    /// A quick read that finds a patient read asking rnsd answers at once with the last
    /// count read, making no call of its own.
    #[tokio::test]
    async fn a_quick_read_during_another_read_answers_with_the_last_count() {
        let dir = keyed_dir("lan-count-quick-busy");
        let source = Scripted::new([Ok(vec![auto(3)]), Ok(vec![auto(1)]), Ok(vec![auto(0)])]);
        let reader = Arc::new(reader_at(&dir.path, source.clone()));
        assert_eq!(reader.read_quick().await.lan, Some(LanReading::Count(3)));
        let held = source.gate.lock().await;
        let patient = tokio::spawn({
            let reader = reader.clone();
            async move { reader.read().await }
        });
        while source.calls() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(reader.read_quick().await.lan, Some(LanReading::Count(3)));
        assert_eq!(source.calls(), 2);
        drop(held);
        assert_eq!(patient.await.unwrap().lan, Some(LanReading::Count(1)));
        assert_eq!(reader.read_quick().await.lan, Some(LanReading::Count(0)));
        assert_eq!(source.calls(), 3);
    }

    #[tokio::test]
    async fn three_failed_calls_leave_one_read_unknown_and_the_next_read_asks_again() {
        let dir = keyed_dir("lan-count-three-failures");
        let source = Scripted::new([
            Err(RpcError::Timeout),
            Err(RpcError::Protocol("Connection RESET by peer".into())),
            Err(refused()),
            Ok(vec![auto(0)]),
        ]);
        let reader = reader_at(&dir.path, source.clone());
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        assert_eq!(source.calls(), 3);
        assert!(!reader.unavailable().await);
        assert_eq!(reader.read().await.lan, Some(LanReading::Count(0)));
        assert_eq!(source.calls(), 4);
    }

    #[tokio::test]
    async fn a_failure_that_is_not_a_drop_is_not_retried_but_asked_again_next_read() {
        let dir = keyed_dir("lan-count-no-retry");
        let source = Scripted::new([
            Err(RpcError::Protocol("frame too long".into())),
            Err(RpcError::Decode("not a map".into())),
            Ok(vec![auto(1)]),
        ]);
        let reader = reader_at(&dir.path, source.clone());
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        assert_eq!(source.calls(), 1);
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        assert_eq!(source.calls(), 2);
        assert_eq!(reader.read().await.lan, Some(LanReading::Count(1)));
    }

    /// A refused key stays refused, so it is told once at debug and not asked again.
    #[tokio::test]
    async fn a_refused_key_makes_the_rnsd_unavailable_for_the_run() {
        crate::testing::install_log_collector();
        for (tag, err) in [
            ("lan-count-auth", RpcError::Authentication),
            ("lan-count-key", RpcError::InvalidKeyMaterial),
            (
                "lan-count-digest",
                RpcError::UnsupportedDigest("md5".into()),
            ),
        ] {
            let dir = keyed_dir(tag);
            let source = Scripted::new([Err(err)]);
            let reader = reader_at(&dir.path, source.clone());
            assert_eq!(reader.read().await, RnsdReading::UNKNOWN, "{tag}");
            assert!(reader.unavailable().await, "{tag}");
            assert_eq!(reader.read().await, RnsdReading::UNKNOWN, "{tag}");
            assert_eq!(source.calls(), 1, "{tag}");
            assert_eq!(unavailable_lines(&dir.path), 1, "{tag}");
        }
    }

    #[tokio::test]
    async fn a_missing_config_dir_is_unavailable_without_a_call() {
        crate::testing::install_log_collector();
        let root = TempDir::new("lan-count-missing-dir");
        let dir = root.path.join("reticulum");
        let source = Scripted::new([]);
        let reader = reader_at(&dir, source.clone());
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        assert!(reader.unavailable().await);
        assert_eq!(source.calls(), 0);
        assert_eq!(unavailable_lines(&dir), 1);
    }

    #[tokio::test]
    async fn a_missing_transport_identity_is_unavailable_without_a_call() {
        let dir = rnsd_dir(
            "lan-count-no-identity",
            "[reticulum]\n  share_instance = Yes\n",
        );
        let source = Scripted::new([]);
        let reader = reader_at(&dir.path, source.clone());
        assert_eq!(reader.read().await, RnsdReading::UNKNOWN);
        assert!(reader.unavailable().await);
        assert_eq!(source.calls(), 0);
    }

    #[tokio::test]
    async fn the_transport_identity_keys_an_rnsd_whose_config_sets_no_rpc_key() {
        let dir = rnsd_dir(
            "lan-count-identity",
            "[reticulum]\n  share_instance = Yes\n",
        );
        fs::create_dir_all(dir.path.join("storage")).unwrap();
        fs::write(dir.path.join("storage/transport_identity"), [7u8; 64]).unwrap();
        let source = Scripted::new([Ok(vec![auto(4)])]);
        let reader = reader_at(&dir.path, source);
        assert_eq!(reader.read().await.lan, Some(LanReading::Count(4)));
    }

    #[test]
    fn a_configured_rpc_key_is_used_and_a_bad_one_or_identity_refuses() {
        let keyed = keyed_dir("lan-count-resolve-keyed");
        assert!(resolve_target(&keyed.path, "default", false).is_ok());

        let bad = rnsd_dir("lan-count-resolve-bad-key", "[reticulum]\nrpc_key = xyz\n");
        let reason = resolve_target(&bad.path, "default", false).unwrap_err();
        assert!(reason.contains("rpc_key"), "{reason}");

        let short = rnsd_dir("lan-count-resolve-short-identity", "");
        fs::create_dir_all(short.path.join("storage")).unwrap();
        fs::write(short.path.join("storage/transport_identity"), [7u8; 63]).unwrap();
        let reason = resolve_target(&short.path, "default", false).unwrap_err();
        assert!(reason.contains("transport_identity"), "{reason}");

        let missing = short.path.join("absent");
        let reason = resolve_target(&missing, "default", false).unwrap_err();
        assert!(reason.contains(&missing.display().to_string()), "{reason}");
    }

    #[test]
    fn a_config_dir_without_a_config_file_still_resolves_on_the_defaults() {
        let dir = TempDir::new("lan-count-resolve-no-config");
        fs::create_dir_all(dir.path.join("storage")).unwrap();
        fs::write(dir.path.join("storage/transport_identity"), [7u8; 64]).unwrap();
        let (endpoint, _) = resolve_target(&dir.path, "default", false).unwrap();
        assert_eq!(endpoint, RpcEndpoint::default_tcp());
    }

    #[test]
    fn the_config_dir_is_found_where_rnsd_finds_it() {
        let root = TempDir::new("lan-count-config-dir");
        let etc = root.path.join("etc/reticulum");
        let home = root.path.join("home");
        let xdg = home.join(".config/reticulum");
        fs::create_dir_all(&etc).unwrap();
        fs::create_dir_all(&xdg).unwrap();
        assert_eq!(
            default_config_dir(&etc, &home),
            home.join(".reticulum"),
            "neither holds a config"
        );
        fs::write(xdg.join("config"), "").unwrap();
        assert_eq!(default_config_dir(&etc, &home), xdg);
        fs::write(etc.join("config"), "").unwrap();
        assert_eq!(default_config_dir(&etc, &home), etc);
    }

    #[test]
    fn only_the_reticulum_section_keys_are_read() {
        let config = "\
# rpc_key = 99
[logging]
  rpc_key = ffff
[reticulum]
  enable_transport = No
  rpc_key = abcd   # set by hand
  instance_control_port = 4243
  shared_instance_type = TCP
[interfaces]
  instance_control_port = 1
";
        assert_eq!(
            parse_settings(config),
            RnsdSettings {
                rpc_key: Some("abcd".into()),
                control_port: Some(4243),
                tcp_instance: true,
            }
        );
        assert_eq!(
            parse_settings("[reticulum]\n  [[nested]]\n    rpc_key = abcd\n"),
            RnsdSettings::default()
        );
        assert_eq!(
            parse_settings("[reticulum]\nshared_instance_type = unix\ninstance_control_port = x\n"),
            RnsdSettings::default()
        );
        assert_eq!(parse_settings(""), RnsdSettings::default());
    }

    #[test]
    fn the_endpoint_follows_the_platform_and_the_shared_instance_type() {
        let tcp = |port| RpcEndpoint::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
        let defaults = RnsdSettings::default();
        let tcp_instance = RnsdSettings {
            tcp_instance: true,
            control_port: Some(4243),
            ..RnsdSettings::default()
        };
        let control_port = RnsdSettings {
            control_port: Some(4243),
            ..RnsdSettings::default()
        };
        assert_eq!(
            rpc_endpoint(true, &defaults, "alpha"),
            RpcEndpoint::for_instance("alpha")
        );
        assert_eq!(
            rpc_endpoint(true, &control_port, "alpha"),
            RpcEndpoint::for_instance("alpha")
        );
        assert_eq!(rpc_endpoint(true, &tcp_instance, "alpha"), tcp(4243));
        assert_eq!(rpc_endpoint(false, &defaults, "alpha"), tcp(37429));
        assert_eq!(rpc_endpoint(false, &control_port, "alpha"), tcp(4243));
    }

    #[test]
    fn the_known_total_sums_the_counts_and_is_none_without_any() {
        use LanReading::{Count, Unknown};
        assert_eq!(known_host_total(&[]), None);
        assert_eq!(known_host_total(&[Unknown, Unknown]), None);
        assert_eq!(known_host_total(&[Count(0)]), Some(0));
        assert_eq!(known_host_total(&[Count(2), Unknown, Count(1)]), Some(3));
    }
}
