//! The ctx-free half of the idle-time driver: what a producer hands the driver, the sink
//! it hands it through, and the flood control that sits above the prompt printer's
//! bounded queue. The driver loop itself lives under `src/repl/`, where it may touch the
//! session context; nothing here does.

use crate::mesh::notify::{Notification, Source};
use crate::mesh::propagation_fetch::{FetchError, FetchReport};
use crate::mesh::r3::{R3Error, redact_hashes, short};
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
/// How soon an automatic propagation fetch is tried again when no propagation node has
/// announced itself yet, when that is sooner than the configured interval.
pub(crate) const PROPAGATION_SYNC_RETRY_WITHOUT_NODE: Duration = Duration::from_secs(15);

/// Who produced an event, which decides whether flood control applies. `Local` events
/// are minted by this crate (child completions, node lifecycle) and are bounded by what
/// the driver itself runs, so they are never rate-limited or folded. `Peer` events carry
/// the peer's 8-hex short hash and go through the bucket and the coalescer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Local,
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

/// Where producers push. `push` returns the note when it could not be queued; the driver
/// counts overflow itself, so the caller decides what, if anything, to do with it.
/// `request_sync` asks for a propagation fetch at the driver's next chance and is never
/// lost: a request that cannot be queued is left as a flag the driver reads whenever it
/// next wakes.
pub(crate) trait IdleSink: Send + Sync {
    fn push(&self, note: IdleNotify) -> Result<(), IdleNotify>;
    fn request_sync(&self);
}

/// Per-source token bucket. The clock is a parameter so admission is a pure function of
/// the calls made so far and the instants passed in. The driver consults it only for
/// sources that are not exempt from folding.
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

/// When the automatic propagation fetch runs next and what its outcome is worth telling
/// the user. The clock is a parameter, as for `RateLimiter`; the driver owns the fetch
/// task and the slot, and consults this for every decision about them. One fetch at a
/// time: a request made while one runs is replayed when it ends, and otherwise the
/// interval re-arms from the moment the fetch ends rather than from when it was asked
/// for. A wait that does not fit the clock (`Instant::checked_add` fails) is never
/// armed, so an absurd interval disables the timer rather than panicking the driver.
pub(crate) struct SyncSchedule {
    interval: Option<Duration>,
    due_at: Option<Instant>,
    in_flight: Option<String>,
    pending: bool,
    last_failure: Option<String>,
}

impl SyncSchedule {
    /// `interval_secs` of 0, or a node that does not announce, means fetch only on
    /// `.mesh fetch`: no request is accepted and no timer is armed. A fetch identifies
    /// this node to the propagation node, which an `announce: false` operator chose not
    /// to have happen on its own.
    pub(crate) fn new(interval_secs: u64, announce: bool) -> Self {
        Self {
            interval: (announce && interval_secs > 0).then(|| Duration::from_secs(interval_secs)),
            due_at: None,
            in_flight: None,
            pending: false,
            last_failure: None,
        }
    }

    pub(crate) fn due_at(&self) -> Option<Instant> {
        self.due_at
    }

    /// Whether a request, from the node joining or from the timer, may start a fetch now.
    /// The timer is disarmed either way: an accepted request ends in `started`, `no_node`
    /// or `mesh_off`, which each decide the next, and one refused because a fetch is
    /// running is owed a re-run when that fetch ends.
    pub(crate) fn accepts_request(&mut self) -> bool {
        self.due_at = None;
        if self.interval.is_none() {
            return false;
        }
        if self.in_flight.is_some() {
            self.pending = true;
            return false;
        }
        true
    }

    /// The slot is empty: nothing to fetch from and nothing to wait for, since the next
    /// install asks again.
    pub(crate) fn mesh_off(&mut self) {
        self.due_at = None;
    }

    /// No propagation node has announced itself yet: ask again after the shorter of the
    /// interval and `PROPAGATION_SYNC_RETRY_WITHOUT_NODE`.
    pub(crate) fn no_node(&mut self, now: Instant) {
        self.due_at = self.interval.and_then(|interval| {
            now.checked_add(interval.min(PROPAGATION_SYNC_RETRY_WITHOUT_NODE))
        });
    }

