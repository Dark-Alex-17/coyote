//! The LXMF propagation nodes this node has heard announce, and which of them a fetch
//! should ask. Kept in memory only: a `DestinationDesc` is not serialisable, nodes
//! re-announce on their own schedule, and what has to survive a restart (which messages
//! were already fetched) lives in the fetch store instead.

use crate::mesh::peers::PeerChange;
use crate::mesh::propagation::{PropagationNode, PropagationNodeError};
use crate::mesh::propagation_fetch::FetchError;
use crate::mesh::r3::short;

use parking_lot::Mutex;
use rns_transport::destination::DestinationDesc;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::time::SystemTime;

/// Upper bound on remembered propagation nodes; the least recently heard is evicted first.
/// A mesh runs a handful of nodes, not hundreds: every one stores every message for every
/// recipient it serves, so operators deploy few of them, and a fetch only ever asks one.
pub(crate) const PROPAGATION_NODE_TABLE_MAX_ENTRIES: usize = 32;

/// A propagation node as last announced, with how far away it was and when.
#[derive(Clone)]
pub(crate) struct PropagationNodeRecord {
    pub node: PropagationNode,
    pub hops: u8,
    pub last_seen: SystemTime,
}

/// Propagation nodes seen on the mesh, keyed by destination hash. Time is always passed
/// in so ordering is testable without a clock.
pub(crate) struct PropagationNodeTable {
    inner: Mutex<BTreeMap<String, PropagationNodeRecord>>,
}

impl PropagationNodeTable {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Files one received announce when it is a propagation node's. Returns `false` for
    /// every announce that is not an `lxmf.propagation` one, so the caller can go on to
    /// read it as something else; a propagation announce that does not parse is dropped
    /// with a debug line and still counts as handled.
    pub(crate) fn observe_announce(
        &self,
        destination: &DestinationDesc,
        app_data: &[u8],
        hops: u8,
        now: SystemTime,
    ) -> bool {
        let destination_hex = destination.address_hash.to_hex_string();
        match PropagationNode::from_announce(destination, app_data) {
            Ok(node) => {
                let change = self.observe(node, hops, now);
                debug!(
                    "{} propagation node {} ({hops} hops)",
                    match change {
                        PeerChange::Added => "Added",
                        PeerChange::Refreshed => "Refreshed",
                    },
                    short(&destination_hex)
                );
                true
            }
            Err(PropagationNodeError::NotAPropagationNode) => false,
            Err(err) => {
                debug!(
                    "Ignored propagation node announce from {} ({hops} hops): {err}",
                    short(&destination_hex)
                );
                true
            }
        }
    }

    pub(crate) fn observe(&self, node: PropagationNode, hops: u8, now: SystemTime) -> PeerChange {
        let key = node.destination.address_hash.to_hex_string();
        let mut nodes = self.inner.lock();
        let change = if nodes.contains_key(&key) {
            PeerChange::Refreshed
        } else {
            PeerChange::Added
        };
        nodes.insert(
            key,
            PropagationNodeRecord {
                node,
                hops,
                last_seen: now,
            },
        );
        while nodes.len() > PROPAGATION_NODE_TABLE_MAX_ENTRIES {
            let oldest = nodes
                .iter()
                .min_by_key(|(_, record)| record.last_seen)
                .map(|(key, _)| key.clone())
                .expect("a table over its cap is not empty");
            nodes.remove(&oldest);
            debug!("Evicted propagation node {}", short(&oldest));
        }
        change
    }

