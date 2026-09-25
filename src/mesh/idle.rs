//! The ctx-free half of the idle-time driver: what a producer hands the driver, the sink
//! it hands it through, and the flood control that sits above the prompt printer's
//! bounded queue. The driver loop itself lives under `src/repl/`, where it may touch the
//! session context; nothing here does.

use crate::mesh::notify::{Notification, Source};
use crate::supervisor::notification::SystemNotification;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Depth of the queue between producers and the driver loop. Overflow is counted and the
/// note handed back to the producer, never waited on.
pub(crate) const IDLE_QUEUE_CAPACITY: usize = 256;
/// Token bucket capacity per `Source`: how many lines a source may put on the prompt
/// back to back before the rest are folded.
pub(crate) const IDLE_NOTIFY_BURST: u32 = 5;
/// One token is returned to each bucket per interval.
pub(crate) const IDLE_NOTIFY_REFILL_INTERVAL: Duration = Duration::from_secs(1);
/// Folded overflow is summarised as one line per peer at the next tick.
pub(crate) const IDLE_COALESCE_TICK: Duration = Duration::from_secs(1);
/// Distinct peers a tick's summary names; the rest share one closing line, so a flood
/// that rotates peer ids still gets a bounded number of summary lines.
pub(crate) const IDLE_COALESCE_MAX_PEERS: usize = 8;
/// Distinct ids remembered past the named cap within one tick, peers and agent names
/// alike. Past this many the closing line says the cap followed by `+` rather than
/// growing a set keyed by whatever a flood chooses to send.
pub(crate) const IDLE_COALESCE_MAX_OTHER_PEERS: usize = 64;

/// Who produced an event, which decides whether flood control applies. `Local` events
/// are minted by this crate (child completions, node lifecycle) and are bounded by what
/// the driver itself runs, so they are never rate-limited or folded. `Peer` events carry
/// the peer's 8-hex short hash and go through the bucket and the coalescer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Local,
    // Constructed by the mesh request handlers once they land.
    #[allow(dead_code)]
    Peer(String),
}

/// One event for the driver. `text` is the human line; `model_note`, when present, is the
/// same event for the model's transcript. The model note is boxed so a note handed back
/// on overflow stays small.
#[derive(Debug)]
pub(crate) struct IdleNotify {
    pub(crate) source: Source,
    pub(crate) text: String,
    pub(crate) origin: Origin,
    pub(crate) model_note: Option<Box<SystemNotification>>,
}

/// Where producers push. Returns the note when it could not be queued; the driver
/// counts overflow itself, so the caller decides what, if anything, to do with it.
pub(crate) trait IdleSink: Send + Sync {
    fn push(&self, note: IdleNotify) -> Result<(), IdleNotify>;
}

/// Per-source token bucket. The clock is a parameter so admission is a pure function of
/// the calls made so far and the instants passed in.
pub(crate) struct RateLimiter {
    buckets: [Bucket; Source::ALL.len()],
}

struct Bucket {
    source: Source,
    tokens: u32,
    last_refill: Option<Instant>,
}

impl RateLimiter {
    pub(crate) fn new() -> Self {
        Self {
            buckets: Source::ALL.map(|source| Bucket {
                source,
                tokens: IDLE_NOTIFY_BURST,
                last_refill: None,
            }),
        }
    }

    /// Spends one token from `source`'s bucket if it has one. Tokens accrue one per
    /// `IDLE_NOTIFY_REFILL_INTERVAL` since the bucket was last credited, capped at
    /// `IDLE_NOTIFY_BURST`.
    pub(crate) fn admit(&mut self, source: Source, now: Instant) -> bool {
        let bucket = self
            .buckets
            .iter_mut()
            .find(|bucket| bucket.source == source)
            .expect("every Source has a bucket");
        match bucket.last_refill {
            None => bucket.last_refill = Some(now),
            Some(last) => {
                let elapsed = now.saturating_duration_since(last);
                let intervals = elapsed.as_nanos() / IDLE_NOTIFY_REFILL_INTERVAL.as_nanos();
                if intervals > 0 {
                    let intervals = u32::try_from(intervals).unwrap_or(u32::MAX);
                    bucket.tokens = bucket
                        .tokens
                        .saturating_add(intervals)
                        .min(IDLE_NOTIFY_BURST);
                    // Credit whole intervals only, so the remainder of a partial one
                    // still counts towards the next token.
                    bucket.last_refill = last
                        .checked_add(IDLE_NOTIFY_REFILL_INTERVAL.saturating_mul(intervals))
                        .or(Some(now));
                }
            }
        }
        if bucket.tokens == 0 {
            return false;
        }
        bucket.tokens -= 1;
        true
    }
}

