//! When this node asks a trusted session for its status card on its own, so the names a
//! card advertises stay current without anyone running `.mesh people status`.

use crate::mesh::card::{STATUS_MAX_CONCURRENCY, StatusError};
use crate::mesh::message::Route;
use crate::mesh::node::MeshRuntime;
use crate::mesh::request::{RequestOptions, short};
use crate::mesh::trust::{Decision, TrustGranted, TrustObserver, TrustRevoked};

use log::debug;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The shortest gap between two background card fetches to one destination.
pub(crate) const CARD_FETCH_MIN_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// What prompted a background card fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardTrigger {
    /// The trust store recorded a grant that covers the session, the owner's included;
    /// asked only when the session served this node.
    Granted,
    /// The owner granted it trust with `.mesh access trust`: asked once whether or not
    /// it ever served this node, since the owner has just chosen to trust it and the card
    /// is what names it. It comes alongside the store's `Granted`; the interval keeps the
    /// two to one fetch.
    OwnerGranted,
    /// The session announced or answered a path request; only the first time a session
    /// is heard in a run counts, and a card or message it served counts as heard.
    Heard,
}

/// The bookkeeping behind the background card fetch, in memory for one run.
pub(crate) struct CardCadence {
    state: Mutex<CadenceState>,
    /// The node's one `STATUS_MAX_CONCURRENCY` cap, shared by the background fetches and
    /// a `mesh__peers` listing with its cards.
    permits: Arc<Semaphore>,
    /// Added to the clock, so a test can let hours pass at once.
    #[cfg(test)]
    skew: Mutex<Duration>,
}

#[derive(Default)]
struct CadenceState {
    /// Destinations heard in this run: by an announce or a path response, or by a card or
    /// a message they served this node.
    heard: HashSet<String>,
    /// When each destination was last asked in the background.
    last_asked: HashMap<String, Instant>,
    /// Destinations whose last answer was a refusal; not asked in the background again
    /// until the trust on either side changes or the owner asks by hand. Kept for the
    /// run only: a restart asks again once, at most, before a refusal stops it anew.
    refused: HashSet<String>,
}

impl Default for CardCadence {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            permits: Arc::new(Semaphore::new(STATUS_MAX_CONCURRENCY)),
            #[cfg(test)]
            skew: Mutex::default(),
        }
    }
}

impl CardCadence {
    /// The permits every `/status` request this node makes on its own or for a listing
    /// takes one of, so together they never exceed `STATUS_MAX_CONCURRENCY`.
    pub(crate) fn status_permits(&self) -> Arc<Semaphore> {
        self.permits.clone()
    }

    fn now(&self) -> Instant {
        Instant::now() + self.skew()
    }

    #[cfg(test)]
    fn skew(&self) -> Duration {
        *self.skew.lock()
    }

    #[cfg(not(test))]
    fn skew(&self) -> Duration {
        Duration::ZERO
    }

    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        *self.skew.lock() += by;
    }

    /// Whether a background fetch of `destination`, prompted by `trigger`, may go out
    /// now, and if so notes it as asked. `served` is whether the session served this
    /// node a card or a message in this run or the previous one; a session that did not
    /// is asked only after the owner granted it trust, and one that refused since the
    /// trust last changed is never asked. A grant is a trust change on this side, so it
    /// lifts a refusal first. Only the first time a session is heard in a run counts, and
    /// no destination is asked twice within `CARD_FETCH_MIN_INTERVAL`.
    pub(crate) fn admit(&self, destination: &str, trigger: CardTrigger, served: bool) -> bool {
        let now = self.now();
        let mut state = self.state.lock();
        match trigger {
            CardTrigger::Heard => {
                if !state.heard.insert(destination.to_string()) {
                    return false;
                }
            }
            CardTrigger::Granted | CardTrigger::OwnerGranted => {
                state.refused.remove(destination);
            }
        }
        if (!served && trigger != CardTrigger::OwnerGranted) || state.refused.contains(destination)
        {
            return false;
        }
        if state
            .last_asked
            .get(destination)
            .is_some_and(|last| now.saturating_duration_since(*last) < CARD_FETCH_MIN_INTERVAL)
        {
            return false;
        }
        state.last_asked.insert(destination.to_string(), now);
        true
    }

    /// The session refused a status request: no background fetch asks it again until
    /// `lift`.
    pub(crate) fn refused(&self, destination: &str) {
        self.state.lock().refused.insert(destination.to_string());
    }

    /// The trust on the session's side may have changed (it sent this node a message,
    /// which it sends only to a session it trusts, though a held message may predate a
    /// later revoke) or the owner asked for its card by hand.
    pub(crate) fn lift(&self, destination: &str) {
        self.state.lock().refused.remove(destination);
    }

    /// The session served this node a card or a message: it has been heard in this run,
    /// so its announces prompt no fetch of their own.
    pub(crate) fn note_heard(&self, destination: &str) {
        self.state.lock().heard.insert(destination.to_string());
    }

    /// An admitted fetch sent nothing (no path or no route was known), so the interval
    /// it took is given back: the next trigger may ask.
    pub(crate) fn unasked(&self, destination: &str) {
        self.state.lock().last_asked.remove(destination);
    }

    #[cfg(test)]
    pub(crate) fn is_refused(&self, destination: &str) -> bool {
        self.state.lock().refused.contains(destination)
    }

    /// Forgets that `destination` was heard, as a new run starts out.
    #[cfg(test)]
    pub(crate) fn forget_heard(&self, destination: &str) {
        self.state.lock().heard.remove(destination);
    }
}

