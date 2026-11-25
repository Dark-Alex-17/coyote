//! Routes that may be stale. A path is *suspect* when the interface it was learned over is
//! down, has come back since the path was learned, or cannot be told. A link that fails
//! over a suspect path drops that path, in the local rnsd's table as well as this node's
//! own, asks the network for it once and tries the link once more before the failure is
//! reported. Reticulum keeps one path per destination and replaces it only for an
//! announce with no more hops, so a dead one-hop route over an interface that went
//! offline is never displaced by a live longer one on its own: a host that moved between
//! networks would otherwise wait out every link to a peer it knew.
//!
//! The facts come from rnsd over its RPC listener when the node attaches to one, and from
//! this node's own transport otherwise. rnsd's stats carry no per-interface uptime, so
//! "came back since" is read off the transitions the upstream watch records; the
//! in-process interfaces report theirs as events.

use crate::mesh::announce::PATH_REQUEST_TIMEOUT;
use crate::mesh::lan_count::RnsdLanReader;
use crate::mesh::propagation::PropagationError;
use crate::mesh::propagation_fetch::FetchError;
use crate::mesh::request::{RequestError, redact_hashes, short};
use crate::mesh::upstream::InterfaceTransitions;

use log::debug;
use parking_lot::Mutex;
use rns_transport::hash::AddressHash;
use rns_transport::transport::shared_instance_rpc::{self, PathRow};
use rns_transport::transport::{InterfaceEventKind, Transport};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Bound on a drop of an rnsd path settling: rnsd culls a dropped row on its next jobs
/// tick, a quarter second, so this leaves room for a loaded host.
const SETTLE_BUDGET: Duration = Duration::from_secs(2);
/// Bound on one path-table or first-hop read from rnsd.
const PATH_READ_BUDGET: Duration = Duration::from_secs(2);
/// Reticulum's `DEFAULT_PER_HOP_TIMEOUT`, which a link's establishment timeout adds per
/// hop, and the first hop's figure when rnsd cannot be asked for it.
const PER_HOP: Duration = Duration::from_secs(6);
/// The link maintenance job runs once a second, so a link's own timeout can land up to
/// that much after its figure.
const MAINTENANCE_SLACK: Duration = Duration::from_secs(1);
/// The most hops a retry is sized for; a count past it is read as unknown.
const MAX_HOPS_COUNTED: u8 = 16;
/// The least time between two weighings of one destination's path against an announce.
const PREFERENCE_GAP: Duration = Duration::from_secs(60);

tokio::task_local! {
    /// Where a typed command's progress lines go while it runs; unset for background
    /// work, which prints nothing.
    static PROGRESS: Arc<dyn Fn(&str) + Send + Sync>;
}

/// Runs `work` with `say` as the place its progress lines go.
pub(crate) async fn with_progress<F: Future>(
    say: Arc<dyn Fn(&str) + Send + Sync>,
    work: F,
) -> F::Output {
    PROGRESS.scope(say, work).await
}

fn say_progress(text: &str) {
    let _ = PROGRESS.try_with(|say| say(text));
}

/// The progress sentence a typed command gains while a stale route is replaced.
pub(crate) fn stale_route_sentence(who: &str) -> String {
    format!("The route to {who} looked stale; asking the network for a new one...")
}

/// What is known of the path held for one destination.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RouteFacts {
    /// No path is held.
    NoPath,
    /// A path is held, but not which interface it rests on or that interface's state.
    Unknown,
    /// A path over `interface`, `online` as last read (`None` when the interface was not
    /// found), learned at `learned_at` over `hops`; `changed_since` when the interface came
    /// back, or the host's network changed, after the path was learned.
    Known {
        interface: String,
        online: Option<bool>,
        changed_since: bool,
        learned_at: SystemTime,
        hops: u8,
    },
}

/// Why a path is suspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuspectWhy {
    InterfaceDown,
    ChangedSince,
    CannotTell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Suspicion {
    Suspect(SuspectWhy),
    Sound,
    NoPath,
}

impl RouteFacts {
    pub(crate) fn suspicion(&self) -> Suspicion {
        match self {
            Self::NoPath => Suspicion::NoPath,
            Self::Unknown
            | Self::Known {
                online: None,
                changed_since: false,
                ..
            } => Suspicion::Suspect(SuspectWhy::CannotTell),
            Self::Known {
                online: Some(false),
                ..
            } => Suspicion::Suspect(SuspectWhy::InterfaceDown),
            Self::Known {
                changed_since: true,
                ..
            } => Suspicion::Suspect(SuspectWhy::ChangedSince),
            Self::Known { .. } => Suspicion::Sound,
        }
    }
}

/// What a failed link does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Drop the path, ask the network once, try the link once more.
    DropAndAsk,
    /// Try the link once more over the same path: a first contact that may only be far.
    RetryOnce,
    /// Report the failure as it is.
    Report,
}

/// What a link that failed with `err` does next, given the path's `suspicion` and whether
/// this was a first contact, a path learned or asked for just before the link. Only a link
/// that never came up, or came up and could not carry the request, is retried: an answer
/// of any kind says the route works.
pub(crate) fn decide(err: &RequestError, suspicion: Suspicion, first_contact: bool) -> Recovery {
    let link_level = matches!(
        err,
        RequestError::LinkActivationTimeout { .. }
            | RequestError::LinkFailed(_)
            | RequestError::LinkNotOpened(_)
    );
    if !link_level {
        return Recovery::Report;
    }
    match suspicion {
        Suspicion::Suspect(_) => Recovery::DropAndAsk,
        Suspicion::Sound
            if first_contact && matches!(err, RequestError::LinkActivationTimeout { .. }) =>
        {
            Recovery::RetryOnce
        }
        Suspicion::Sound | Suspicion::NoPath => Recovery::Report,
    }
}

/// The link timeout of the one retry: rnsd's first-hop figure plus `PER_HOP` a hop and the
/// maintenance slack, as a link's own establishment timeout runs, never less than
/// `link_timeout` and never more than twice it, so a retry adds at most one more link
/// timeout to what the failure took. A hop count past `MAX_HOPS_COUNTED`, as the
/// transport reads one it holds no path for, counts as one hop.
pub(crate) fn retry_link_timeout(
    first_hop: Option<Duration>,
    hops: u8,
    link_timeout: Duration,
) -> Duration {
    let hops = if hops > MAX_HOPS_COUNTED {
        1
    } else {
        hops.max(1)
    };
    let sized = first_hop.unwrap_or(PER_HOP) + PER_HOP * u32::from(hops) + MAINTENANCE_SLACK;
    sized.clamp(link_timeout, link_timeout * 2)
}

/// What a network transition made suspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Affected {
    /// The paths over one interface: it went down or came back.
    Interface(String),
    /// Every path: the host's network changed.
    Everything,
}

/// The destinations of `rows` (destination hex, interface) whose path `affected` covers.
pub(crate) fn paths_over(rows: &[(String, String)], affected: &Affected) -> Vec<String> {
    rows.iter()
        .filter(|(_, interface)| match affected {
            Affected::Interface(name) => interface == name,
            Affected::Everything => true,
        })
        .map(|(destination, _)| destination.clone())
        .collect()
}

/// The destinations among `wanted` (trusted sessions and filed propagation nodes) whose
/// path in `rows` (destination hex, interface, hops) runs the long way round, over another
/// interface than `returned` and more than one hop, so a path request may find the shorter
/// one the returned interface reaches. Each destination at most once.
pub(crate) fn return_home_candidates(
    rows: &[(String, String, u8)],
    returned: &str,
    wanted: &HashSet<String>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    rows.iter()
        .filter(|(destination, interface, hops)| {
            interface != returned && *hops > 1 && wanted.contains(destination)
        })
        .filter(|(destination, _, _)| seen.insert(destination.clone()))
        .map(|(destination, _, _)| destination.clone())
        .collect()
}

