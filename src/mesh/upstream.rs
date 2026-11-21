//! The transitions after which this node greets its relay again although its own
//! connection never dropped: the local rnsd regaining one of its upstream TCP connections,
//! and the host's interface addresses changing, as a move between networks does. Either
//! leaves everyone who knew this session unable to reach it until it announces. The
//! watch reads rnsd through the reader the LAN row uses, once per `UPSTREAM_POLL`, and
//! keeps each transition with its interface and time in `InterfaceTransitions`.

use crate::mesh::lan_count::{RnsdLanReader, RnsdTcpInterface};
use log::debug;
use parking_lot::Mutex;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, SystemTime};
use tokio::time::{Interval, MissedTickBehavior, interval_at};

/// How often the watch reads rnsd's interfaces and the host's addresses.
pub(crate) const UPSTREAM_POLL: Duration = Duration::from_secs(30);

/// The transitions kept, the oldest dropped first.
const TRANSITIONS_KEPT: usize = 64;

/// The host's interface addresses by interface name, loopback and link-local left out:
/// those do not change when the host moves between networks. `None` when they cannot be
/// read.
pub(crate) fn host_addresses() -> Option<BTreeSet<(String, IpAddr)>> {
    match if_addrs::get_if_addrs() {
        Ok(interfaces) => Some(
            interfaces
                .into_iter()
                .filter(|interface| !interface.is_loopback() && !interface.is_link_local())
                .map(|interface| {
                    let ip = interface.ip();
                    (interface.name, ip)
                })
                .collect(),
        ),
        Err(err) => {
            debug!("Mesh could not read this host's interface addresses: {err}");
            None
        }
    }
}

/// What one poll found that the node greets for: the rnsd TCP interfaces that came back
/// online, and the host interfaces whose addresses changed. One poll that finds both is
/// one greeting.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct UpstreamChange {
    pub reconnected: Vec<String>,
    pub network_changed: Vec<String>,
}

impl UpstreamChange {
    pub(crate) fn is_empty(&self) -> bool {
        self.reconnected.is_empty() && self.network_changed.is_empty()
    }
}

/// What the watch saw last: each rnsd TCP interface's online state, and the host's
/// addresses. Neither is set before its first reading, which only sets it.
#[derive(Debug, Default)]
pub(crate) struct UpstreamWatch {
    rnsd: Option<BTreeMap<String, bool>>,
    addresses: Option<BTreeSet<(String, IpAddr)>>,
}

impl UpstreamWatch {
    /// The watch with the host's addresses already read, so a change in the first poll
    /// interval is seen.
    pub(crate) fn with_addresses(addresses: Option<BTreeSet<(String, IpAddr)>>) -> Self {
        Self {
            rnsd: None,
            addresses,
        }
    }

    /// Folds one reading of rnsd's TCP interfaces in, recording each that went down or
    /// came back, and returns the ones that came back: online now and offline at the
    /// last reading. An interface first seen online is no transition, nor is anything in
    /// the first reading.
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

    /// One poll: one read of rnsd when there is one (serialised with every other read
    /// through `rnsd`, and no call at all once rnsd refused the key), then `addresses`.
    pub(crate) async fn read(
        &mut self,
        rnsd: Option<&RnsdLanReader>,
        addresses: impl FnOnce() -> Option<BTreeSet<(String, IpAddr)>>,
        transitions: &InterfaceTransitions,
    ) -> UpstreamChange {
        let reconnected = match rnsd {
            Some(reader) => match reader.read_tcp_interfaces().await {
                Some(interfaces) => {
                    self.watch
                        .observe_rnsd(&interfaces, transitions, SystemTime::now())
                }
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        let network_changed =
            self.watch
                .observe_addresses(addresses(), transitions, SystemTime::now());
        UpstreamChange {
            reconnected,
            network_changed,
        }
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
            if let IpAddr::V4(v4) = ip {
                assert!(!v4.is_link_local(), "{name} {ip}");
            }
        }
    }
}
