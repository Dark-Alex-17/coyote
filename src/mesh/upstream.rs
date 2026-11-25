//! The transitions after which this node greets its relay again although its own
//! connection never dropped: the local rnsd regaining one of its upstream TCP connections,
//! and the host's interface addresses changing, as a move between networks does. Either
//! leaves everyone who knew this session unable to reach it until it announces. The
//! watch reads rnsd through the reader the LAN row uses, once per `UPSTREAM_POLL`, and
//! keeps each transition with its interface and time in `InterfaceTransitions`. What one
//! network change does (several upstreams dropping and coming back, the addresses moving)
//! is settled into one transition by `TransitionSettle` and greeted once.

use crate::mesh::lan_count::{RnsdLanReader, RnsdTcpInterface};
use crate::mesh::redact_hashes;
use log::debug;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};
use tokio::time::{Interval, MissedTickBehavior, interval_at};

/// How often the watch reads rnsd's interfaces and the host's addresses.
pub(crate) const UPSTREAM_POLL: Duration = Duration::from_secs(10);

/// The quiet a transition needs before it is greeted: a reconnect or address change seen
/// within this of the last one belongs to the same transition.
pub(crate) const GREETING_DEBOUNCE_SECS: u64 = 10;

/// `GREETING_DEBOUNCE_SECS` as a duration.
pub(crate) const GREETING_DEBOUNCE: Duration = Duration::from_secs(GREETING_DEBOUNCE_SECS);

/// How often the watch reads while a transition settles.
pub(crate) const SETTLE_READ: Duration = Duration::from_secs(5);

/// The longest a transition settles, from its first sighting: an upstream that keeps
/// flapping is greeted after this however often it changes.
pub(crate) const SETTLE_CAP: Duration = Duration::from_secs(60);

/// The transitions kept, the oldest dropped first.
const TRANSITIONS_KEPT: usize = 64;

/// The host's interface addresses by interface name, loopback and link-local left out:
/// those do not change when the host moves between networks. An IPv6 address counts by
/// its /64 prefix, so the temporary addresses a host rotates on its own inside one
/// network are not a move. `None` when they cannot be read.
pub(crate) fn host_addresses() -> Option<BTreeSet<(String, IpAddr)>> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => Some(network_fingerprint(interfaces.into_iter().map(
            |interface| {
                let ip = interface.ip();
                (interface.name, ip)
            },
        ))),
        Err(err) => {
            debug!(
                "Mesh could not read this host's interface addresses: {}",
                redact_hashes(&err.to_string())
            );
            None
        }
    }
}

/// What of `addresses` says which network the host is on: every address but loopback and
/// link-local (all of `fe80::/10`), an IPv6 one cut to its /64 prefix.
fn network_fingerprint(
    addresses: impl IntoIterator<Item = (String, IpAddr)>,
) -> BTreeSet<(String, IpAddr)> {
    addresses
        .into_iter()
        .filter_map(|(name, ip)| match ip {
            IpAddr::V4(v4) if v4.is_loopback() || v4.is_link_local() => None,
            IpAddr::V6(v6) if v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80 => None,
            IpAddr::V6(v6) => {
                let prefix = u128::from(v6) & !u128::from(u64::MAX);
                Some((name, IpAddr::V6(prefix.into())))
            }
            IpAddr::V4(_) => Some((name, ip)),
        })
        .collect()
}

/// What one poll found that the node greets for: the rnsd TCP interfaces that came back
/// online, and the host interfaces whose addresses changed. One poll that finds both is
/// one greeting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct UpstreamChange {
    pub reconnected: Vec<String>,
    pub network_changed: Vec<String>,
}

impl UpstreamChange {
    pub(crate) fn is_empty(&self) -> bool {
        self.reconnected.is_empty() && self.network_changed.is_empty()
    }

    /// Adds `other`'s interfaces, each named once.
    fn absorb(&mut self, other: UpstreamChange) {
        for (into, from) in [
            (&mut self.reconnected, other.reconnected),
            (&mut self.network_changed, other.network_changed),
        ] {
            for name in from {
                if !into.contains(&name) {
                    into.push(name);
                }
            }
        }
    }
}