/// The ids a tick's summary does not name. Holds at most `IDLE_COALESCE_MAX_OTHER_PEERS`
/// of them; once full it only remembers that more arrived.
#[derive(Default)]
pub(crate) struct OtherNames {
    names: BTreeSet<String>,
    saturated: bool,
}

impl OtherNames {
    pub(crate) fn insert(&mut self, name: &str) {
        if self.names.contains(name) {
            return;
        }
        if self.names.len() >= IDLE_COALESCE_MAX_OTHER_PEERS {
            self.saturated = true;
            return;
        }
        self.names.insert(name.to_string());
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn is_saturated(&self) -> bool {
        self.saturated
    }

    /// `K` while every id fit, the cap followed by `+` once one did not.
    pub(crate) fn count_label(&self) -> String {
        if self.saturated {
            format!("{IDLE_COALESCE_MAX_OTHER_PEERS}+")
        } else {
            self.names.len().to_string()
        }
    }
}

/// Counts what the rate limiter turned away, per peer, until the next tick summarises
/// it. The summary lines bypass the bucket: they are the one line a flood is allowed.
/// Only the first `IDLE_COALESCE_MAX_PEERS` peers of a tick get a line of their own;
/// later peers are counted together and named by number.
#[derive(Default)]
pub(crate) struct Coalescer {
    folded: BTreeMap<String, usize>,
    other_peers: OtherNames,
    other_events: usize,
}

impl Coalescer {
    pub(crate) fn fold(&mut self, peer: &str) {
        if let Some(count) = self.folded.get_mut(peer) {
            *count += 1;
        } else if self.folded.len() < IDLE_COALESCE_MAX_PEERS {
            self.folded.insert(peer.to_string(), 1);
        } else {
            self.other_peers.insert(peer);
            self.other_events += 1;
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.folded.is_empty() && self.other_events == 0
    }

    /// One line per named peer, then one for the rest if there were any, then empty.
    pub(crate) fn flush(&mut self) -> Vec<Notification> {
        let mut lines: Vec<Notification> = std::mem::take(&mut self.folded)
            .into_iter()
            .map(|(peer, count)| {
                Notification::new(Source::Mesh, format!("({count} more from {peer})"))
            })
            .collect();
        let other_peers = std::mem::take(&mut self.other_peers).count_label();
        let other_events = std::mem::take(&mut self.other_events);
        if other_events > 0 {
            lines.push(Notification::new(
                Source::Mesh,
                format!("({other_events} more from {other_peers} other peers)"),
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_admits_exactly_the_bucket_capacity() {
        let mut limiter = RateLimiter::new();
        let now = Instant::now();
        let admitted = (0..100)
            .filter(|_| limiter.admit(Source::Message, now))
            .count();
        assert_eq!(admitted, IDLE_NOTIFY_BURST as usize);
    }

    #[test]
    fn one_refill_interval_returns_exactly_one_token() {
        let mut limiter = RateLimiter::new();
        let now = Instant::now();
        for _ in 0..IDLE_NOTIFY_BURST {
            assert!(limiter.admit(Source::Knock, now));
        }
        assert!(!limiter.admit(Source::Knock, now));
        let later = now + IDLE_NOTIFY_REFILL_INTERVAL;
        assert!(limiter.admit(Source::Knock, later));
        assert!(!limiter.admit(Source::Knock, later));
    }

    /// The half interval left over after the first credit still counts towards the
    /// next token instead of being forgotten when the bucket is credited.
    #[test]
    fn a_partial_interval_carries_its_remainder_into_the_next_refill() {
        let mut limiter = RateLimiter::new();
        let now = Instant::now();
        for _ in 0..IDLE_NOTIFY_BURST {
            assert!(limiter.admit(Source::Message, now));
        }
        assert!(!limiter.admit(Source::Message, now));

        let one_and_a_half = now + IDLE_NOTIFY_REFILL_INTERVAL * 3 / 2;
        assert!(limiter.admit(Source::Message, one_and_a_half));
        assert!(!limiter.admit(Source::Message, one_and_a_half));

        let two = now + IDLE_NOTIFY_REFILL_INTERVAL * 2;
        assert!(limiter.admit(Source::Message, two));
        assert!(!limiter.admit(Source::Message, two));
    }

    #[test]
    fn a_long_gap_never_fills_the_bucket_past_its_capacity() {
        let mut limiter = RateLimiter::new();
        let now = Instant::now();
        assert!(limiter.admit(Source::Mesh, now));
        let much_later = now + IDLE_NOTIFY_REFILL_INTERVAL * 1_000;
        let admitted = (0..100)
            .filter(|_| limiter.admit(Source::Mesh, much_later))
            .count();
        assert_eq!(admitted, IDLE_NOTIFY_BURST as usize);
    }

    #[test]
    fn buckets_are_independent_per_source() {
        let mut limiter = RateLimiter::new();
        let now = Instant::now();
        for _ in 0..IDLE_NOTIFY_BURST {
            assert!(limiter.admit(Source::Message, now));
        }
        assert!(!limiter.admit(Source::Message, now));
        assert!(limiter.admit(Source::Propagation, now));
    }

    #[test]
    fn coalescer_flushes_one_summary_line_per_peer_and_is_then_empty() {
        let mut coalescer = Coalescer::default();
        assert!(coalescer.is_empty());
        for _ in 0..95 {
            coalescer.fold("deadbeef");
        }
        assert!(!coalescer.is_empty());
        let lines: Vec<Vec<String>> = coalescer
            .flush()
            .into_iter()
            .map(|note| note.render_lines())
            .collect();
        assert_eq!(lines, vec![vec!["[mesh] (95 more from deadbeef)"]]);
        assert!(coalescer.is_empty());
        assert!(coalescer.flush().is_empty());
    }

    #[test]
    fn coalescer_keeps_peers_apart() {
        let mut coalescer = Coalescer::default();
        coalescer.fold("deadbeef");
        coalescer.fold("cafef00d");
        coalescer.fold("cafef00d");
        let lines: Vec<String> = coalescer
            .flush()
            .into_iter()
            .flat_map(|note| note.render_lines())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.contains(&"[mesh] (1 more from deadbeef)".to_string()));
        assert!(lines.contains(&"[mesh] (2 more from cafef00d)".to_string()));
    }

    #[test]
    fn coalescer_names_a_bounded_number_of_peers_and_counts_the_rest_together() {
        const PEERS: usize = 50;
        const EVENTS: usize = 100;
        let mut coalescer = Coalescer::default();
        for i in 0..EVENTS {
            coalescer.fold(&format!("{:08x}", i % PEERS));
        }

        let lines: Vec<String> = coalescer
            .flush()
            .into_iter()
            .flat_map(|note| note.render_lines())
            .collect();

        assert_eq!(lines.len(), IDLE_COALESCE_MAX_PEERS + 1);
        let events_per_peer = EVENTS / PEERS;
        for (i, line) in lines[..IDLE_COALESCE_MAX_PEERS].iter().enumerate() {
            assert_eq!(
                line,
                &format!("[mesh] ({events_per_peer} more from {i:08x})")
            );
        }
        let other_peers = PEERS - IDLE_COALESCE_MAX_PEERS;
        let other_events = other_peers * events_per_peer;
        assert!(other_peers <= IDLE_COALESCE_MAX_OTHER_PEERS);
        assert_eq!(
            lines[IDLE_COALESCE_MAX_PEERS],
            format!("[mesh] ({other_events} more from {other_peers} other peers)")
        );
        assert!(coalescer.is_empty());
    }

    /// A flood that mints a fresh peer id per event must not grow a set with it: the
    /// unnamed peers are remembered up to the cap and counted past it.
    #[test]
    fn coalescer_stops_remembering_other_peers_at_the_cap_and_says_so() {
        const PEERS: usize = 10_000;
        let mut coalescer = Coalescer::default();
        for i in 0..PEERS {
            coalescer.fold(&format!("{i:08x}"));
            assert!(coalescer.other_peers.len() <= IDLE_COALESCE_MAX_OTHER_PEERS);
        }
        assert!(coalescer.other_peers.is_saturated());

        let lines: Vec<String> = coalescer
            .flush()
            .into_iter()
            .flat_map(|note| note.render_lines())
            .collect();

        assert_eq!(lines.len(), IDLE_COALESCE_MAX_PEERS + 1);
        let other_events = PEERS - IDLE_COALESCE_MAX_PEERS;
        assert_eq!(
            lines[IDLE_COALESCE_MAX_PEERS],
            format!(
                "[mesh] ({other_events} more from {IDLE_COALESCE_MAX_OTHER_PEERS}+ other peers)"
            )
        );
        assert!(coalescer.is_empty());
        assert!(
            !coalescer.other_peers.is_saturated(),
            "the flush resets the saturation with the set"
        );
    }
}