    /// Automatic, with no timer armed and no fetch running, while a node is installed:
    /// the state a join-time request that never reached the driver leaves behind. Arms
    /// the no-node retry so the schedule recovers on its own.
    pub(crate) fn heal(&mut self, now: Instant) {
        if self.interval.is_some() && self.due_at.is_none() && self.in_flight.is_none() {
            self.due_at = now.checked_add(PROPAGATION_SYNC_RETRY_WITHOUT_NODE);
        }
    }

    /// A fetch from `node` (lower-hex destination hash) is running.
    pub(crate) fn started(&mut self, node: String) {
        self.in_flight = Some(node);
        self.due_at = None;
    }

    /// The fetch ended. Re-arms the interval and returns the one line the user gets, if
    /// any: a summary when something arrived, or the failure text unless it repeats the
    /// failure already shown, so a node that stays dead is reported once. Refusals from
    /// a fetch already running elsewhere, cancellation and a stopped node say nothing;
    /// a fetch that found no node yet retries as `no_node` does. A request refused while
    /// this fetch ran is due at once instead.
    pub(crate) fn finished(
        &mut self,
        outcome: &Result<FetchReport, FetchError>,
        now: Instant,
    ) -> Option<String> {
        let node = self.in_flight.take().unwrap_or_default();
        let line = match outcome {
            Ok(report) => {
                self.re_arm(now);
                self.last_failure = None;
                (report.received > 0).then(|| synced_line(report))
            }
            Err(FetchError::NoPropagationNode) => {
                self.no_node(now);
                None
            }
            Err(
                FetchError::AlreadyRunning
                | FetchError::HeldByOtherProcess { .. }
                | FetchError::Cancelled
                | FetchError::Link(R3Error::NotRunning | R3Error::Shutdown),
            ) => {
                self.re_arm(now);
                None
            }
            Err(err) => {
                self.re_arm(now);
                let text = format!(
                    "sync from {} failed: {}",
                    short(&node),
                    redact_hashes(&err.to_string())
                );
                if self.last_failure.as_deref() == Some(text.as_str()) {
                    None
                } else {
                    self.last_failure = Some(text.clone());
                    Some(text)
                }
            }
        };
        if std::mem::take(&mut self.pending) {
            self.due_at = Some(now);
        }
        line
    }

    fn re_arm(&mut self, now: Instant) {
        self.due_at = self.interval.and_then(|interval| now.checked_add(interval));
    }
}