/// The transition the watch is settling: every change seen from its first sighting until
/// `GREETING_DEBOUNCE` passes with nothing new, or `SETTLE_CAP` from the first sighting,
/// greeted as one. One network change drops and brings back each of rnsd's upstreams on
/// its own reconnect timer and moves the host's addresses; greeted per sighting, it would
/// spend the hourly budget on its own.
#[derive(Debug, Default)]
pub(crate) struct TransitionSettle {
    open: Option<Settling>,
}

#[derive(Debug)]
struct Settling {
    first: tokio::time::Instant,
    last: tokio::time::Instant,
    change: UpstreamChange,
}

impl TransitionSettle {
    /// Whether a transition is settling, so the watch reads every `SETTLE_READ`.
    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Folds one reading's `change`, read at `now`, in. A change that opens a transition
    /// is a first sighting, which `opens` says. Returns the transition once it settled,
    /// with its first sighting, and closes it.
    pub(crate) fn observe(
        &mut self,
        change: &UpstreamChange,
        now: tokio::time::Instant,
    ) -> SettleStep {
        let opens = !change.is_empty() && self.open.is_none();
        if !change.is_empty() {
            match &mut self.open {
                Some(open) => {
                    open.change.absorb(change.clone());
                    open.last = now;
                }
                None => {
                    self.open = Some(Settling {
                        first: now,
                        last: now,
                        change: change.clone(),
                    });
                }
            }
        }
        let settled = self.open.as_ref().is_some_and(|open| {
            now.saturating_duration_since(open.last) >= GREETING_DEBOUNCE
                || now.saturating_duration_since(open.first) >= SETTLE_CAP
        });
        let settled = if settled {
            self.open.take().map(|open| (open.first, open.change))
        } else {
            None
        };
        SettleStep { opens, settled }
    }
}

/// What one reading did to the transition being settled.
#[derive(Debug, Default)]
pub(crate) struct SettleStep {
    /// The reading opened a transition: its change is the first sighting.
    pub opens: bool,
    /// The transition that settled, with its first sighting.
    pub settled: Option<(tokio::time::Instant, UpstreamChange)>,
}

/// Whether each LAN interface reaches anyone, by the interface: an rnsd AutoInterface or
/// an in-process `lan` channel. A LAN that goes from nobody to somebody has come back,
/// and the routes the long way round may have a shorter one over it.
#[derive(Debug, Default)]
pub(crate) struct LanPresence {
    seen: BTreeMap<String, u64>,
}

impl LanPresence {
    /// Folds one reading of `counts` (interface, hosts it reaches; `None` unread) in and
    /// returns the interfaces that reach someone now and reached nobody at the last
    /// reading. An interface's first reading is no transition, and an unread one keeps
    /// the last count.
    pub(crate) fn observe(&mut self, counts: &[(String, Option<u64>)]) -> Vec<String> {
        let mut gained = Vec::new();
        for (name, count) in counts {
            let Some(count) = count else { continue };
            if self.seen.insert(name.clone(), *count) == Some(0) && *count > 0 {
                gained.push(name.clone());
            }
        }
        gained
    }
}

/// What the watch saw last: each rnsd TCP interface's online state, and the host's
/// addresses. Neither is set before its first reading, which only sets it.
#[derive(Debug, Default)]
pub(crate) struct UpstreamWatch {
    rnsd: Option<BTreeMap<String, bool>>,
    addresses: Option<BTreeSet<(String, IpAddr)>>,
    /// The host's network changed at the last reading: rnsd's upstream may still have
    /// been down then, or have dropped and come back inside one poll, so the next
    /// reading that finds it online counts as a reconnect once more.
    after_move: bool,
}

impl UpstreamWatch {
    /// The watch with the host's addresses already read, so a change in the first poll
    /// interval is seen.
    pub(crate) fn with_addresses(addresses: Option<BTreeSet<(String, IpAddr)>>) -> Self {
        Self {
            rnsd: None,
            addresses,
            after_move: false,
        }
    }

    /// Takes `interfaces` as the first rnsd reading, when none was taken yet: the read
    /// made at join, so an upstream that comes back before the first poll is seen.
    pub(crate) fn seed_rnsd(&mut self, interfaces: &[RnsdTcpInterface]) {
        if self.rnsd.is_none() {
            self.rnsd = Some(
                interfaces
                    .iter()
                    .map(|interface| (interface.name.clone(), interface.online))
                    .collect(),
            );
        }
    }