/// The `candidates` not in `asked_before`, each added to it: return home asks a
/// destination once per transition however many interfaces come back in it.
fn claim_unasked(
    candidates: Vec<String>,
    asked_before: &parking_lot::Mutex<HashSet<String>>,
) -> Vec<String> {
    let mut asked = asked_before.lock();
    candidates
        .into_iter()
        .filter(|destination| asked.insert(destination.clone()))
        .collect()
}

/// This node's memory of its own interfaces' state and of the paths a transition made
/// suspect.
#[derive(Default)]
pub(crate) struct RouteBook {
    /// The in-process interfaces seen disconnected and not connected since.
    down: Mutex<HashSet<AddressHash>>,
    /// When each in-process interface last came back after a disconnect.
    back_at: Mutex<HashMap<AddressHash, SystemTime>>,
    /// Destinations whose path a transition made suspect; the next request to each asks
    /// the network first.
    marked: Mutex<HashSet<String>>,
    /// When each destination's held path was last weighed against an announce that
    /// arrived over another interface, so a stream of announces weighs it at most once
    /// per `PREFERENCE_GAP`.
    weighed: Mutex<HashMap<String, std::time::Instant>>,
    /// Set when interface events were missed: an in-process interface with no event since
    /// cannot be told down or up, so a path over it reads `Unknown`.
    lagged: Mutex<Option<HashSet<AddressHash>>>,
}

impl RouteBook {
    /// Folds one in-process interface event in. `true` when it says the interface came
    /// back after a disconnect.
    pub(crate) fn note_interface_event(
        &self,
        iface: AddressHash,
        kind: InterfaceEventKind,
        at: SystemTime,
    ) -> bool {
        if let Some(seen) = self.lagged.lock().as_mut() {
            seen.insert(iface);
        }
        match kind {
            InterfaceEventKind::Disconnected { .. } => {
                self.down.lock().insert(iface);
                false
            }
            InterfaceEventKind::Connected => {
                let came_back = self.down.lock().remove(&iface);
                if came_back {
                    self.back_at.lock().insert(iface, at);
                }
                came_back
            }
            _ => false,
        }
    }

    /// Interface events were missed: until each in-process interface reports again, its
    /// state cannot be told.
    pub(crate) fn note_lag(&self) {
        *self.lagged.lock() = Some(HashSet::new());
    }

    /// Whether events for `iface` were missed and none has come since.
    fn in_process_unknown(&self, iface: &AddressHash) -> bool {
        self.lagged
            .lock()
            .as_ref()
            .is_some_and(|seen| !seen.contains(iface))
    }

    /// Whether any in-process interface is down now: only then can an announce over an
    /// up one displace a path.
    pub(crate) fn any_down(&self) -> bool {
        !self.down.lock().is_empty()
    }

    fn in_process_online(&self, iface: &AddressHash) -> bool {
        !self.down.lock().contains(iface)
    }

    fn in_process_back_since(&self, iface: &AddressHash, since: SystemTime) -> bool {
        self.back_at.lock().get(iface).is_some_and(|at| *at > since)
    }

    /// Marks `destinations` suspect until each is next requested.
    pub(crate) fn mark(&self, destinations: impl IntoIterator<Item = String>) {
        self.marked.lock().extend(destinations);
    }

    /// Takes the mark off `destination`: `true` when it had one.
    pub(crate) fn take_mark(&self, destination: &str) -> bool {
        self.marked.lock().remove(destination)
    }

    #[cfg(test)]
    pub(crate) fn is_marked(&self, destination: &str) -> bool {
        self.marked.lock().contains(destination)
    }

    /// Claims the next weighing of `destination`'s path at `now`: `false` while one ran
    /// within `PREFERENCE_GAP`.
    fn claim_weighing(&self, destination: &str, now: std::time::Instant) -> bool {
        let mut weighed = self.weighed.lock();
        if weighed
            .get(destination)
            .is_some_and(|at| now.saturating_duration_since(*at) < PREFERENCE_GAP)
        {
            return false;
        }
        weighed.retain(|_, at| now.saturating_duration_since(*at) < PREFERENCE_GAP);
        weighed.insert(destination.to_string(), now);
        true
    }
}

/// What this node's routes are read from and dropped in: its transport, the local rnsd
/// when it attaches to one, the transitions the upstream watch saw and the book of its own
/// interfaces.
#[derive(Clone, Copy)]
pub(crate) struct Routes<'a> {
    pub transport: &'a Transport,
    pub rnsd: Option<&'a RnsdLanReader>,
    pub transitions: &'a InterfaceTransitions,
    pub book: &'a RouteBook,
}

/// rnsd's row for one destination, if it could be read.
enum RnsdRow {
    Row(PathRow),
    None,
    Unreadable,
}

fn learned_at(timestamp: f64) -> SystemTime {
    if timestamp.is_finite() && timestamp > 0.0 {
        SystemTime::UNIX_EPOCH + Duration::from_secs_f64(timestamp)
    } else {
        SystemTime::UNIX_EPOCH
    }
}