/// The trust store's observer with the background card fetch in front: every grant is
/// queued for `drive_card_cadence`, then handed on. The queue is unbounded because the
/// store calls observers with its lock held, where nothing may wait.
pub(crate) struct CadenceTrustObserver {
    pub(crate) inner: Arc<dyn TrustObserver>,
    pub(crate) grants: mpsc::UnboundedSender<TrustGranted>,
}

impl TrustObserver for CadenceTrustObserver {
    fn granted(&self, event: TrustGranted) {
        // A closed queue means the node has stopped; nothing is fetched any more.
        let _ = self.grants.send(event.clone());
        self.inner.granted(event);
    }

    fn revoked(&self, event: TrustRevoked) {
        self.inner.revoked(event);
    }
}

/// Turns each grant into a background card fetch of every session it covers that the
/// peer table holds: the one destination of a destination grant, every session of the
/// identity for an identity grant.
pub(crate) async fn drive_card_cadence(
    mut grants: mpsc::UnboundedReceiver<TrustGranted>,
    runtime: Weak<MeshRuntime>,
    cancel: CancellationToken,
) {
    loop {
        let grant = tokio::select! {
            () = cancel.cancelled() => break,
            grant = grants.recv() => match grant {
                Some(grant) => grant,
                None => break,
            },
        };
        let Some(runtime) = runtime.upgrade() else {
            break;
        };
        for destination in
            runtime.sessions_a_grant_covers(&grant.identity_hash, grant.destination_hash.as_deref())
        {
            runtime.consider_card_fetch(&destination, CardTrigger::Granted);
        }
    }
}

impl MeshRuntime {
    /// The sessions a grant covers: its one destination, or every session of `identity`
    /// the peer table holds when it names none. A session the peer table does not hold
    /// is not asked for its card.
    fn sessions_a_grant_covers(&self, identity: &str, destination: Option<&str>) -> Vec<String> {
        match destination {
            Some(destination) => vec![destination.to_string()],
            None => self
                .peers()
                .snapshot()
                .into_iter()
                .filter(|row| row.identity_hash == identity)
                .map(|row| row.destination_hash)
                .collect(),
        }
    }

    /// The owner granted trust with `.mesh access trust`: one background card fetch to
    /// `destination`, or to every session of `identity` the peer table holds when no
    /// destination is named, whether or not they ever served this node. A session the
    /// peer table does not hold, or one with no known path, is not asked.
    pub(crate) fn fetch_cards_after_owner_grant(
        self: &Arc<Self>,
        identity: &str,
        destination: Option<&str>,
    ) {
        for destination in self.sessions_a_grant_covers(identity, destination) {
            self.consider_card_fetch(&destination, CardTrigger::OwnerGranted);
        }
    }