    /// Folds one reading of rnsd's TCP interfaces in, recording each that went down or
    /// came back, and returns the ones that came back: online now and offline at the
    /// last reading. An interface first seen online is no transition, nor is anything in
    /// the first reading. After a network change the first reading with an interface
    /// online returns those online ones too, once.
    pub(crate) fn observe_rnsd(
        &mut self,
        interfaces: &[RnsdTcpInterface],
        transitions: &InterfaceTransitions,
        now: SystemTime,
    ) -> Vec<String> {
        let current: BTreeMap<String, bool> = interfaces
            .iter()
            .map(|interface| (interface.name.clone(), interface.online))
            .collect();
        let Some(previous) = self.rnsd.replace(current.clone()) else {
            return Vec::new();
        };
        let mut back = Vec::new();
        for (name, online) in &current {
            match (previous.get(name), online) {
                (Some(false), true) => {
                    transitions.record(name, TransitionKind::Up, now);
                    back.push(name.clone());
                }
                (Some(true), false) => transitions.record(name, TransitionKind::Down, now),
                _ => {}
            }
        }
        if std::mem::take(&mut self.after_move) {
            let online: Vec<String> = current
                .iter()
                .filter(|(name, online)| **online && !back.contains(*name))
                .map(|(name, _)| name.clone())
                .collect();
            if online.is_empty() && back.is_empty() {
                self.after_move = true;
            }
            back.extend(online);
        }
        back
    }

    /// Folds one reading of the host's addresses in and returns the interfaces whose
    /// addresses changed, each recorded; an unreadable reading keeps the last one.
    pub(crate) fn observe_addresses(
        &mut self,
        addresses: Option<BTreeSet<(String, IpAddr)>>,
        transitions: &InterfaceTransitions,
        now: SystemTime,
    ) -> Vec<String> {
        let Some(current) = addresses else {
            return Vec::new();
        };
        let Some(previous) = self.addresses.replace(current.clone()) else {
            return Vec::new();
        };
        let changed: BTreeSet<String> = previous
            .symmetric_difference(&current)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &changed {
            transitions.record(name, TransitionKind::AddressesChanged, now);
        }
        self.after_move |= !changed.is_empty();
        changed.into_iter().collect()
    }
}

/// The poll the interface watcher runs: one tick per `UPSTREAM_POLL`, the first one
/// interval after start, and a host that slept reads once rather than once per tick
/// missed.
pub(crate) struct UpstreamPoll {
    ticks: Interval,
    watch: UpstreamWatch,
}

impl UpstreamPoll {
    pub(crate) fn new(addresses: Option<BTreeSet<(String, IpAddr)>>) -> Self {
        let mut ticks = interval_at(tokio::time::Instant::now() + UPSTREAM_POLL, UPSTREAM_POLL);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            ticks,
            watch: UpstreamWatch::with_addresses(addresses),
        }
    }

    /// Waits for the next tick.
    pub(crate) async fn tick(&mut self) {
        self.ticks.tick().await;
    }

    /// Waits for the next read: `SETTLE_READ` while a transition settles, after which the
    /// regular ticks restart a whole `UPSTREAM_POLL` on; the next tick otherwise.
    pub(crate) async fn tick_for(&mut self, settling: bool) {
        if settling {
            tokio::time::sleep(SETTLE_READ).await;
            self.ticks.reset();
        } else {
            self.tick().await;
        }
    }

    /// One poll: one read of rnsd when there is one (serialised with every other read
    /// through `rnsd`, and no call at all once rnsd refused the key), then `addresses`.
    /// The first poll takes what an earlier read, the one at join, saw as its starting
    /// point. A slow rnsd holds the watcher for the read, `RPC_ATTEMPTS` budgets at
    /// most; the interface events wait in their streams meanwhile.
    pub(crate) async fn read(
        &mut self,
        rnsd: Option<&RnsdLanReader>,
        addresses: impl FnOnce() -> Option<BTreeSet<(String, IpAddr)>>,
        transitions: &InterfaceTransitions,
    ) -> UpstreamChange {
        let reconnected = match rnsd {
            Some(reader) => {
                if let Some(seen) = reader.tcp_interfaces_seen() {
                    self.watch.seed_rnsd(&seen);
                }
                match reader.read_tcp_interfaces().await {
                    Some(interfaces) => {
                        self.watch
                            .observe_rnsd(&interfaces, transitions, SystemTime::now())
                    }
                    None => Vec::new(),
                }
            }
            None => Vec::new(),
        };
        let network_changed =
            self.watch
                .observe_addresses(addresses(), transitions, SystemTime::now());
        if !reconnected.is_empty() {
            // The upstream is already back in the poll that saw the move: greeting now
            // covers both, and nothing is owed when the next poll finds it online.
            self.watch.after_move = false;
        }
        UpstreamChange {
            reconnected,
            network_changed,
        }
    }
}