impl Routes<'_> {
    async fn rnsd_rows(&self) -> Option<Vec<PathRow>> {
        let (endpoint, key) = self.rnsd?.rpc_target().await?;
        match shared_instance_rpc::path_table(&endpoint, &key, None, PATH_READ_BUDGET).await {
            Ok(rows) => Some(rows),
            Err(err) => {
                debug!(
                    "Mesh could not read rnsd's path table: {}",
                    redact_hashes(&err.to_string())
                );
                None
            }
        }
    }

    async fn rnsd_row(&self, destination: &AddressHash) -> RnsdRow {
        match self.rnsd_rows().await {
            Some(rows) => rows
                .into_iter()
                .find(|row| row.hash == *destination)
                .map_or(RnsdRow::None, RnsdRow::Row),
            None => RnsdRow::Unreadable,
        }
    }

    /// What is known of the path to `destination`: rnsd's row and its interface's state
    /// when the node attaches to an rnsd that holds one, this node's own path otherwise.
    pub(crate) async fn facts(&self, destination: &AddressHash) -> RouteFacts {
        if let Some(reader) = self.rnsd {
            match self.rnsd_row(destination).await {
                RnsdRow::Row(row) => {
                    let online = reader.interface_states().await.and_then(|states| {
                        states
                            .into_iter()
                            .find(|state| state.name == row.interface)
                            .map(|state| state.online)
                    });
                    let learned_at = learned_at(row.timestamp);
                    // A row whose timestamp is zero was dropped and not culled yet.
                    let changed_since = row.timestamp <= 0.0
                        || self.transitions.changed_since(&row.interface, learned_at);
                    return RouteFacts::Known {
                        interface: row.interface,
                        online,
                        changed_since,
                        learned_at,
                        hops: row.hops,
                    };
                }
                RnsdRow::Unreadable if self.transport.has_path(destination).await => {
                    return RouteFacts::Unknown;
                }
                RnsdRow::Unreadable | RnsdRow::None => {}
            }
        }
        let Some(path) = self.transport.path_interface(destination).await else {
            return RouteFacts::NoPath;
        };
        if self.book.in_process_unknown(&path.iface) {
            return RouteFacts::Unknown;
        }
        let online = self.book.in_process_online(&path.iface);
        // Every in-process interface reports its disconnects as events, which the book
        // folds in, so one the manager keeps no name for is told by its hash.
        let interface = path.name.unwrap_or_else(|| path.iface.to_hex_string());
        RouteFacts::Known {
            interface,
            online: Some(online),
            changed_since: self
                .book
                .in_process_back_since(&path.iface, path.learned_at),
            learned_at: path.learned_at,
            hops: path.hops,
        }
    }

    /// Drops the path to `destination`, rnsd's settled first so the request after it is
    /// not answered from the dropped row, then this node's own, and asks the network once,
    /// waiting up to `wait` for an answer. Whether a path came back.
    pub(crate) async fn drop_and_ask(&self, destination: &AddressHash, wait: Duration) -> bool {
        let dest8 = short(&destination.to_hex_string()).to_string();
        self.drop_rnsd(destination).await;
        self.transport.expire_path(destination).await;
        let found = self.transport.await_path(destination, wait, None).await;
        debug!(
            "Mesh asked the network for a new path to {dest8} and {} within {:.1} s",
            if found { "heard one" } else { "heard none" },
            wait.as_secs_f64()
        );
        found
    }

    /// Drops rnsd's path to `destination`, settled, when the node attaches to an rnsd;
    /// this node's own copy is left alone.
    async fn drop_rnsd(&self, destination: &AddressHash) {
        let dest8 = short(&destination.to_hex_string()).to_string();
        if let Some(reader) = self.rnsd
            && let Some((endpoint, key)) = reader.rpc_target().await
        {
            match shared_instance_rpc::drop_path_settled(
                &endpoint,
                &key,
                destination,
                SETTLE_BUDGET,
            )
            .await
            {
                Ok(true) => debug!("Mesh dropped rnsd's path to {dest8}"),
                Ok(false) => debug!("rnsd held no path to {dest8} to drop"),
                Err(err) => debug!(
                    "Mesh could not settle the drop of rnsd's path to {dest8}: {}",
                    redact_hashes(&err.to_string())
                ),
            }
        }
    }

    /// Drops rnsd's row to `destination` and asks the network once, keeping this node's
    /// own copy, so links opened meanwhile still have a path: rnsd would answer a request
    /// from its row, so only its row goes.
    async fn replace_rnsd_row(&self, destination: &AddressHash) {
        self.drop_rnsd(destination).await;
        self.transport.request_path(destination, None, None).await;
    }

    /// An announce for `destination` arrived over `arrival`, an interface that is up. A
    /// path held over an interface that is down is replaced whatever its hops: dropped
    /// (rnsd's too, settled) and asked for again, the request in process going out over
    /// `arrival`. Reticulum itself keeps the path with fewer hops. Through rnsd only rnsd's
    /// row is dropped, this node's copy from the announce kept. Through rnsd every
    /// announce arrives over the shared instance, so rnsd's row is read, and only while
    /// the latest stats read saw one of its upstreams offline. Whether the path was
    /// replaced.
    pub(crate) async fn prefer_up(&self, destination: &AddressHash, arrival: AddressHash) -> bool {
        let destination_hex = destination.to_hex_string();
        let dest8 = short(&destination_hex).to_string();
        let claim = || {
            self.book
                .claim_weighing(&destination_hex, std::time::Instant::now())
        };
        if let Some(reader) = self.rnsd {
            let upstream_down = reader
                .tcp_interfaces_seen()
                .is_some_and(|interfaces| interfaces.iter().any(|interface| !interface.online));
            if !upstream_down || !claim() {
                return false;
            }
            let RnsdRow::Row(row) = self.rnsd_row(destination).await else {
                return false;
            };
            let down = reader.interface_states().await.is_some_and(|states| {
                states
                    .iter()
                    .any(|state| state.name == row.interface && !state.online)
            });
            if !down {
                return false;
            }
            debug!(
                "Mesh replaces rnsd's path to {dest8} over {}, which is down, with one over an interface that is up",
                row.interface
            );
            self.replace_rnsd_row(destination).await;
            return true;
        }
        let Some(path) = self.transport.path_interface(destination).await else {
            return false;
        };
        if path.iface == arrival || self.book.in_process_online(&path.iface) || !claim() {
            return false;
        }
        debug!(
            "Mesh replaces the path to {dest8} over {}, which is down, ({} hops) with one over an interface that is up",
            path.name
                .as_deref()
                .unwrap_or("an interface no longer registered"),
            path.hops
        );
        self.transport.expire_path(destination).await;
        self.transport
            .request_path(destination, Some(arrival), None)
            .await;
        true
    }

    /// The link timeout for the one retry to `destination`.
    pub(crate) async fn retry_timeout(
        &self,
        destination: &AddressHash,
        link_timeout: Duration,
    ) -> Duration {
        let hops = self.transport.hops_to(destination).await;
        let first_hop = match self.rnsd {
            Some(reader) => match reader.rpc_target().await {
                Some((endpoint, key)) => shared_instance_rpc::first_hop_timeout(
                    &endpoint,
                    &key,
                    destination,
                    PATH_READ_BUDGET,
                )
                .await
                .ok()
                .filter(|secs| secs.is_finite() && *secs > 0.0)
                .map(Duration::from_secs_f64),
                None => None,
            },
            None => None,
        };
        retry_link_timeout(first_hop, hops, link_timeout)
    }

    /// The destinations whose path rests on what `affected` names: rnsd's rows by
    /// interface name when the node attaches to an rnsd, this node's own table by
    /// interface hash otherwise.
    pub(crate) async fn destinations_over(&self, affected: &Affected) -> Vec<String> {
        let rows: Vec<(String, String)> = match self.rnsd_rows().await {
            Some(rows) => rows
                .into_iter()
                .map(|row| (row.hash.to_hex_string(), row.interface))
                .collect(),
            None => self
                .own_rows()
                .await
                .into_iter()
                .map(|(destination, interface, _)| (destination, interface))
                .collect(),
        };
        paths_over(&rows, affected)
    }

    /// The destinations whose path in this node's own table rests on `iface`.
    pub(crate) async fn own_destinations_over(&self, iface: AddressHash) -> Vec<String> {
        let rows: Vec<(String, String)> = self
            .own_rows()
            .await
            .into_iter()
            .map(|(destination, interface, _)| (destination, interface))
            .collect();
        paths_over(&rows, &Affected::Interface(iface.to_hex_string()))
    }

    /// This node's own paths: destination hex, interface hash hex, hops.
    async fn own_rows(&self) -> Vec<(String, String, u8)> {
        self.transport
            .path_table(None)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|row| {
                        (
                            row.destination.to_hex_string(),
                            row.interface.to_hex_string(),
                            row.hops,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// After the rnsd interface `returned` came back: each of `wanted` routed the long way
    /// round over another interface has rnsd's row dropped and asked for once, so the
    /// shorter route over `returned` can answer; rnsd would answer the request from the
    /// long row otherwise. This node's own copy stays, so a link opened meanwhile still
    /// has a path. How many were asked for.
    pub(crate) async fn return_home_rnsd(
        &self,
        returned: &str,
        wanted: &HashSet<String>,
        asked_before: &parking_lot::Mutex<HashSet<String>>,
    ) -> usize {
        let Some(rows) = self.rnsd_rows().await else {
            return 0;
        };
        let rows: Vec<(String, String, u8)> = rows
            .into_iter()
            .map(|row| (row.hash.to_hex_string(), row.interface, row.hops))
            .collect();
        let candidates = claim_unasked(
            return_home_candidates(&rows, returned, wanted),
            asked_before,
        );
        let mut asked = 0;
        for destination in candidates {
            let Ok(hash) = AddressHash::new_from_hex_string(&destination) else {
                continue;
            };
            debug!(
                "Mesh is asking again for the path to {}, which runs the long way round now that {returned} is back",
                short(&destination)
            );
            self.replace_rnsd_row(&hash).await;
            asked += 1;
        }
        asked
    }

    /// `return_home_rnsd` for an in-process interface that came back: a path request on
    /// that interface alone, which the transport takes over the long route only for fewer
    /// hops, so the working path is never dropped. How many were asked for.
    pub(crate) async fn return_home_in_process(
        &self,
        iface: AddressHash,
        wanted: &HashSet<String>,
        asked_before: &parking_lot::Mutex<HashSet<String>>,
    ) -> usize {
        let rows = self.own_rows().await;
        let mut asked = 0;
        let candidates = claim_unasked(
            return_home_candidates(&rows, &iface.to_hex_string(), wanted),
            asked_before,
        );
        for destination in candidates {
            let Ok(hash) = AddressHash::new_from_hex_string(&destination) else {
                continue;
            };
            debug!(
                "Mesh is asking for a shorter path to {} over an interface that came back",
                short(&destination)
            );
            self.transport.request_path(&hash, Some(iface), None).await;
            asked += 1;
        }
        asked
    }
}

/// One try of a link: its timeout, whether the link should leave the path request to
/// its caller (`path_requested`), and whether a path request for the destination was
/// actually sent just before it, by this call or the resolve that led to it (`asked`):
/// only such a try is a first contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkTry {
    pub link_timeout: Duration,
    pub path_requested: bool,
    pub asked: bool,
}

/// How a caller's error is read for recovery: the link error it carries, if any.
pub(crate) type LinkErrorOf<E> = fn(&mut E) -> Option<&mut RequestError>;

/// `LinkErrorOf` for a request's own error.
pub(crate) fn the_error(err: &mut RequestError) -> Option<&mut RequestError> {
    Some(err)
}

/// `LinkErrorOf` for a post to a propagation node.
pub(crate) fn post_link_error(err: &mut PropagationError) -> Option<&mut RequestError> {
    match err {
        PropagationError::Link(err) => Some(err),
        _ => None,
    }
}

/// `LinkErrorOf` for a fetch from a propagation node.
pub(crate) fn fetch_link_error(err: &mut FetchError) -> Option<&mut RequestError> {
    match err {
        FetchError::Link(err) => Some(err),
        _ => None,
    }
}

impl Routes<'_> {
    /// Runs `attempt`, and once more when it fails in a way a stale or far route explains:
    /// over a suspect path the path is dropped and asked for first, and the progress line
    /// says so once; a first contact over a sound path is tried again as it is. A path a
    /// network change marked suspect is replaced before the first try. The retry's link
    /// timeout is sized from the route; a retry that still sees no link come up says a far
    /// peer can take longer, naming `who`, and never that the peer left.
    pub(crate) async fn recovering<T, E, Fut>(
        &self,
        destination: &AddressHash,
        who: impl Fn() -> String,
        first: LinkTry,
        mut attempt: impl FnMut(LinkTry) -> Fut,
        link_error: LinkErrorOf<E>,
    ) -> Result<T, E>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        let destination_hex = destination.to_hex_string();
        let dest8 = short(&destination_hex).to_string();
        let mut first = first;
        let mut said = false;
        let mut asked_for_mark = false;
        // Taken whether or not this node holds a path itself: rnsd may still hold the
        // stale row, and would answer a path request from it. A path learned again since
        // the mark, as from the session's own greeting, is sound and is left alone.
        if self.book.take_mark(&destination_hex)
            && matches!(
                self.facts(destination).await.suspicion(),
                Suspicion::Suspect(_)
            )
        {
            debug!(
                "Mesh path to {dest8} rests on an interface that changed; asking the network for a new one first"
            );
            say_progress(&stale_route_sentence(&who()));
            said = true;
            self.drop_and_ask(destination, PATH_REQUEST_TIMEOUT).await;
            first.path_requested = true;
            first.asked = true;
            asked_for_mark = true;
        }
        let mut outcome = attempt(first).await;
        let Err(err) = &mut outcome else {
            return outcome;
        };
        let Some(failed) = link_error(err) else {
            return outcome;
        };
        let facts = self.facts(destination).await;
        // A first contact is a try made right after a path request that was actually
        // sent; a path's age is no evidence of one, as every announce refreshes it.
        let first_contact = first.asked;
        let first_never_linked = matches!(failed, RequestError::LinkActivationTimeout { .. });
        let suspicion = facts.suspicion();
        let mut recovery = decide(failed, suspicion, first_contact);
        // One path request per call: a marked path already had its drop and ask.
        if recovery == Recovery::DropAndAsk && asked_for_mark {
            recovery = Recovery::RetryOnce;
        }
        match recovery {
            Recovery::Report => return outcome,
            Recovery::DropAndAsk => {
                debug!(
                    "Mesh link to {dest8} failed over a suspect path ({suspicion:?}): {}; dropping it and asking the network once",
                    redact_hashes(&failed.to_string())
                );
                if !said {
                    say_progress(&stale_route_sentence(&who()));
                }
                if !self.drop_and_ask(destination, PATH_REQUEST_TIMEOUT).await {
                    debug!("Mesh found no new path to {dest8}");
                }
            }
            Recovery::RetryOnce => debug!(
                "Mesh link to {dest8} did not come up right after a path request; trying it once more"
            ),
        }
        let link_timeout = self.retry_timeout(destination, first.link_timeout).await;
        let mut retried = attempt(LinkTry {
            link_timeout,
            path_requested: true,
            asked: true,
        })
        .await;
        // A retry that found no path at all says less than the first failure did.
        if let Err(err) = &mut retried
            && matches!(link_error(err), Some(RequestError::LinkNotOpened(_)))
            && !self.transport.has_path(destination).await
        {
            return outcome;
        }
        if let Err(err) = &mut retried
            && let Some(RequestError::LinkActivationTimeout {
                retried_to,
                recently_seen,
                ..
            }) = link_error(err)
        {
            *retried_to = Some(who());
            *recently_seen = false;
        }
        // The first link never came up and the retry's did, so the request went out on
        // it: a silence now may be the answer still on its way, not a session that never
        // heard it.
        if first_never_linked
            && let Err(err) = &mut retried
            && let Some(failed) = link_error(err)
            && let RequestError::Timeout { path, after } = failed
        {
            debug!(
                "Mesh request {path} to {dest8} went out on a link that came up late and was not answered within {:.1} s",
                after.as_secs_f64()
            );
            *failed = RequestError::LateLinkUnanswered {
                path: std::mem::take(path),
                after: *after,
            };
        }
        retried
    }
}