/// `synced 3 messages from deadbeef: 2 delivered, 1 duplicate`, naming only the non-zero
/// outcomes.
fn synced_line(report: &FetchReport) -> String {
    let outcomes: Vec<String> = [
        (report.delivered, "delivered", "delivered"),
        (report.duplicates, "duplicate", "duplicates"),
        (report.discarded, "discarded", "discarded"),
        (report.deferred, "deferred", "deferred"),
    ]
    .into_iter()
    .filter(|(count, _, _)| *count > 0)
    .map(|(count, one, many)| plural(count, one, many))
    .collect();
    let mut line = format!(
        "synced {} from {}",
        plural(report.received, "message", "messages"),
        short(&report.node)
    );
    if !outcomes.is_empty() {
        line.push_str(": ");
        line.push_str(&outcomes.join(", "));
    }
    line
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
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
            assert!(limiter.admit(Source::Message, now));
        }
        assert!(!limiter.admit(Source::Message, now));
        let later = now + IDLE_NOTIFY_REFILL_INTERVAL;
        assert!(limiter.admit(Source::Message, later));
        assert!(!limiter.admit(Source::Message, later));
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

    const NODE: &str = "deadbeefdeadbeefdeadbeefdeadbeef";

    fn report(received: usize, delivered: usize, duplicates: usize) -> FetchReport {
        FetchReport {
            node: NODE.to_string(),
            listed: received,
            wanted: received,
            received,
            delivered,
            duplicates,
            discarded: 0,
            deferred: 0,
            acknowledged: received,
            response_branch: None,
        }
    }

    fn link_failure(text: &str) -> Result<FetchReport, FetchError> {
        Err(FetchError::Link(R3Error::LinkFailed(text.to_string())))
    }

    #[test]
    fn sync_schedule_re_arms_the_interval_from_the_end_of_a_fetch() {
        let mut schedule = SyncSchedule::new(300, true);
        let now = Instant::now();
        assert_eq!(schedule.due_at(), None);
        assert!(schedule.accepts_request());
        schedule.started(NODE.to_string());
        let later = now + Duration::from_secs(40);
        assert_eq!(schedule.finished(&Ok(report(0, 0, 0)), later), None);
        assert_eq!(schedule.due_at(), Some(later + Duration::from_secs(300)));
        assert!(schedule.accepts_request());
        assert_eq!(
            schedule.due_at(),
            None,
            "accepting a request takes the timer"
        );
    }

    #[test]
    fn a_request_refused_while_a_fetch_runs_is_replayed_when_it_ends() {
        let now = Instant::now();
        for outcome in [
            Ok(report(0, 0, 0)),
            Err(FetchError::Cancelled),
            Err(FetchError::NoPropagationNode),
            link_failure("the relay hung up"),
        ] {
            let mut schedule = SyncSchedule::new(300, true);
            assert!(schedule.accepts_request());
            schedule.started(NODE.to_string());
            assert!(
                !schedule.accepts_request(),
                "one fetch at a time: {outcome:?}"
            );
            schedule.finished(&outcome, now);
            assert_eq!(
                schedule.due_at(),
                Some(now),
                "the refused request is due as soon as the fetch ends: {outcome:?}"
            );
            assert!(schedule.accepts_request());
            schedule.started(NODE.to_string());
            schedule.finished(&Ok(report(0, 0, 0)), now);
            assert_eq!(
                schedule.due_at(),
                Some(now + Duration::from_secs(300)),
                "a replay is owed once: {outcome:?}"
            );
        }
    }

    #[test]
    fn sync_schedule_with_a_zero_interval_never_accepts_and_never_arms() {
        let mut schedule = SyncSchedule::new(0, true);
        let now = Instant::now();
        assert!(!schedule.accepts_request());
        schedule.no_node(now);
        assert_eq!(schedule.due_at(), None);
        schedule.heal(now);
        assert_eq!(schedule.due_at(), None);
        assert!(!schedule.accepts_request());
    }

    #[test]
    fn sync_schedule_is_manual_only_when_the_node_does_not_announce() {
        let mut schedule = SyncSchedule::new(300, false);
        let now = Instant::now();
        assert!(!schedule.accepts_request());
        schedule.no_node(now);
        assert_eq!(schedule.due_at(), None);
        schedule.heal(now);
        assert_eq!(schedule.due_at(), None);
        assert!(!schedule.accepts_request());
    }

    #[test]
    fn an_interval_too_large_to_schedule_never_arms_and_never_panics() {
        let mut schedule = SyncSchedule::new(u64::MAX, true);
        let now = Instant::now();
        assert!(schedule.accepts_request());
        schedule.started(NODE.to_string());
        assert_eq!(schedule.finished(&Ok(report(0, 0, 0)), now), None);
        assert_eq!(schedule.due_at(), None);
        schedule.started(NODE.to_string());
        assert!(
            schedule
                .finished(&link_failure("the relay hung up"), now)
                .is_some()
        );
        assert_eq!(schedule.due_at(), None);
        schedule.no_node(now);
        assert_eq!(
            schedule.due_at(),
            Some(now + PROPAGATION_SYNC_RETRY_WITHOUT_NODE),
            "the no-node retry still fits the clock"
        );
    }

    #[test]
    fn sync_schedule_heals_an_automatic_schedule_left_unarmed() {
        let now = Instant::now();
        let mut schedule = SyncSchedule::new(300, true);
        schedule.heal(now);
        assert_eq!(
            schedule.due_at(),
            Some(now + PROPAGATION_SYNC_RETRY_WITHOUT_NODE)
        );
        let later = now + Duration::from_secs(5);
        schedule.heal(later);
        assert_eq!(
            schedule.due_at(),
            Some(now + PROPAGATION_SYNC_RETRY_WITHOUT_NODE),
            "an armed timer is left alone"
        );
        assert!(schedule.accepts_request());
        schedule.started(NODE.to_string());
        schedule.heal(later);
        assert_eq!(schedule.due_at(), None, "a running fetch is left alone");
    }

    #[test]
    fn sync_schedule_retries_sooner_while_no_node_has_announced() {
        let now = Instant::now();
        let mut schedule = SyncSchedule::new(300, true);
        schedule.no_node(now);
        assert_eq!(
            schedule.due_at(),
            Some(now + PROPAGATION_SYNC_RETRY_WITHOUT_NODE)
        );
        let mut short_interval = SyncSchedule::new(5, true);
        short_interval.no_node(now);
        assert_eq!(
            short_interval.due_at(),
            Some(now + Duration::from_secs(5)),
            "the interval wins when it is the shorter wait"
        );
        let mut raced = SyncSchedule::new(300, true);
        raced.started(NODE.to_string());
        assert_eq!(
            raced.finished(&Err(FetchError::NoPropagationNode), now),
            None
        );
        assert_eq!(
            raced.due_at(),
            Some(now + PROPAGATION_SYNC_RETRY_WITHOUT_NODE),
            "a node that vanished between the check and the fetch is waited for the same way"
        );
    }

    #[test]
    fn sync_schedule_stops_when_the_mesh_is_off() {
        let now = Instant::now();
        let mut schedule = SyncSchedule::new(300, true);
        schedule.no_node(now);
        assert!(schedule.due_at().is_some());
        schedule.mesh_off();
        assert_eq!(schedule.due_at(), None);
    }

    #[test]
    fn sync_schedule_reports_what_arrived_and_stays_quiet_otherwise() {
        let now = Instant::now();
        let mut schedule = SyncSchedule::new(300, true);
        schedule.started(NODE.to_string());
        assert_eq!(
            schedule.finished(&Ok(report(3, 2, 1)), now),
            Some("synced 3 messages from deadbeef: 2 delivered, 1 duplicate".to_string())
        );
        schedule.started(NODE.to_string());
        assert_eq!(
            schedule.finished(&Ok(report(1, 1, 0)), now),
            Some("synced 1 message from deadbeef: 1 delivered".to_string())
        );
        schedule.started(NODE.to_string());
        assert_eq!(schedule.finished(&Ok(report(0, 0, 0)), now), None);
    }

    #[test]
    fn sync_schedule_says_a_failure_once_until_it_changes_or_a_fetch_succeeds() {
        let now = Instant::now();
        let mut schedule = SyncSchedule::new(300, true);
        let hash = "ab".repeat(16);
        let failure = format!("no known path to destination {hash}");
        schedule.started(NODE.to_string());
        let line = schedule.finished(&link_failure(&failure), now).unwrap();
        assert_eq!(
            line,
            "sync from deadbeef failed: The mesh link failed: no known path to destination abababab"
        );
        assert_eq!(schedule.due_at(), Some(now + Duration::from_secs(300)));

        schedule.started(NODE.to_string());
        assert_eq!(
            schedule.finished(&link_failure(&failure), now),
            None,
            "the same failure again is not repeated"
        );
        schedule.started(NODE.to_string());
        assert!(
            schedule
                .finished(&link_failure("the relay hung up"), now)
                .is_some(),
            "a different failure is said"
        );
        schedule.started(NODE.to_string());
        assert_eq!(schedule.finished(&Ok(report(0, 0, 0)), now), None);
        schedule.started(NODE.to_string());
        assert!(
            schedule
                .finished(&link_failure("the relay hung up"), now)
                .is_some(),
            "a success in between makes the failure news again"
        );
    }

    #[test]
    fn sync_schedule_is_silent_about_a_fetch_running_elsewhere_or_a_stopped_node() {
        let now = Instant::now();
        for err in [
            FetchError::AlreadyRunning,
            FetchError::HeldByOtherProcess {
                path: std::path::PathBuf::from("/x/propagation.json"),
                pid: Some(7),
            },
            FetchError::Cancelled,
            FetchError::Link(R3Error::NotRunning),
            FetchError::Link(R3Error::Shutdown),
        ] {
            let mut schedule = SyncSchedule::new(300, true);
            schedule.started(NODE.to_string());
            assert_eq!(schedule.finished(&Err(err.clone()), now), None, "{err:?}");
            assert_eq!(
                schedule.due_at(),
                Some(now + Duration::from_secs(300)),
                "{err:?}"
            );
            assert!(schedule.accepts_request(), "{err:?}");
        }
    }
}