/// Holds a network change back one poll on a node whose relays will greet by
/// themselves, so one move greets once: a TCP client that reconnects after the move
/// greets its relay with its own announce, and an rnsd upstream that comes back shows up
/// in the next poll. A node with neither greets a change at once.
#[derive(Debug, Default)]
pub(crate) struct MoveGreeting {
    pending: Option<(tokio::time::Instant, Vec<String>)>,
}

impl MoveGreeting {
    /// What of `change`, read at `now`, is greeted now. `defers` says the node has a TCP
    /// client or an rnsd to wait for; `client_greeted_at` is when a TCP client's own
    /// reconnect greeting last went out. A change held back is greeted at the next poll,
    /// with whatever that poll found, unless a client greeted since, which covered it.
    pub(crate) fn decide(
        &mut self,
        mut change: UpstreamChange,
        defers: bool,
        client_greeted_at: Option<tokio::time::Instant>,
        now: tokio::time::Instant,
    ) -> UpstreamChange {
        if let Some((since, moved)) = self.pending.take() {
            if client_greeted_at.is_some_and(|at| at >= since) {
                debug!("Mesh left the network change to the relay greeting that followed it");
                return UpstreamChange::default();
            }
            let mut network_changed = moved;
            network_changed.extend(change.network_changed);
            change.network_changed = network_changed;
            return change;
        }
        if defers && !change.network_changed.is_empty() && change.reconnected.is_empty() {
            self.pending = Some((now, std::mem::take(&mut change.network_changed)));
        }
        change
    }
}

/// What happened to an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionKind {
    /// An rnsd TCP interface went offline.
    Down,
    /// An rnsd TCP interface came back online.
    Up,
    /// A host interface's addresses changed.
    AddressesChanged,
}

/// One transition: the interface, what happened to it and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterfaceTransition {
    pub interface: String,
    pub kind: TransitionKind,
    pub at: SystemTime,
}

impl fmt::Display for InterfaceTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            TransitionKind::Down => "went offline",
            TransitionKind::Up => "came back online",
            TransitionKind::AddressesChanged => "changed addresses",
        };
        let ago = SystemTime::now()
            .duration_since(self.at)
            .unwrap_or_default()
            .as_secs();
        write!(f, "interface {} {what} {ago}s ago", self.interface)
    }
}

/// The latest interface transitions, newest last, at most `TRANSITIONS_KEPT`, in memory
/// for the run: what a path learned over an interface is judged against.
#[derive(Debug, Default)]
pub(crate) struct InterfaceTransitions {
    recent: Mutex<VecDeque<InterfaceTransition>>,
}

impl InterfaceTransitions {
    pub(crate) fn record(&self, interface: &str, kind: TransitionKind, at: SystemTime) {
        let transition = InterfaceTransition {
            interface: interface.to_string(),
            kind,
            at,
        };
        debug!("Mesh noted that {transition}");
        let mut recent = self.recent.lock();
        if recent.len() == TRANSITIONS_KEPT {
            recent.pop_front();
        }
        recent.push_back(transition);
    }