    /// Asks the session at `destination` for its card in the background when the cadence
    /// allows: the peer table holds it, it has not said it left, this node trusts it, and
    /// `CardCadence::admit` agrees. The fetch waits for one of `STATUS_MAX_CONCURRENCY`
    /// permits, never asks the network for a path and never sends a `/knock` of its own
    /// (a peer that no longer trusts this session files the refused request as a knock,
    /// which is why a refusal stops the cadence); it says nothing above the debug log.
    /// Returns the fetch's task, which a caller may drop.
    pub(crate) fn consider_card_fetch(
        self: &Arc<Self>,
        destination: &str,
        trigger: CardTrigger,
    ) -> Option<JoinHandle<()>> {
        let peer = self.peers().get(destination)?;
        if peer.left_at.is_some()
            || self
                .trust()
                .authorize(&peer.identity_hash, destination)
                .decision
                != Decision::Allow
        {
            return None;
        }
        let cadence = self.card_cadence();
        let served = self.peers().served_recently(destination);
        if !cadence.admit(destination, trigger, served) {
            return None;
        }
        let runtime = Arc::downgrade(self);
        let destination = destination.to_string();
        let permits = cadence.permits.clone();
        let cancel = self.cancellation_token();
        Some(tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = async {
                    let Ok(_permit) = permits.acquire_owned().await else {
                        return;
                    };
                    if let Some(runtime) = runtime.upgrade() {
                        runtime.fetch_card_in_background(&destination, trigger).await;
                    }
                } => {}
            }
        }))
    }

    async fn fetch_card_in_background(&self, destination: &str, trigger: CardTrigger) {
        let dest8 = short(destination);
        if !self.path_known(destination).await {
            debug!("Mesh background status card for {dest8} ({trigger:?}) skipped: no path");
            self.card_cadence().unasked(destination);
            return;
        }
        let Some(desc) = self.resolve_destination_cached(destination).await else {
            debug!("Mesh background status card for {dest8} ({trigger:?}) skipped: no route");
            self.card_cadence().unasked(destination);
            return;
        };
        let route = Route::unasked(desc);
        let options = route.options(self.request_timeouts().raise(RequestOptions::default()));
        let outcome = match self.fetch_status(&route.desc, options).await {
            Ok(_) => "read",
            Err(StatusError::NotTrustedByPeer(_)) => "refused",
            Err(StatusError::NoAnswer(_)) => "not answered",
            Err(StatusError::NotServed(_)) => "not served",
            Err(StatusError::Malformed(_)) => "malformed",
            Err(StatusError::UnsupportedVersion { .. }) => "of an unsupported version",
            Err(StatusError::Transport(_)) => "not delivered",
        };
        debug!("Mesh background status card for {dest8} ({trigger:?}): {outcome}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "dddddddddddddddddddddddddddddddd";

    #[test]
    fn only_a_session_that_served_this_node_is_asked_unless_the_owner_granted_it_trust() {
        let cadence = CardCadence::default();
        assert_eq!(
            cadence.permits.available_permits(),
            STATUS_MAX_CONCURRENCY,
            "background fetches share the status concurrency cap"
        );
        assert!(!cadence.admit(D, CardTrigger::Granted, false));
        assert!(!cadence.admit(D, CardTrigger::Heard, false));
        assert!(cadence.admit(D, CardTrigger::Granted, true));
        let other = "e".repeat(32);
        assert!(
            cadence.admit(&other, CardTrigger::OwnerGranted, false),
            "the owner's grant asks a session that never served this node"
        );
        assert!(
            !cadence.admit(&other, CardTrigger::OwnerGranted, false),
            "and only once per interval"
        );
        cadence.refused(&other);
        cadence.advance(CARD_FETCH_MIN_INTERVAL);
        assert!(
            !cadence.admit(&other, CardTrigger::Heard, true),
            "a refusal still stops the announces"
        );
        assert!(
            cadence.admit(&other, CardTrigger::OwnerGranted, false),
            "the owner's grant lifts the refusal and asks again after the interval"
        );
        assert!(!cadence.is_refused(&other));
        cadence.unasked(&other);
        assert!(
            cadence.admit(&other, CardTrigger::OwnerGranted, false),
            "a fetch that sent nothing gives its interval back"
        );
    }

    #[test]
    fn only_the_first_announce_heard_in_a_run_counts() {
        let cadence = CardCadence::default();
        assert!(cadence.admit(D, CardTrigger::Heard, true));
        for _ in 0..12 {
            cadence.advance(CARD_FETCH_MIN_INTERVAL);
            assert!(!cadence.admit(D, CardTrigger::Heard, true));
        }
    }

    #[test]
    fn a_destination_is_asked_at_most_once_per_interval() {
        let cadence = CardCadence::default();
        assert!(cadence.admit(D, CardTrigger::Granted, true));
        assert!(!cadence.admit(D, CardTrigger::Granted, true));
        cadence.advance(CARD_FETCH_MIN_INTERVAL - Duration::from_secs(1));
        assert!(!cadence.admit(D, CardTrigger::Granted, true));
        cadence.advance(Duration::from_secs(1));
        assert!(cadence.admit(D, CardTrigger::Granted, true));
        assert!(
            cadence.admit(&"e".repeat(32), CardTrigger::Granted, true),
            "the interval is per destination"
        );
    }

    #[test]
    fn a_refusal_stops_the_cadence_until_the_trust_changes_or_the_owner_asks() {
        let cadence = CardCadence::default();
        cadence.refused(D);
        assert!(!cadence.admit(D, CardTrigger::Heard, true));
        for _ in 0..6 {
            cadence.advance(CARD_FETCH_MIN_INTERVAL);
            assert!(!cadence.admit(D, CardTrigger::Heard, true));
        }
        cadence.lift(D);
        assert!(!cadence.is_refused(D));
        cadence.refused(D);
        assert!(
            cadence.admit(D, CardTrigger::Granted, true),
            "a grant on this side is a trust change and lifts the refusal"
        );
        assert!(!cadence.is_refused(D));
    }
}

#[cfg(all(test, unix))]
mod network {
    use super::*;
    use crate::mesh::node::MeshSlot;
    use crate::mesh::request::STATUS_PATH;
    use crate::mesh::test_support::{
        TransportRelay, closed_port, started_runtime_on, started_runtime_on_with, status_round_trip,
    };
    use std::time::SystemTime;

    fn status_requests(runtime: &MeshRuntime) -> usize {
        runtime
            .requests_made()
            .iter()
            .filter(|(path, _)| path == STATUS_PATH)
            .count()
    }

    fn knocks(at: &MeshRuntime, from_identity: &str) -> usize {
        at.knock_gate()
            .cache()
            .list(SystemTime::now())
            .unwrap()
            .iter()
            .filter(|knock| knock.identity_hash == from_identity)
            .count()
    }

    /// Waits for `at` to have filed `count` knocks from `from_identity`; knocks are filed
    /// off the request path.
    async fn wait_for_knocks(at: &MeshRuntime, from_identity: &str, count: usize) {
        for _ in 0..100 {
            if knocks(at, from_identity) >= count {
                assert_eq!(knocks(at, from_identity), count);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{count} knock(s) were never filed");
    }

    /// A card from a session A trusts is kept as that session's and its person's
    /// advertised names; after B revokes A, an hour of the triggers A's background
    /// fetch answers to (B's announces every ten minutes) files nothing at B: no status
    /// request leaves A and B holds no knock from A. A refusal then stops the cadence
    /// for good until the trust changes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_b_revokes_a_an_hour_of_the_background_cadence_files_nothing_at_b() {
        let relay = TransportRelay::start_on_with_retries(closed_port().await, 0).await;
        let a = started_runtime_on("cadence-revoke-a", relay.port).await;
        let b = started_runtime_on_with("cadence-revoke-b", relay.port, |config| {
            config.person_name = Some("Beatrice".into());
        })
        .await;
        let slot_a = Arc::new(MeshSlot::default());
        slot_a.install(a.runtime.clone()).unwrap();
        let slot_b = Arc::new(MeshSlot::default());
        slot_b.install(b.runtime.clone()).unwrap();
        slot_b.rename_session("temp");
        let (a_rt, b_rt) = (&a.runtime, &b.runtime);
        let b_dest = b_rt.current_destination_hash();
        let b_identity = b_rt.fingerprint().to_string();

        let card = status_round_trip(&slot_a, &slot_b).await;
        assert_eq!(card.person_name.as_deref(), Some("Beatrice"));
        assert_eq!(card.session.as_deref(), Some("temp"));
        let people = a_rt.trust().people(&[]);
        assert_eq!(
            people.person(&b_identity).map(|alias| alias.text.as_str()),
            Some("Beatrice")
        );
        assert_eq!(
            people
                .session(&b_identity, &b_dest)
                .map(|alias| alias.text.as_str()),
            Some("temp")
        );
        assert!(a_rt.peers().served_recently(&b_dest));

        slot_b.rename_session("named-session");
        // The grant `status_round_trip` made may already have fetched in the background.
        a_rt.card_cadence().advance(CARD_FETCH_MIN_INTERVAL);
        let fetch = a_rt
            .consider_card_fetch(&b_dest, CardTrigger::Granted)
            .expect("a session that served A and is trusted is fetched after a grant");
        fetch.await.unwrap();
        assert_eq!(
            a_rt.trust()
                .people(&[])
                .session(&b_identity, &b_dest)
                .map(|alias| alias.text.clone())
                .as_deref(),
            Some("named-session"),
            "a later card replaces the session name an earlier one gave"
        );
        assert!(
            a_rt.consider_card_fetch(&b_dest, CardTrigger::Granted)
                .is_none(),
            "at most one background fetch per destination per ten minutes"
        );

        b_rt.trust()
            .untrust_destination(slot_b.as_ref(), &a_rt.current_destination_hash(), false)
            .unwrap();
        let asked = status_requests(a_rt);
        // B's announces over the next hour: A has heard B in this run, so none of them
        // prompts a fetch, and nothing reaches B.
        for _ in 0..6 {
            a_rt.card_cadence().advance(CARD_FETCH_MIN_INTERVAL);
            if let Some(fetch) = a_rt.consider_card_fetch(&b_dest, CardTrigger::Heard) {
                fetch.await.unwrap();
            }
        }
        assert_eq!(
            status_requests(a_rt),
            asked,
            "an hour of B's announces sends B no status request"
        );
        let knocks_from_a = |at: &MeshRuntime| knocks(at, a_rt.fingerprint());
        assert_eq!(knocks_from_a(b_rt), 0, "B holds no knock from A");

        // A grant on A's side (a trust change on A's side) asks once more. B refuses it,
        // and since B knows A's identity but no longer trusts its session, B files that
        // one request as a knock: the one a refusal costs before the stop holds.
        a_rt.card_cadence().advance(CARD_FETCH_MIN_INTERVAL);
        a_rt.consider_card_fetch(&b_dest, CardTrigger::Granted)
            .expect("a grant on A's side asks once more")
            .await
            .unwrap();
        assert_eq!(status_requests(a_rt), asked + 1);
        assert!(a_rt.card_cadence().is_refused(&b_dest));
        wait_for_knocks(b_rt, a_rt.fingerprint(), 1).await;

        // From here the stop alone holds the cadence: each hour step forgets that B was
        // heard, as a new run would, so every one of these announces would ask B but for
        // the refusal.
        let asked = status_requests(a_rt);
        for _ in 0..6 {
            a_rt.card_cadence().advance(CARD_FETCH_MIN_INTERVAL);
            a_rt.card_cadence().forget_heard(&b_dest);
            assert!(
                a_rt.consider_card_fetch(&b_dest, CardTrigger::Heard)
                    .is_none()
            );
        }
        assert_eq!(status_requests(a_rt), asked);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            knocks_from_a(b_rt),
            1,
            "an hour of announces after the refusal files nothing more at B"
        );
        assert_eq!(
            a_rt.trust()
                .people(&[])
                .session(&b_identity, &b_dest)
                .map(|alias| alias.text.clone())
                .as_deref(),
            Some("named-session"),
            "a refusal keeps the names the last card gave"
        );
        // A refusal reaches tools' results unfenced, so it names B by A's own aliases or
        // B's hashes, never by the names B's card advertised.
        let refusal = a_rt.remote_refusal(&b_dest);
        for text in [refusal.to_string(), refusal.next_action()] {
            assert!(
                !text.contains("Beatrice") && !text.contains("named-session"),
                "{text}"
            );
        }
        a_rt.trust()
            .set_person_alias(&b_identity, Some("Bea"), &[])
            .unwrap();
        assert!(
            a_rt.remote_refusal(&b_dest).to_string().contains("Bea"),
            "the owner's own alias still names the peer"
        );
        a_rt.note_served_by(&b_dest);
        assert!(
            !a_rt.card_cadence().is_refused(&b_dest),
            "a message from B is a trust change on B's side and lifts the stop"
        );

        assert!(slot_a.stop().await.unwrap());
        assert!(slot_b.stop().await.unwrap());
        relay.stop().await;
    }

    /// A session this node does not trust is never fetched in the background, even one
    /// that served it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_untrusted_session_is_never_fetched_in_the_background() {
        let relay = TransportRelay::start_on_with_retries(closed_port().await, 0).await;
        let a = started_runtime_on("cadence-untrusted-a", relay.port).await;
        let b = started_runtime_on("cadence-untrusted-b", relay.port).await;
        let slot_a = Arc::new(MeshSlot::default());
        slot_a.install(a.runtime.clone()).unwrap();
        let slot_b = Arc::new(MeshSlot::default());
        slot_b.install(b.runtime.clone()).unwrap();
        let b_dest = b.runtime.current_destination_hash();
        crate::mesh::test_support::wait_for_peer(&a.runtime, &b_dest).await;
        a.runtime.peers().mark_served(&b_dest);

        for trigger in [CardTrigger::Granted, CardTrigger::Heard] {
            assert!(a.runtime.consider_card_fetch(&b_dest, trigger).is_none());
        }
        assert_eq!(status_requests(&a.runtime), 0);

        assert!(slot_a.stop().await.unwrap());
        assert!(slot_b.stop().await.unwrap());
        relay.stop().await;
    }
}