/// The rnsd half of the route checks against a stand-in for rnsd's RPC listener: the
/// CPython stdlib handshake, then one request and one reply per connection.
#[cfg(all(test, unix))]
mod rnsd_tests {
    use super::*;
    use crate::mesh::lan_count::RnsdLocator;
    use crate::mesh::test_support::TempDir;
    use crate::mesh::upstream::TransitionKind;
    use hmac::{Hmac, Mac};
    use rmpv::Value;
    use rns_transport::identity::PrivateIdentity;
    use rns_transport::rand_core::OsRng;
    use rns_transport::transport::TransportConfig;
    use sha2::Sha256;
    use std::collections::VecDeque;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const KEY: [u8; 32] = [0x5a; 32];
    const CHALLENGE: &[u8] = b"#CHALLENGE#";
    const WELCOME: &[u8] = b"#WELCOME#";
    const DIGEST: &[u8] = b"{sha256}";
    const TEAM_RELAY: &str = "TCPInterface[Team Relay/192.168.0.84:4244]";
    const PUBLIC_HUB: &str = "TCPInterface[Public Hub/reticulum.nepamesh.com:4242]";

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut out = i32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(payload);
        out
    }

    async fn recv_frame<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<Vec<u8>> {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).await?;
        let mut payload = vec![0u8; usize::try_from(i32::from_be_bytes(header)).unwrap()];
        stream.read_exact(&mut payload).await?;
        Ok(payload)
    }

    fn answer(message: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&KEY).unwrap();
        mac.update(message);
        let mut out = DIGEST.to_vec();
        out.extend_from_slice(&mac.finalize().into_bytes());
        out
    }

    /// The request's verb and target: `("get", "path_table")`, `("drop", "path")`.
    fn verb(request: &Value) -> (String, String) {
        let map = request.as_map().unwrap();
        for (key, value) in map {
            let key = key.as_str().unwrap();
            if key == "get" || key == "drop" {
                return (key.to_string(), value.as_str().unwrap().to_string());
            }
        }
        panic!("no verb in {request:?}");
    }

    /// A stand-in rnsd answering each request with `reply`, keeping the verbs in order.
    struct FakeRnsd {
        port: u16,
        asked: Arc<Mutex<Vec<(String, String)>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeRnsd {
        async fn start(reply: impl Fn(&(String, String)) -> Value + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let asked = Arc::new(Mutex::new(Vec::new()));
            let log = asked.clone();
            let reply = Arc::new(reply);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (log, reply) = (log.clone(), reply.clone());
                    tokio::spawn(async move {
                        let mut message = DIGEST.to_vec();
                        message.extend_from_slice(&[0x42; 40]);
                        let mut challenge = CHALLENGE.to_vec();
                        challenge.extend_from_slice(&message);
                        stream.write_all(&frame(&challenge)).await.unwrap();
                        assert_eq!(recv_frame(&mut stream).await.unwrap(), answer(&message));
                        stream.write_all(&frame(WELCOME)).await.unwrap();
                        let theirs = recv_frame(&mut stream).await.unwrap();
                        let ours = answer(&theirs[CHALLENGE.len()..]);
                        stream.write_all(&frame(&ours)).await.unwrap();
                        assert_eq!(recv_frame(&mut stream).await.unwrap(), WELCOME);
                        let request = recv_frame(&mut stream).await.unwrap();
                        let request = rmpv::decode::read_value(&mut request.as_slice()).unwrap();
                        let verb = verb(&request);
                        log.lock().push(verb.clone());
                        let mut packed = Vec::new();
                        rmpv::encode::write_value(&mut packed, &reply(&verb)).unwrap();
                        stream.write_all(&frame(&packed)).await.unwrap();
                        let _ = stream.shutdown().await;
                    });
                }
            });
            Self { port, asked, task }
        }

        fn asked(&self) -> Vec<(String, String)> {
            self.asked.lock().clone()
        }

        fn reader(&self, tmp: &TempDir) -> RnsdLanReader {
            let dir = tmp.path.join("rnsd");
            std::fs::create_dir_all(&dir).unwrap();
            let key: String = KEY.iter().map(|byte| format!("{byte:02x}")).collect();
            std::fs::write(
                dir.join("config"),
                format!(
                    "[reticulum]\n  rpc_key = {key}\n  shared_instance_type = tcp\n  instance_control_port = {}\n",
                    self.port
                ),
            )
            .unwrap();
            RnsdLanReader::new(
                RnsdLocator {
                    dir_override: Some(dir),
                },
                "default",
            )
        }
    }

    impl Drop for FakeRnsd {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn map(pairs: Vec<(&str, Value)>) -> Value {
        Value::Map(
            pairs
                .into_iter()
                .map(|(key, value)| (Value::from(key), value))
                .collect(),
        )
    }

    fn stat(name: &str, online: bool) -> Value {
        map(vec![
            ("name", Value::from(name)),
            ("short_name", Value::from("relay")),
            ("type", Value::from("TCPClientInterface")),
            ("status", Value::from(online)),
            ("rxb", Value::from(0)),
            ("txb", Value::from(0)),
        ])
    }

    /// rnsd answering for one row to `destination` over `TEAM_RELAY`, learned `age` ago,
    /// with Team Relay `online` and the Public Hub up; a drop answers true and the next
    /// hop then reads "None", as after rnsd's cull.
    async fn rnsd_with_row(destination: AddressHash, age: Duration, online: bool) -> FakeRnsd {
        let learned = (SystemTime::now() - age)
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        FakeRnsd::start(move |(verb, what)| match (verb.as_str(), what.as_str()) {
            ("get", "path_table") => Value::Array(vec![map(vec![
                ("hash", Value::Binary(destination.as_slice().to_vec())),
                ("timestamp", Value::F64(learned)),
                ("via", Value::Binary(vec![0x11; 16])),
                ("hops", Value::from(1)),
                ("expires", Value::F64(learned + 604_800.0)),
                ("interface", Value::from(TEAM_RELAY)),
            ])]),
            ("get", "interface_stats") => map(vec![(
                "interfaces",
                Value::Array(vec![stat(TEAM_RELAY, online), stat(PUBLIC_HUB, true)]),
            )]),
            ("drop", "path") => Value::from(true),
            ("get", "next_hop_if_name") => Value::from("None"),
            ("get", "first_hop_timeout") => Value::F64(6.05),
            other => panic!("unexpected request {other:?}"),
        })
        .await
    }

    fn transport() -> Transport {
        Transport::new(TransportConfig::new(
            "routes",
            &PrivateIdentity::new_from_rand(OsRng),
            false,
        ))
    }

    fn destination() -> AddressHash {
        AddressHash::new_from_hex_string(&"ab".repeat(16)).unwrap()
    }

    /// rnsd's row over an interface it reads offline is suspect, joined to the stats by
    /// the interface's full name; recovering drops it settled (the drop, then the next hop
    /// read until "None") before asking the network, and sizes the retry from rnsd's
    /// first-hop figure.
    #[tokio::test]
    async fn rnsds_row_over_an_offline_interface_is_dropped_settled_and_asked_for() {
        let tmp = TempDir::new("routes-rnsd-offline");
        let rnsd = rnsd_with_row(destination(), Duration::from_secs(60), false).await;
        let reader = rnsd.reader(&tmp);
        let transport = transport();
        let transitions = InterfaceTransitions::default();
        let book = RouteBook::default();
        let routes = Routes {
            transport: &transport,
            rnsd: Some(&reader),
            transitions: &transitions,
            book: &book,
        };

        let facts = routes.facts(&destination()).await;
        assert!(
            matches!(&facts, RouteFacts::Known { interface, online: Some(false), hops: 1, .. } if interface == TEAM_RELAY),
            "{facts:?}"
        );
        assert_eq!(
            facts.suspicion(),
            Suspicion::Suspect(SuspectWhy::InterfaceDown)
        );

        let before = rnsd.asked().len();
        assert!(
            !routes
                .drop_and_ask(&destination(), Duration::from_millis(50))
                .await
        );
        let drop: Vec<(String, String)> = rnsd.asked()[before..].to_vec();
        assert_eq!(
            drop,
            [
                ("drop".to_string(), "path".to_string()),
                ("get".to_string(), "next_hop_if_name".to_string()),
            ],
            "the drop is settled before the path request"
        );

        let hops = transport.hops_to(&destination()).await;
        assert_eq!(
            routes
                .retry_timeout(&destination(), Duration::from_secs(10))
                .await,
            retry_link_timeout(
                Some(Duration::from_secs_f64(6.05)),
                hops,
                Duration::from_secs(10)
            )
        );
    }

    /// A row over an interface that is up is sound until that interface comes back after
    /// the row was learned, or the host's network changes after it.
    #[tokio::test]
    async fn rnsds_row_is_suspect_once_its_interface_came_back_since() {
        let tmp = TempDir::new("routes-rnsd-returned");
        let rnsd = rnsd_with_row(destination(), Duration::from_secs(60), true).await;
        let reader = rnsd.reader(&tmp);
        let transport = transport();
        let transitions = InterfaceTransitions::default();
        let book = RouteBook::default();
        let routes = Routes {
            transport: &transport,
            rnsd: Some(&reader),
            transitions: &transitions,
            book: &book,
        };
        assert_eq!(
            routes.facts(&destination()).await.suspicion(),
            Suspicion::Sound
        );

        transitions.record(PUBLIC_HUB, TransitionKind::Up, SystemTime::now());
        assert_eq!(
            routes.facts(&destination()).await.suspicion(),
            Suspicion::Sound,
            "another interface came back"
        );
        transitions.record(TEAM_RELAY, TransitionKind::Up, SystemTime::now());
        assert_eq!(
            routes.facts(&destination()).await.suspicion(),
            Suspicion::Suspect(SuspectWhy::ChangedSince)
        );

        let moved = InterfaceTransitions::default();
        moved.record("en0", TransitionKind::AddressesChanged, SystemTime::now());
        let routes = Routes {
            transitions: &moved,
            ..routes
        };
        assert_eq!(
            routes.facts(&destination()).await.suspicion(),
            Suspicion::Suspect(SuspectWhy::ChangedSince)
        );
        assert_eq!(
            routes
                .destinations_over(&Affected::Interface(TEAM_RELAY.to_string()))
                .await,
            [destination().to_hex_string()]
        );
        assert!(
            routes
                .destinations_over(&Affected::Interface(PUBLIC_HUB.to_string()))
                .await
                .is_empty()
        );
    }

    /// A path rnsd holds over an interface that is down gives way to an announce that
    /// arrived over one that is up, whatever its hops: the row is dropped settled and
    /// asked for again, once per `PREFERENCE_GAP`. Over an interface that is up it stays.
    #[tokio::test]
    async fn a_path_over_a_down_interface_gives_way_to_an_announce_over_an_up_one() {
        for (online, replaced) in [(false, true), (true, false)] {
            let tmp = TempDir::new("routes-prefer-up");
            let rnsd = rnsd_with_row(destination(), Duration::from_secs(60), online).await;
            let reader = rnsd.reader(&tmp);
            // The stats read the LAN row makes, which tells the preference an upstream is
            // down at all.
            reader.read().await;
            let transport = transport();
            let transitions = InterfaceTransitions::default();
            let book = RouteBook::default();
            let routes = Routes {
                transport: &transport,
                rnsd: Some(&reader),
                transitions: &transitions,
                book: &book,
            };
            let arrival = AddressHash::new_from_hex_string(&"cd".repeat(16)).unwrap();
            assert_eq!(
                routes.prefer_up(&destination(), arrival).await,
                replaced,
                "Team Relay online: {online}"
            );
            let dropped = rnsd
                .asked()
                .contains(&("drop".to_string(), "path".to_string()));
            assert_eq!(dropped, replaced, "{:?}", rnsd.asked());
            if replaced {
                assert!(
                    !routes.prefer_up(&destination(), arrival).await,
                    "weighed at most once per gap"
                );
            }
        }
    }

    fn never_linked() -> RequestError {
        RequestError::LinkActivationTimeout {
            path: "/status".to_string(),
            after: Duration::from_secs(10),
            recently_seen: false,
            retried_to: None,
        }
    }

    /// Whether a transition marked the path before the request: `AfterChange` with Team
    /// Relay coming back since the row was learned, `Refreshed` with the row learned again
    /// since.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        No,
        AfterChange,
        Refreshed,
    }

    /// What `recovering` did: the tries it made, the progress lines it said, and the
    /// paths it dropped in rnsd.
    struct Run {
        tries: Vec<LinkTry>,
        said: Vec<String>,
        drops: usize,
        outcome: Result<(), RequestError>,
    }

    /// `recovering` over rnsd's row learned `age` ago with Team Relay `online`, the first
    /// try failing with `first` and the second with `second`, inside a typed command.
    async fn recover(
        tag: &str,
        age: Duration,
        online: bool,
        marked: Mark,
        asked: bool,
        first: Option<RequestError>,
        second: Option<RequestError>,
    ) -> Run {
        let tmp = TempDir::new(tag);
        let rnsd = rnsd_with_row(destination(), age, online).await;
        let reader = rnsd.reader(&tmp);
        let transport = transport();
        let transitions = InterfaceTransitions::default();
        let book = RouteBook::default();
        if marked != Mark::No {
            book.mark([destination().to_hex_string()]);
        }
        if marked == Mark::AfterChange {
            transitions.record(TEAM_RELAY, TransitionKind::Up, SystemTime::now());
        }
        let routes = Routes {
            transport: &transport,
            rnsd: Some(&reader),
            transitions: &transitions,
            book: &book,
        };
        let tries = Mutex::new(Vec::new());
        let answers = Mutex::new(VecDeque::from([first, second]));
        let said = Arc::new(Mutex::new(Vec::new()));
        let sink = said.clone();
        let outcome = with_progress(
            Arc::new(move |text: &str| sink.lock().push(text.to_string())),
            routes.recovering(
                &destination(),
                || "Ann / laptop".to_string(),
                LinkTry {
                    link_timeout: Duration::from_secs(10),
                    path_requested: asked,
                    asked,
                },
                |link| {
                    tries.lock().push(link);
                    let answer = answers.lock().pop_front().flatten();
                    async move { answer.map_or(Ok(()), Err) }
                },
                the_error,
            ),
        )
        .await;
        let said = said.lock().clone();
        let drops = rnsd
            .asked()
            .iter()
            .filter(|verb| *verb == &("drop".to_string(), "path".to_string()))
            .count();
        Run {
            tries: tries.into_inner(),
            said,
            drops,
            outcome,
        }
    }

    const STALE: &str =
        "The route to Ann / laptop looked stale; asking the network for a new one...";

    /// A link that never came up over a row whose interface is offline is retried once,
    /// on a route asked for and with the timeout sized from the first hop, and the
    /// progress line says the route looked stale once.
    #[tokio::test]
    async fn a_failed_link_over_an_offline_interface_is_retried_once_and_said_once() {
        let run = recover(
            "routes-recover-offline",
            Duration::from_secs(3600),
            false,
            Mark::No,
            false,
            Some(never_linked()),
            None,
        )
        .await;
        assert_eq!(run.outcome, Ok(()));
        assert_eq!(run.drops, 1, "rnsd's row is dropped once");
        assert_eq!(run.tries.len(), 2);
        assert!(run.tries[1].path_requested);
        assert!(run.tries[1].link_timeout > Duration::from_secs(10));
        assert_eq!(run.said, [STALE]);
    }

    /// A first contact, a link right after a path request was sent, that never came up
    /// is tried once more as it is, without dropping the path and with the timeout sized
    /// from rnsd's first hop, saying nothing of a stale route; when the second try fails
    /// too, the error names the session and says a far session can take longer, never that
    /// it left.
    #[tokio::test]
    async fn a_first_contact_is_retried_once_and_reports_the_far_peer_wording() {
        let run = recover(
            "routes-recover-first-contact",
            Duration::from_secs(3600),
            true,
            Mark::No,
            true,
            Some(never_linked()),
            Some(never_linked()),
        )
        .await;
        assert_eq!(run.tries.len(), 2);
        assert_eq!(run.drops, 0, "a sound path is never dropped");
        assert_eq!(
            run.tries[1].link_timeout,
            retry_link_timeout(
                Some(Duration::from_secs_f64(6.05)),
                1,
                Duration::from_secs(10)
            )
        );
        assert!(run.said.is_empty(), "{:?}", run.said);
        let err = run.outcome.unwrap_err();
        assert!(
            matches!(
                &err,
                RequestError::LinkActivationTimeout { retried_to: Some(who), recently_seen: false, .. } if who == "Ann / laptop"
            ),
            "{err:?}"
        );
        let text = format!("{err}. {}.", crate::mesh::request::request_remedy(&err));
        assert_eq!(
            text,
            "No mesh link to Ann / laptop came up within 10 s. Far sessions (several hops, a public gateway) can take longer; try once more. `.mesh people list` shows when it was last heard."
        );
        assert!(!text.contains("may have left"), "{text}");
    }

    /// The first link never came up, the retry's did and carried the request, and no
    /// answer followed: the silence is reported as a request that may have arrived, never
    /// as the plain timeout a caller would store with a propagation node. A retry that
    /// failed another way, or a first try that timed out on an active link, keeps its
    /// error.
    #[tokio::test]
    async fn a_request_sent_on_the_retrys_late_link_and_unanswered_may_have_arrived() {
        let unanswered = || RequestError::Timeout {
            path: "/message".to_string(),
            after: Duration::from_secs(30),
        };
        let run = recover(
            "routes-recover-late-link",
            Duration::from_secs(3600),
            true,
            Mark::No,
            true,
            Some(never_linked()),
            Some(unanswered()),
        )
        .await;
        assert_eq!(run.tries.len(), 2);
        let err = run.outcome.unwrap_err();
        assert_eq!(
            err,
            RequestError::LateLinkUnanswered {
                path: "/message".to_string(),
                after: Duration::from_secs(30),
            }
        );
        assert!(!err.falls_back_to_store(), "never stored as well");
        assert_eq!(
            err.to_string(),
            "The request went out on a mesh link that came up late, and no answer came within 30 s; it may have arrived"
        );

        let run = recover(
            "routes-recover-late-link-dropped",
            Duration::from_secs(3600),
            true,
            Mark::No,
            true,
            Some(never_linked()),
            Some(RequestError::LinkDropped),
        )
        .await;
        assert_eq!(run.outcome, Err(RequestError::LinkDropped));

        let run = recover(
            "routes-recover-first-timeout",
            Duration::from_secs(3600),
            true,
            Mark::No,
            true,
            Some(unanswered()),
            None,
        )
        .await;
        assert_eq!(
            run.tries.len(),
            1,
            "a timeout on an active link is not retried"
        );
        assert_eq!(run.outcome, Err(unanswered()));
    }

    /// Return home after an interface came back: a trusted destination rnsd routes the
    /// long way round, 3 hops over the Public Hub, has its row dropped and is asked for,
    /// once per transition however many interfaces come back in it. A destination on the
    /// interface that came back is not asked, and a new transition asks again.
    #[tokio::test]
    async fn return_home_asks_once_per_transition_for_a_destination_on_a_longer_path_elsewhere() {
        let tmp = TempDir::new("routes-return-home");
        let learned = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let rnsd = FakeRnsd::start(move |(verb, what)| match (verb.as_str(), what.as_str()) {
            ("get", "path_table") => Value::Array(vec![map(vec![
                ("hash", Value::Binary(destination().as_slice().to_vec())),
                ("timestamp", Value::F64(learned)),
                ("via", Value::Binary(vec![0x11; 16])),
                ("hops", Value::from(3)),
                ("expires", Value::F64(learned + 604_800.0)),
                ("interface", Value::from(PUBLIC_HUB)),
            ])]),
            ("drop", "path") => Value::from(true),
            ("get", "next_hop_if_name") => Value::from("None"),
            other => panic!("unexpected request {other:?}"),
        })
        .await;
        let reader = rnsd.reader(&tmp);
        let transport = transport();
        let transitions = InterfaceTransitions::default();
        let book = RouteBook::default();
        let routes = Routes {
            transport: &transport,
            rnsd: Some(&reader),
            transitions: &transitions,
            book: &book,
        };
        let drops = || {
            rnsd.asked()
                .iter()
                .filter(|verb| *verb == &("drop".to_string(), "path".to_string()))
                .count()
        };
        let wanted: HashSet<String> = [destination().to_hex_string()].into();
        let asked = parking_lot::Mutex::new(HashSet::new());

        assert_eq!(
            routes.return_home_rnsd(TEAM_RELAY, &wanted, &asked).await,
            1
        );
        assert_eq!(drops(), 1);
        assert_eq!(
            routes
                .return_home_rnsd("TCPInterface[Propagation Node]", &wanted, &asked)
                .await,
            0,
            "asked once in the transition"
        );
        assert_eq!(drops(), 1);

        asked.lock().clear();
        assert_eq!(
            routes.return_home_rnsd(PUBLIC_HUB, &wanted, &asked).await,
            0,
            "the path already runs over the interface that came back"
        );
        assert_eq!(
            routes
                .return_home_rnsd(TEAM_RELAY, &HashSet::new(), &asked)
                .await,
            0,
            "nothing the node wants"
        );
        assert_eq!(
            routes.return_home_rnsd(TEAM_RELAY, &wanted, &asked).await,
            1,
            "a new transition asks again"
        );
        assert_eq!(drops(), 2);
    }

    /// A link that never came up over a sound path learned long ago is reported at once.
    #[tokio::test]
    async fn a_failed_link_over_a_sound_old_path_is_reported_as_it_is() {
        let run = recover(
            "routes-recover-sound",
            Duration::from_secs(3600),
            true,
            Mark::No,
            false,
            Some(never_linked()),
            None,
        )
        .await;
        assert_eq!(run.tries.len(), 1);
        assert_eq!(run.drops, 0);
        assert_eq!(run.outcome, Err(never_linked()));
        assert!(run.said.is_empty());
    }

    /// A path a network change marked suspect is asked for before the first try, which
    /// then needs no second; the stale line is said once.
    #[tokio::test]
    async fn a_marked_path_is_asked_for_before_the_first_try() {
        let run = recover(
            "routes-recover-marked",
            Duration::from_secs(3600),
            true,
            Mark::AfterChange,
            false,
            None,
            None,
        )
        .await;
        assert_eq!(run.outcome, Ok(()));
        assert_eq!(run.tries.len(), 1);
        assert!(run.tries[0].path_requested);
        assert_eq!(run.said, [STALE]);
    }

    /// A marked path learned again since its interface changed, as from the session's
    /// own greeting, is sound: it is neither dropped nor asked for before the try.
    #[tokio::test]
    async fn a_marked_path_learned_again_since_is_left_alone() {
        let run = recover(
            "routes-recover-marked-refreshed",
            Duration::from_secs(3600),
            true,
            Mark::Refreshed,
            false,
            None,
            None,
        )
        .await;
        assert_eq!(run.outcome, Ok(()));
        assert_eq!(run.tries.len(), 1);
        assert_eq!(run.drops, 0);
        assert!(!run.tries[0].path_requested);
        assert!(run.said.is_empty());
    }

    /// An up path refreshed seconds ago is no first contact: a link over it that never
    /// came up gives the plain error, with no drop and no retry.
    #[tokio::test]
    async fn a_failed_link_over_a_sound_fresh_path_is_reported_as_it_is() {
        let run = recover(
            "routes-recover-fresh",
            Duration::from_secs(2),
            true,
            Mark::No,
            false,
            Some(never_linked()),
            None,
        )
        .await;
        assert_eq!(run.tries.len(), 1);
        assert_eq!(run.drops, 0);
        assert_eq!(run.outcome, Err(never_linked()));
    }

    /// A marked path whose first try, after its drop and ask, still fails over an
    /// interface that is down is tried once more without a second drop or path request.
    #[tokio::test]
    async fn a_marked_path_still_suspect_is_asked_for_once_per_call() {
        let run = recover(
            "routes-recover-marked-down",
            Duration::from_secs(3600),
            false,
            Mark::AfterChange,
            false,
            Some(never_linked()),
            None,
        )
        .await;
        assert_eq!(run.outcome, Ok(()));
        assert_eq!(run.tries.len(), 2);
        assert_eq!(run.drops, 1, "one drop and one path request per call");
        assert_eq!(run.said, [STALE]);
    }

    /// A link over a suspect path that fails both tries reports the far-session wording,
    /// never that the session may have left.
    #[tokio::test]
    async fn a_suspect_path_that_fails_twice_never_says_may_have_left() {
        let run = recover(
            "routes-recover-suspect-twice",
            Duration::from_secs(3600),
            false,
            Mark::No,
            false,
            Some(never_linked()),
            Some(never_linked()),
        )
        .await;
        assert_eq!(run.tries.len(), 2);
        let err = run.outcome.unwrap_err();
        let text = format!("{err}. {}.", crate::mesh::request::request_remedy(&err));
        assert!(!text.contains("may have left"), "{text}");
        assert!(text.contains("Far sessions"), "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(online: Option<bool>, changed_since: bool) -> RouteFacts {
        RouteFacts::Known {
            interface: "TCPInterface[Team Relay/192.168.0.84:4244]".to_string(),
            online,
            changed_since,
            learned_at: SystemTime::now(),
            hops: 1,
        }
    }

    fn activation_timeout() -> RequestError {
        RequestError::LinkActivationTimeout {
            path: "/status".to_string(),
            after: Duration::from_secs(10),
            recently_seen: false,
            retried_to: None,
        }
    }

    /// A path over an interface that is down, came back since, or cannot be told is
    /// suspect; one over an interface up all along is sound.
    #[test]
    fn a_path_is_suspect_over_a_down_returned_or_unknown_interface() {
        assert_eq!(
            known(Some(false), false).suspicion(),
            Suspicion::Suspect(SuspectWhy::InterfaceDown)
        );
        assert_eq!(
            known(Some(true), true).suspicion(),
            Suspicion::Suspect(SuspectWhy::ChangedSince)
        );
        assert_eq!(
            known(None, false).suspicion(),
            Suspicion::Suspect(SuspectWhy::CannotTell)
        );
        assert_eq!(
            RouteFacts::Unknown.suspicion(),
            Suspicion::Suspect(SuspectWhy::CannotTell)
        );
        assert_eq!(known(Some(true), false).suspicion(), Suspicion::Sound);
        assert_eq!(RouteFacts::NoPath.suspicion(), Suspicion::NoPath);
    }

    /// A link failure over a suspect path drops, asks and retries; over a sound path it is
    /// reported, but for a first contact's link that never came up, which is tried once
    /// more; an answer of any kind, or no path at all, is reported.
    #[test]
    fn the_failed_link_decision() {
        let suspect = Suspicion::Suspect(SuspectWhy::InterfaceDown);
        let link_failures = [
            activation_timeout(),
            RequestError::LinkFailed("request not sent".to_string()),
            RequestError::LinkNotOpened("no link".to_string()),
        ];
        for err in &link_failures {
            assert_eq!(decide(err, suspect, false), Recovery::DropAndAsk, "{err:?}");
            assert_eq!(
                decide(err, Suspicion::Suspect(SuspectWhy::CannotTell), true),
                Recovery::DropAndAsk
            );
            assert_eq!(
                decide(err, Suspicion::Sound, false),
                Recovery::Report,
                "{err:?}"
            );
            assert_eq!(
                decide(err, Suspicion::NoPath, true),
                Recovery::Report,
                "{err:?}"
            );
        }
        assert_eq!(
            decide(&activation_timeout(), Suspicion::Sound, true),
            Recovery::RetryOnce
        );
        assert_eq!(
            decide(
                &RequestError::LinkFailed("x".to_string()),
                Suspicion::Sound,
                true
            ),
            Recovery::Report
        );
        let answered = RequestError::Timeout {
            path: "/status".to_string(),
            after: Duration::from_secs(30),
        };
        assert_eq!(decide(&answered, suspect, true), Recovery::Report);
        assert_eq!(
            decide(&RequestError::LinkClosed, suspect, true),
            Recovery::Report
        );
    }

    /// The retry waits for rnsd's first hop plus 6 s a hop and a second of slack, between
    /// the configured link timeout and twice it, with 6 s for the first hop when rnsd
    /// cannot be asked.
    #[test]
    fn the_retry_is_sized_from_the_first_hop_and_the_hops() {
        let ten = Duration::from_secs(10);
        assert_eq!(
            retry_link_timeout(Some(Duration::from_millis(6_050)), 1, ten),
            Duration::from_millis(13_050)
        );
        assert_eq!(
            retry_link_timeout(Some(Duration::from_secs(6)), 3, ten),
            Duration::from_secs(20),
            "capped at twice the link timeout"
        );
        assert_eq!(
            retry_link_timeout(Some(Duration::from_secs(6)), 2, ten),
            Duration::from_secs(19)
        );
        assert_eq!(retry_link_timeout(None, 1, ten), Duration::from_secs(13));
        assert_eq!(
            retry_link_timeout(Some(Duration::from_secs(1)), 0, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert_eq!(
            retry_link_timeout(None, 128, ten),
            Duration::from_secs(13),
            "the hop count of a destination with no path is read as one hop"
        );
    }

    /// A transition of one interface marks the paths over it; a change of the host's
    /// network marks them all.
    #[test]
    fn a_transition_marks_the_paths_over_its_interface() {
        let rows = vec![
            ("aa".to_string(), "TCPInterface[Team Relay]".to_string()),
            ("bb".to_string(), "TCPInterface[Public Hub]".to_string()),
            ("cc".to_string(), "TCPInterface[Team Relay]".to_string()),
        ];
        assert_eq!(
            paths_over(
                &rows,
                &Affected::Interface("TCPInterface[Team Relay]".to_string())
            ),
            ["aa", "cc"]
        );
        assert_eq!(paths_over(&rows, &Affected::Everything), ["aa", "bb", "cc"]);
    }

    /// Back home, only the trusted sessions and filed nodes routed the long way round over
    /// another interface are asked for again, each once.
    #[test]
    fn return_home_asks_only_for_wanted_destinations_routed_the_long_way() {
        let rows = vec![
            (
                "alice".to_string(),
                "TCPInterface[Public Hub]".to_string(),
                3,
            ),
            (
                "alice".to_string(),
                "TCPInterface[Public Hub]".to_string(),
                3,
            ),
            ("pn".to_string(), "TCPInterface[Public Hub]".to_string(), 2),
            (
                "near".to_string(),
                "TCPInterface[Public Hub]".to_string(),
                1,
            ),
            (
                "home".to_string(),
                "TCPInterface[Team Relay]".to_string(),
                2,
            ),
            (
                "stranger".to_string(),
                "TCPInterface[Public Hub]".to_string(),
                3,
            ),
        ];
        let wanted: HashSet<String> = ["alice", "pn", "near", "home"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            return_home_candidates(&rows, "TCPInterface[Team Relay]", &wanted),
            ["alice", "pn"]
        );
    }

    /// An in-process interface that disconnects is down until it connects again, which
    /// is a return; a connect with no disconnect before it is none.
    #[test]
    fn the_book_reads_an_in_process_interface_down_and_back() {
        let book = RouteBook::default();
        let iface = AddressHash::new_from_hex_string(&"ab".repeat(16)).unwrap();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        assert!(!book.note_interface_event(iface, InterfaceEventKind::Connected, t0));
        assert!(book.in_process_online(&iface));
        assert!(!book.note_interface_event(
            iface,
            InterfaceEventKind::Disconnected {
                reason: rns_transport::transport::DisconnectReason::PeerClosed,
            },
            t0
        ));
        assert!(!book.in_process_online(&iface));
        let back = t0 + Duration::from_secs(5);
        assert!(book.note_interface_event(iface, InterfaceEventKind::Connected, back));
        assert!(book.in_process_online(&iface));
        assert!(book.in_process_back_since(&iface, t0));
        assert!(!book.in_process_back_since(&iface, back));
    }

    /// After missed events, an in-process interface cannot be told until it reports
    /// again, and only one is down while any is.
    #[test]
    fn missed_events_leave_an_interface_unknown_until_it_reports() {
        let book = RouteBook::default();
        let iface = AddressHash::new_from_hex_string(&"ab".repeat(16)).unwrap();
        let other = AddressHash::new_from_hex_string(&"cd".repeat(16)).unwrap();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        assert!(!book.in_process_unknown(&iface));
        assert!(!book.any_down());
        book.note_lag();
        assert!(book.in_process_unknown(&iface) && book.in_process_unknown(&other));
        book.note_interface_event(
            iface,
            InterfaceEventKind::Disconnected {
                reason: rns_transport::transport::DisconnectReason::PeerClosed,
            },
            t0,
        );
        assert!(!book.in_process_unknown(&iface));
        assert!(book.in_process_unknown(&other));
        assert!(book.any_down());
    }

    /// A mark lasts until the destination is next requested.
    #[test]
    fn a_mark_is_taken_once() {
        let book = RouteBook::default();
        book.mark(["aa".to_string()]);
        assert!(book.is_marked("aa"));
        assert!(book.take_mark("aa"));
        assert!(!book.take_mark("aa"));
    }

    /// The progress sentence reaches a typed command's output once set, and nothing
    /// outside one.
    #[tokio::test]
    async fn the_progress_sentence_reaches_only_a_typed_command() {
        let said = Arc::new(Mutex::new(Vec::new()));
        let sink = said.clone();
        with_progress(
            Arc::new(move |text: &str| sink.lock().push(text.to_string())),
            async { say_progress(&stale_route_sentence("Ann / laptop")) },
        )
        .await;
        say_progress("unheard");
        assert_eq!(
            *said.lock(),
            ["The route to Ann / laptop looked stale; asking the network for a new one..."]
        );
    }
}