    /// Every transition kept, oldest first.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<InterfaceTransition> {
        self.recent.lock().iter().cloned().collect()
    }

    /// Whether a path learned over the rnsd interface `interface` at `since` has seen
    /// the network change under it: that interface came back online after `since`, or
    /// the host's addresses changed after it, which moves every path. A transition the
    /// record has dropped as too old is not seen.
    pub(crate) fn changed_since(&self, interface: &str, since: SystemTime) -> bool {
        self.recent.lock().iter().any(|transition| {
            transition.at > since
                && match transition.kind {
                    TransitionKind::Up => transition.interface == interface,
                    TransitionKind::AddressesChanged => true,
                    TransitionKind::Down => false,
                }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn tcp(name: &str, online: bool) -> RnsdTcpInterface {
        RnsdTcpInterface {
            name: name.to_string(),
            online,
        }
    }

    fn addresses(pairs: &[(&str, [u8; 4])]) -> Option<BTreeSet<(String, IpAddr)>> {
        Some(
            pairs
                .iter()
                .map(|(name, ip)| (name.to_string(), IpAddr::V4(Ipv4Addr::from(*ip))))
                .collect(),
        )
    }

    const RELAY: &str = "TCPInterface[Team Relay/192.168.0.84:4244]";

    /// An rnsd TCP interface that went offline and came back greets once; the reading
    /// that sets the watch, a second online reading and an interface first seen online
    /// do not. Both edges are recorded with the interface and the time.
    #[test]
    fn only_an_offline_to_online_edge_between_two_readings_is_a_reconnect() {
        let transitions = InterfaceTransitions::default();
        let mut watch = UpstreamWatch::default();
        let now = SystemTime::now();
        assert!(
            watch
                .observe_rnsd(&[tcp(RELAY, false)], &transitions, now)
                .is_empty()
        );
        assert_eq!(
            watch.observe_rnsd(&[tcp(RELAY, true)], &transitions, now),
            [RELAY],
            "offline then online"
        );
        assert!(
            watch
                .observe_rnsd(&[tcp(RELAY, true)], &transitions, now)
                .is_empty(),
            "online then online"
        );
        assert!(
            watch
                .observe_rnsd(
                    &[tcp(RELAY, true), tcp("TCPInterface[new]", true)],
                    &transitions,
                    now
                )
                .is_empty(),
            "an interface first seen online"
        );
        assert!(
            watch
                .observe_rnsd(&[tcp(RELAY, false)], &transitions, now)
                .is_empty()
        );
        let kinds: Vec<(String, TransitionKind)> = transitions
            .snapshot()
            .into_iter()
            .map(|transition| (transition.interface, transition.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (RELAY.to_string(), TransitionKind::Up),
                (RELAY.to_string(), TransitionKind::Down),
            ]
        );
        assert!(
            transitions
                .snapshot()
                .iter()
                .all(|transition| transition.at == now)
        );

        let mut first = UpstreamWatch::default();
        assert!(
            first
                .observe_rnsd(&[tcp(RELAY, true)], &transitions, now)
                .is_empty(),
            "the first reading never greets"
        );
    }

    /// A changed address set is a network change once, naming the interfaces whose
    /// addresses moved; the same set again, an unreadable reading and the first reading
    /// are not.
    #[test]
    fn a_changed_address_set_is_one_network_change() {
        let transitions = InterfaceTransitions::default();
        let now = SystemTime::now();
        let home = addresses(&[("en0", [192, 168, 0, 10]), ("utun3", [10, 8, 0, 2])]);
        let hotspot = addresses(&[("en0", [172, 20, 10, 4]), ("utun3", [10, 8, 0, 2])]);
        let mut watch = UpstreamWatch::default();
        assert!(
            watch
                .observe_addresses(home.clone(), &transitions, now)
                .is_empty()
        );
        assert!(
            watch
                .observe_addresses(home.clone(), &transitions, now)
                .is_empty()
        );
        assert!(watch.observe_addresses(None, &transitions, now).is_empty());
        assert_eq!(
            watch.observe_addresses(hotspot.clone(), &transitions, now),
            ["en0"]
        );
        assert!(
            watch
                .observe_addresses(hotspot, &transitions, now)
                .is_empty()
        );
        assert_eq!(
            transitions.snapshot(),
            [InterfaceTransition {
                interface: "en0".to_string(),
                kind: TransitionKind::AddressesChanged,
                at: now,
            }]
        );

        let mut seeded = UpstreamWatch::with_addresses(home);
        assert_eq!(
            seeded.observe_addresses(addresses(&[]), &transitions, now),
            ["en0", "utun3"],
            "addresses read at start are the first reading"
        );
    }

    /// One move, one greeting: on a node with relays that greet by themselves the move
    /// waits a poll; a TCP client that reconnected and greeted meanwhile covers it, and
    /// otherwise the next poll greets it once, together with an rnsd upstream that came
    /// back. A node with no such relay greets the move at once.
    #[test]
    fn one_move_is_greeted_once() {
        let at = tokio::time::Instant::now();
        let poll = |seconds| at + Duration::from_secs(seconds);
        let moved = || UpstreamChange {
            reconnected: Vec::new(),
            network_changed: vec!["en0".to_string()],
        };

        let mut covered = MoveGreeting::default();
        assert!(
            covered.decide(moved(), true, None, poll(0)).is_empty(),
            "held back"
        );
        assert!(
            covered
                .decide(UpstreamChange::default(), true, Some(poll(12)), poll(30))
                .is_empty(),
            "the relay reconnect's own greeting covered the move"
        );

        let mut held = MoveGreeting::default();
        assert!(
            held.decide(
                moved(),
                true,
                Some(poll(0) - Duration::from_secs(5)),
                poll(0)
            )
            .is_empty()
        );
        let upstream_back = UpstreamChange {
            reconnected: vec!["TCPInterface[Team Relay]".to_string()],
            network_changed: Vec::new(),
        };
        assert_eq!(
            held.decide(
                upstream_back,
                true,
                Some(poll(0) - Duration::from_secs(5)),
                poll(30)
            ),
            UpstreamChange {
                reconnected: vec!["TCPInterface[Team Relay]".to_string()],
                network_changed: vec!["en0".to_string()],
            },
            "one greeting for the move and the upstream that came back"
        );
        assert!(
            held.decide(UpstreamChange::default(), true, None, poll(60))
                .is_empty()
        );

        let mut lan_only = MoveGreeting::default();
        assert_eq!(lan_only.decide(moved(), false, None, poll(0)), moved());
        assert!(
            lan_only
                .decide(UpstreamChange::default(), false, None, poll(30))
                .is_empty()
        );
    }

    /// An IPv6 address counts by its /64 prefix: a temporary address rotating inside one
    /// network is no move, a new prefix is, and no `fe80::/10` address is counted.
    #[test]
    fn ipv6_counts_by_its_prefix_and_link_local_is_left_out() {
        let v6 = |text: &str| ("en0".to_string(), text.parse::<IpAddr>().unwrap());
        let before = network_fingerprint([v6("2001:db8:1:2::aaaa"), v6("fe80::1")]);
        let rotated = network_fingerprint([v6("2001:db8:1:2::bbbb"), v6("febf::1")]);
        assert_eq!(before, rotated, "same /64, link-local left out");
        assert_eq!(rotated.len(), 1);
        let moved = network_fingerprint([v6("2001:db8:9:9::aaaa")]);
        let transitions = InterfaceTransitions::default();
        let mut watch = UpstreamWatch::with_addresses(Some(before));
        let now = SystemTime::now();
        assert!(
            watch
                .observe_addresses(Some(rotated), &transitions, now)
                .is_empty()
        );
        assert_eq!(
            watch.observe_addresses(Some(moved), &transitions, now),
            ["en0"]
        );
        assert!(
            network_fingerprint([
                ("lo0".to_string(), "::1".parse().unwrap()),
                ("lo0".to_string(), "127.0.0.1".parse().unwrap()),
                ("en0".to_string(), "169.254.3.4".parse().unwrap()),
            ])
            .is_empty()
        );
    }

    /// After a network change, rnsd's upstream may have been down when the change was
    /// greeted, or dropped and came back between two readings: the next reading that
    /// finds it online counts as a reconnect once, and the one after does not.
    #[test]
    fn after_a_network_change_the_next_online_upstream_reading_greets_once() {
        let transitions = InterfaceTransitions::default();
        let now = SystemTime::now();
        let mut watch = UpstreamWatch::with_addresses(addresses(&[("en0", [192, 168, 0, 10])]));
        assert!(
            watch
                .observe_rnsd(&[tcp(RELAY, true)], &transitions, now)
                .is_empty()
        );
        assert_eq!(
            watch.observe_addresses(addresses(&[("en0", [172, 20, 10, 4])]), &transitions, now),
            ["en0"]
        );
        assert!(
            watch
                .observe_rnsd(&[tcp(RELAY, false)], &transitions, now)
                .is_empty(),
            "still down after the move"
        );
        assert_eq!(
            watch.observe_rnsd(&[tcp(RELAY, true)], &transitions, now),
            [RELAY],
            "back online: an edge, counted once"
        );
        assert!(
            watch
                .observe_rnsd(&[tcp(RELAY, true)], &transitions, now)
                .is_empty()
        );

        let mut quick = UpstreamWatch::with_addresses(addresses(&[("en0", [192, 168, 0, 10])]));
        assert!(
            quick
                .observe_rnsd(&[tcp(RELAY, true)], &transitions, now)
                .is_empty()
        );
        quick.observe_addresses(addresses(&[("en0", [172, 20, 10, 4])]), &transitions, now);
        assert_eq!(
            quick.observe_rnsd(&[tcp(RELAY, true)], &transitions, now),
            [RELAY],
            "dropped and back inside one poll: online after the move counts once"
        );
        assert!(
            quick
                .observe_rnsd(&[tcp(RELAY, true)], &transitions, now)
                .is_empty()
        );
    }

    /// The read made at join is the starting point: an upstream offline then and online
    /// at the first poll is a reconnect.
    #[test]
    fn the_read_at_join_is_the_first_reading() {
        let transitions = InterfaceTransitions::default();
        let mut watch = UpstreamWatch::default();
        watch.seed_rnsd(&[tcp(RELAY, false)]);
        watch.seed_rnsd(&[tcp(RELAY, true)]);
        assert_eq!(
            watch.observe_rnsd(&[tcp(RELAY, true)], &transitions, SystemTime::now()),
            [RELAY],
            "a later seed does not replace the first"
        );
    }

    /// The record keeps the newest transitions only.
    #[test]
    fn the_record_keeps_the_newest_transitions() {
        let transitions = InterfaceTransitions::default();
        for index in 0..=TRANSITIONS_KEPT {
            transitions.record(&format!("if{index}"), TransitionKind::Up, SystemTime::now());
        }
        let kept = transitions.snapshot();
        assert_eq!(kept.len(), TRANSITIONS_KEPT);
        assert_eq!(kept[0].interface, "if1");
    }

    /// This host's own addresses read, none of them loopback or link-local.
    #[test]
    fn the_host_addresses_leave_out_loopback_and_link_local() {
        let read = host_addresses().expect("this host's addresses can be read");
        for (name, ip) in read {
            assert!(!ip.is_loopback(), "{name} {ip}");
            match ip {
                IpAddr::V4(v4) => assert!(!v4.is_link_local(), "{name} {ip}"),
                IpAddr::V6(v6) => {
                    assert_ne!(v6.segments()[0] & 0xffc0, 0xfe80, "{name} {ip}");
                    assert_eq!(u128::from(v6) & u128::from(u64::MAX), 0, "{name} {ip}");
                }
            }
        }
    }

    fn reconnect(name: &str) -> UpstreamChange {
        UpstreamChange {
            reconnected: vec![name.to_string()],
            network_changed: Vec::new(),
        }
    }

    const PN: &str = "TCPInterface[Propagation Node/192.168.0.84:4242]";
    const HUB: &str = "TCPInterface[Public Hub/reticulum.nepamesh.com:4242]";

    /// One network switch, as the hotspot test saw it: the addresses move, then Team
    /// Relay, the propagation node and the public hub come back one read apart. Each read
    /// lands within `GREETING_DEBOUNCE` of the last, so it is one transition, settled once
    /// `GREETING_DEBOUNCE` passes with nothing new and holding every interface once; the
    /// first sighting alone opens it. A second switch later is a transition of its own.
    #[test]
    fn the_changes_of_one_network_switch_settle_into_one_transition() {
        let start = tokio::time::Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let mut settle = TransitionSettle::default();
        let first = settle.observe(
            &UpstreamChange {
                reconnected: Vec::new(),
                network_changed: vec!["en0".to_string()],
            },
            at(0),
        );
        assert!(first.opens && first.settled.is_none() && settle.is_open());
        for (secs, name) in [(5, RELAY), (10, PN), (15, HUB), (20, PN)] {
            let step = settle.observe(&reconnect(name), at(secs));
            assert!(!step.opens && step.settled.is_none(), "{name} at {secs} s");
        }
        assert!(
            settle
                .observe(&UpstreamChange::default(), at(25))
                .settled
                .is_none(),
            "5 s of quiet is not yet the debounce"
        );
        let settled = settle.observe(&UpstreamChange::default(), at(30));
        assert!(!settled.opens);
        let (since, change) = settled.settled.expect("10 s of quiet settles it");
        assert_eq!(since, at(0), "the first sighting");
        assert_eq!(change.network_changed, ["en0"]);
        assert_eq!(change.reconnected, [RELAY, PN, HUB], "each interface once");
        assert!(!settle.is_open());
        assert!(
            settle
                .observe(&UpstreamChange::default(), at(40))
                .settled
                .is_none(),
            "nothing is settled twice"
        );

        let again = settle.observe(&reconnect(RELAY), at(600));
        assert!(again.opens, "a later switch opens a transition of its own");
        let (since, change) = settle
            .observe(&UpstreamChange::default(), at(610))
            .settled
            .unwrap();
        assert_eq!(
            (since, change.reconnected),
            (at(600), vec![RELAY.to_string()])
        );
    }

    /// An upstream that keeps flapping, a change every read, is settled `SETTLE_CAP`
    /// after its first sighting with everything seen until then.
    #[test]
    fn a_transition_that_keeps_changing_settles_at_the_cap() {
        let start = tokio::time::Instant::now();
        let mut settle = TransitionSettle::default();
        let mut secs = 0;
        let settled = loop {
            let name = if secs % 10 == 0 { RELAY } else { HUB };
            let step = settle.observe(&reconnect(name), start + Duration::from_secs(secs));
            if let Some(settled) = step.settled {
                break settled;
            }
            secs += 5;
            assert!(secs <= 120, "never settled");
        };
        assert_eq!(Duration::from_secs(secs), SETTLE_CAP);
        assert_eq!(settled.1.reconnected, [RELAY, HUB]);
    }

    /// Debouncing makes the hourly budget count transitions: three switches an hour,
    /// each flapping three upstreams, take three greetings, under the cap of four.
    #[test]
    fn a_switch_flapping_three_upstreams_takes_one_greeting() {
        let start = tokio::time::Instant::now();
        let mut settle = TransitionSettle::default();
        let mut greetings = 0;
        for switch in 0..3u64 {
            let base = switch * 1200;
            for (offset, name) in [(0, RELAY), (5, PN), (10, HUB)] {
                if settle
                    .observe(&reconnect(name), start + Duration::from_secs(base + offset))
                    .settled
                    .is_some()
                {
                    greetings += 1;
                }
            }
            for quiet in [15, 20, 25] {
                if settle
                    .observe(
                        &UpstreamChange::default(),
                        start + Duration::from_secs(base + quiet),
                    )
                    .settled
                    .is_some()
                {
                    greetings += 1;
                }
            }
        }
        assert_eq!(greetings, 3);
        assert!(greetings < crate::mesh::announce::RECONNECT_GREETINGS_PER_HOUR);
    }

    /// The reads run every `UPSTREAM_POLL`, and every `SETTLE_READ` while a transition
    /// settles: a return home asked at the first sighting goes out within about 15 s of
    /// the transition, and the debounce is read finely enough to merge what one switch
    /// does.
    #[test]
    fn the_watch_reads_often_enough_for_the_debounce_and_return_home() {
        assert_eq!(GREETING_DEBOUNCE_SECS, 10);
        assert!(UPSTREAM_POLL <= Duration::from_secs(10));
        assert!(SETTLE_READ < GREETING_DEBOUNCE);
        assert!(SETTLE_CAP > GREETING_DEBOUNCE);
    }

    /// A LAN interface that goes from nobody to somebody has come back; its first reading,
    /// an unread count, and a LAN that keeps its hosts or loses them are no such change.
    #[test]
    fn a_lan_that_reaches_hosts_again_is_seen_once() {
        let mut presence = LanPresence::default();
        let read = |count: Option<u64>| vec![("AutoInterface".to_string(), count)];
        assert!(
            presence.observe(&read(Some(0))).is_empty(),
            "the first reading"
        );
        assert!(presence.observe(&read(None)).is_empty(), "unread");
        assert_eq!(presence.observe(&read(Some(2))), ["AutoInterface"]);
        assert!(
            presence.observe(&read(Some(3))).is_empty(),
            "still reaching hosts"
        );
        assert!(presence.observe(&read(Some(0))).is_empty(), "lost them");
        assert_eq!(
            presence.observe(&read(Some(1))),
            ["AutoInterface"],
            "back again"
        );
        let mut fresh = LanPresence::default();
        assert!(
            fresh.observe(&read(Some(4))).is_empty(),
            "a LAN first read with hosts is no transition"
        );
    }
}