    /// The node a fetch should ask: the nearest by hops, the most recently heard breaking a
    /// tie. Neither the enabled flag nor the announced cost is consulted: slot `[2]` is
    /// `propagation_node and not from_static_only` (`LXMRouter.py:309`), which gates posting
    /// only, since `message_get_request` never reads it (`LXMRouter.py:1427-1429`), and a
    /// fetch mines no stamp. Both stay on the record for the posting picker.
    pub(crate) fn select(&self) -> Result<PropagationNode, FetchError> {
        self.inner
            .lock()
            .values()
            .min_by_key(|record| (record.hops, Reverse(record.last_seen)))
            .map(|record| record.node.clone())
            .ok_or(FetchError::NoPropagationNode)
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<PropagationNodeRecord> {
        self.inner.lock().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::propagation::pn_announce_app_data;
    use crate::testing::{debug_snapshot, install_log_collector};

    use rand_core::OsRng;
    use rns_transport::destination::{DestinationName, SingleOutputDestination};
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use std::time::Duration;

    fn desc(aspect: &str) -> DestinationDesc {
        SingleOutputDestination::new(
            *TransportIdentity::new_from_rand(OsRng).as_identity(),
            DestinationName::new("lxmf", aspect),
        )
        .desc
    }

    fn node(enabled: bool) -> PropagationNode {
        PropagationNode {
            destination: desc("propagation"),
            stamp_cost: 8,
            per_transfer_limit_kb: 256,
            propagation_enabled: enabled,
        }
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn hex(node: &PropagationNode) -> String {
        node.destination.address_hash.to_hex_string()
    }

    fn select_error(table: &PropagationNodeTable) -> FetchError {
        match table.select() {
            Ok(node) => panic!("expected no node, got {}", hex(&node)),
            Err(err) => err,
        }
    }

    #[test]
    fn empty_table_selects_nothing_with_the_teaching_error() {
        let table = PropagationNodeTable::new();
        let err = select_error(&table);
        assert_eq!(err, FetchError::NoPropagationNode);
        let text = err.to_string();
        assert!(
            text.contains("No LXMF propagation node has announced"),
            "{text}"
        );
        assert!(text.contains("interfaces"), "{text}");
    }

    #[test]
    fn select_prefers_fewest_hops_and_a_disabled_nearer_node_is_still_fetched() {
        let table = PropagationNodeTable::new();
        let far = node(true);
        let near = node(true);
        let nearest_but_disabled = node(false);
        table.observe(far.clone(), 3, t(10));
        table.observe(near.clone(), 2, t(5));
        assert_eq!(hex(&table.select().unwrap()), hex(&near));

        table.observe(nearest_but_disabled.clone(), 1, t(20));
        let selected = table.select().unwrap();
        assert_eq!(hex(&selected), hex(&nearest_but_disabled));
        assert!(!selected.propagation_enabled);
        let record = table
            .snapshot()
            .into_iter()
            .find(|record| hex(&record.node) == hex(&nearest_but_disabled))
            .unwrap();
        assert!(!record.node.propagation_enabled);
    }

    #[test]
    fn select_breaks_a_hop_tie_by_most_recent_announce() {
        let table = PropagationNodeTable::new();
        let stale = node(true);
        let fresh = node(true);
        table.observe(stale.clone(), 2, t(100));
        table.observe(fresh.clone(), 2, t(200));
        assert_eq!(hex(&table.select().unwrap()), hex(&fresh));

        table.observe(stale.clone(), 2, t(300));
        assert_eq!(hex(&table.select().unwrap()), hex(&stale));
    }

    #[test]
    fn observe_refreshes_in_place_and_reports_the_change() {
        let table = PropagationNodeTable::new();
        let first = node(true);
        assert_eq!(table.observe(first.clone(), 4, t(1)), PeerChange::Added);
        let mut updated = first.clone();
        updated.stamp_cost = 12;
        assert_eq!(table.observe(updated, 1, t(2)), PeerChange::Refreshed);

        let records = table.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].hops, 1);
        assert_eq!(records[0].last_seen, t(2));
        assert_eq!(records[0].node.stamp_cost, 12);
    }

    #[test]
    fn cap_evicts_the_least_recently_heard_and_logs_it() {
        install_log_collector();
        let table = PropagationNodeTable::new();
        let mut nodes = Vec::new();
        for i in 0..PROPAGATION_NODE_TABLE_MAX_ENTRIES {
            let candidate = node(true);
            table.observe(candidate.clone(), 1, t(1_000 + i as u64));
            nodes.push(candidate);
        }
        // The oldest is refreshed so eviction has to follow last_seen, not insertion order.
        table.observe(nodes[0].clone(), 1, t(5_000));

        let newcomer = node(true);
        table.observe(newcomer.clone(), 1, t(5_001));

        let kept: Vec<String> = table.snapshot().iter().map(|r| hex(&r.node)).collect();
        assert_eq!(kept.len(), PROPAGATION_NODE_TABLE_MAX_ENTRIES);
        assert!(kept.contains(&hex(&nodes[0])));
        assert!(kept.contains(&hex(&newcomer)));
        assert!(!kept.contains(&hex(&nodes[1])));
        let evicted = format!("Evicted propagation node {}", short(&hex(&nodes[1])));
        assert!(
            debug_snapshot().iter().any(|line| line == &evicted),
            "no debug line {evicted:?}"
        );
    }

    #[test]
    fn observe_announce_files_a_propagation_announce_and_passes_others_through() {
        install_log_collector();
        let table = PropagationNodeTable::new();
        let pn = desc("propagation");
        let coyote = SingleOutputDestination::new(
            *TransportIdentity::new_from_rand(OsRng).as_identity(),
            DestinationName::new("coyote", "mesh.abc"),
        )
        .desc;

        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, t(1)));
        assert!(!table.observe_announce(&coyote, b"not a pn", 1, t(1)));
        assert_eq!(table.snapshot().len(), 1);
        assert_eq!(table.select().unwrap().stamp_cost, 8);

        // A cost this node would never mine is still filed: it bounds posting, not fetching.
        let dear = desc("propagation");
        assert!(table.observe_announce(&dear, &pn_announce_app_data(true, 30, 256), 3, t(1)));
        let filed = table
            .snapshot()
            .into_iter()
            .find(|record| hex(&record.node) == dear.address_hash.to_hex_string())
            .unwrap();
        assert_eq!(filed.node.stamp_cost, 30);
        assert_eq!(table.snapshot().len(), 2);

        // A propagation announce that does not parse is handled (dropped), not passed on.
        let broken = desc("propagation");
        assert!(table.observe_announce(&broken, b"\x90", 1, t(2)));
        assert_eq!(table.snapshot().len(), 2);
        let ignored = format!(
            "Ignored propagation node announce from {} (1 hops)",
            short(&broken.address_hash.to_hex_string())
        );
        assert!(
            debug_snapshot()
                .iter()
                .any(|line| line.starts_with(&ignored)),
            "no debug line starting {ignored:?}"
        );
    }
}
