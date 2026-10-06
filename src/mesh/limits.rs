//! Per-peer ceilings on what this node accepts and spends: how many messages an identity
//! may send an hour, how many envoy runs it may have queued or running at once, and how
//! many tokens or dollars its runs may burn an hour. Windows are fixed hours per identity,
//! kept in memory only, so a restart opens a fresh window. A refusal carries a typed
//! reason back to the peer; the REPL sees the first refusal per identity, per reason, per
//! window, and a folded count of the rest the next time that peer is heard from after
//! the window rolls over. The peer's own reply budget on the store-and-forward path is
//! counted apart from the REPL fold, so a refused reply cannot spend an ask's reply.

use crate::config::mesh_config::MeshConfig;

use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const PEER_WINDOW: Duration = Duration::from_secs(60 * 60);
/// Identities the limiter keeps windows for. Only trusted identities get this far, so the
/// cap is a backstop against a trust list of thousands, not against an attacker minting
/// identities. An evicted identity returns with a fresh window.
pub(crate) const PEER_LIMITS_MAX_IDENTITIES: usize = 256;
/// How long a refused peer is told to wait when the reason is capacity, not the window:
/// the envoy's run ceiling.
pub(crate) const PEER_RETRY_AFTER_CAPACITY: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PeerLimitConfig {
    pub messages_per_hour: u32,
    pub concurrency: u32,
    pub tokens_per_hour: u64,
    /// 0.0 = no cost ceiling.
    pub cost_usd_per_hour: f64,
}

impl From<&MeshConfig> for PeerLimitConfig {
    fn from(mesh: &MeshConfig) -> Self {
        Self {
            messages_per_hour: mesh.peer_max_messages_per_hour,
            concurrency: mesh.peer_max_concurrent,
            tokens_per_hour: mesh.peer_max_tokens_per_hour,
            cost_usd_per_hour: mesh.peer_max_cost_usd_per_hour,
        }
    }
}

impl Default for PeerLimitConfig {
    fn default() -> Self {
        Self::from(&MeshConfig::default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RefusalReason {
    RateLimited,
    EnvoyBusy,
    /// The job was queued when the envoy stopped; the inbox keeps it.
    EnvoyStopping,
    PeerConcurrency,
    TokenCeiling,
    CostCeiling,
    /// The job carries `in_reply_to`, so answering it would answer a reply. Never sent
    /// to the peer: a reply to a reply is exactly what the guard exists to stop.
    LoopGuard,
}

impl RefusalReason {
    pub(crate) const ALL: [RefusalReason; 7] = [
        RefusalReason::RateLimited,
        RefusalReason::EnvoyBusy,
        RefusalReason::EnvoyStopping,
        RefusalReason::PeerConcurrency,
        RefusalReason::TokenCeiling,
        RefusalReason::CostCeiling,
        RefusalReason::LoopGuard,
    ];

    /// Wire and log spelling.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RefusalReason::RateLimited => "rate_limited",
            RefusalReason::EnvoyBusy => "envoy_busy",
            RefusalReason::EnvoyStopping => "envoy_stopping",
            RefusalReason::PeerConcurrency => "peer_concurrency",
            RefusalReason::TokenCeiling => "token_ceiling",
            RefusalReason::CostCeiling => "cost_ceiling",
            RefusalReason::LoopGuard => "loop_guard",
        }
    }

    /// One sentence for the peer, naming no identifiers.
    pub(crate) fn peer_text(self) -> &'static str {
        match self {
            RefusalReason::RateLimited => {
                "This node has taken as many messages from you as it accepts in an hour; try again when the hour is up."
            }
            RefusalReason::EnvoyBusy => {
                "This node's envoy is busy; try again in a couple of minutes."
            }
            RefusalReason::EnvoyStopping => "No answer: this node is shutting down.",
            RefusalReason::PeerConcurrency => {
                "This node is not taking another message from you until the current one finishes."
            }
            RefusalReason::TokenCeiling => {
                "This node has spent as many model tokens on you as it allows in an hour; try again when the hour is up."
            }
            RefusalReason::CostCeiling => {
                "This node has spent as much on you as it allows in an hour; try again when the hour is up."
            }
            RefusalReason::LoopGuard => "This node does not answer replies.",
        }
    }

    fn slot(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PeerRefusal {
    pub reason: RefusalReason,
    pub retry_after: Duration,
}

impl PeerRefusal {
    /// A refusal whose remedy is waiting out a run, not a window.
    pub(crate) fn capacity(reason: RefusalReason) -> Self {
        Self {
            reason,
            retry_after: PEER_RETRY_AFTER_CAPACITY,
        }
    }

    /// Seconds the peer is told to wait: rounded up, at least 1.
    pub(crate) fn retry_after_secs(&self) -> u64 {
        let whole = self.retry_after.as_secs();
        let secs = whole + u64::from(self.retry_after.subsec_nanos() > 0);
        secs.max(1)
    }

    /// `{"refusal": "<reason>", "retry_after_secs": <retry_after_secs>}` for the
    /// correlated reply's `fields`.
    pub(crate) fn fields(&self) -> serde_json::Value {
        json!({
            "refusal": self.reason.as_str(),
            "retry_after_secs": self.retry_after_secs(),
        })
    }
}

/// What the caller prints about a refusal. `surface` is true for the first refusal of a
/// reason from an identity in the current window; later ones fold, counted but not
/// printed. `reports` carries the folded counts of windows that have since rolled over,
/// each handed out exactly once.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct FoldNotice {
    pub surface: bool,
    pub reports: Vec<(RefusalReason, u32)>,
}

/// One identity's current window, for inspection.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct WindowView {
    pub messages: u32,
    pub tokens: u64,
    pub cost_usd: f64,
    pub in_flight: u32,
}

struct IdentityWindow {
    last_seen: Instant,
    started: Instant,
    messages: u32,
    tokens: u64,
    cost_usd: f64,
    in_flight: u32,
    surfaced: [bool; RefusalReason::ALL.len()],
    folded: [u32; RefusalReason::ALL.len()],
    told: [bool; RefusalReason::ALL.len()],
    pending: Vec<(RefusalReason, u32)>,
}

impl IdentityWindow {
    fn new(now: Instant) -> Self {
        Self {
            last_seen: now,
            started: now,
            messages: 0,
            tokens: 0,
            cost_usd: 0.0,
            in_flight: 0,
            surfaced: [false; RefusalReason::ALL.len()],
            folded: [0; RefusalReason::ALL.len()],
            told: [false; RefusalReason::ALL.len()],
            pending: Vec::new(),
        }
    }

    /// Advances to the window containing `now` when the current one has ended, resetting
    /// the hourly counts and moving folded refusal counts into the pending reports.
    /// `in_flight` is not windowed and survives.
    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed < PEER_WINDOW {
            return;
        }
        let windows =
            u32::try_from(elapsed.as_nanos() / PEER_WINDOW.as_nanos()).unwrap_or(u32::MAX);
        self.started = self
            .started
            .checked_add(PEER_WINDOW.saturating_mul(windows))
            .unwrap_or(now);
        self.messages = 0;
        self.tokens = 0;
        self.cost_usd = 0.0;
        self.surfaced = [false; RefusalReason::ALL.len()];
        self.told = [false; RefusalReason::ALL.len()];
        for reason in RefusalReason::ALL {
            let count = std::mem::take(&mut self.folded[reason.slot()]);
            if count > 0 {
                self.pending.push((reason, count));
            }
        }
    }

    fn remaining(&self, now: Instant) -> Duration {
        self.started
            .checked_add(PEER_WINDOW)
            .map_or(Duration::ZERO, |end| end.saturating_duration_since(now))
    }

    /// The token ceiling, then the cost ceiling when one is configured.
    fn check_ceilings(&self, config: &PeerLimitConfig, now: Instant) -> Result<(), PeerRefusal> {
        if self.tokens >= config.tokens_per_hour {
            return Err(PeerRefusal {
                reason: RefusalReason::TokenCeiling,
                retry_after: self.remaining(now),
            });
        }
        if config.cost_usd_per_hour > 0.0 && self.cost_usd >= config.cost_usd_per_hour {
            return Err(PeerRefusal {
                reason: RefusalReason::CostCeiling,
                retry_after: self.remaining(now),
            });
        }
        Ok(())
    }

    /// Whether one more run may start now: the concurrency slot, then the ceilings.
    fn admissible(&self, config: &PeerLimitConfig, now: Instant) -> Result<(), PeerRefusal> {
        if self.in_flight >= config.concurrency {
            return Err(PeerRefusal::capacity(RefusalReason::PeerConcurrency));
        }
        self.check_ceilings(config, now)
    }

    #[cfg(test)]
    fn view(&self) -> WindowView {
        WindowView {
            messages: self.messages,
            tokens: self.tokens,
            cost_usd: self.cost_usd,
            in_flight: self.in_flight,
        }
    }
}

#[derive(Default)]
struct LimitState {
    identities: HashMap<String, IdentityWindow>,
}

impl LimitState {
    /// The window for `identity`, made fresh if absent and rolled to the hour containing
    /// `now`. At the cap the least recently seen identity with nothing in flight makes
    /// room; `None` when every identity is in flight, so the newcomer has no slot.
    fn entry(&mut self, identity: &str, now: Instant) -> Option<&mut IdentityWindow> {
        if !self.identities.contains_key(identity)
            && self.identities.len() >= PEER_LIMITS_MAX_IDENTITIES
        {
            let stalest = self
                .identities
                .iter()
                .filter(|(_, window)| window.in_flight == 0)
                .min_by_key(|(_, window)| window.last_seen)
                .map(|(key, _)| key.clone())?;
            self.identities.remove(&stalest);
        }
        let window = self
            .identities
            .entry(identity.to_string())
            .or_insert_with(|| IdentityWindow::new(now));
        window.roll(now);
        window.last_seen = now;
        Some(window)
    }
}

pub(crate) struct PeerLimits {
    config: Mutex<PeerLimitConfig>,
    state: Mutex<LimitState>,
}

/// One envoy run's place in its sender's concurrency count, given back when dropped, so
/// every way a job ends releases it exactly once.
pub(crate) struct Reservation {
    limits: Arc<PeerLimits>,
    identity: String,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.limits.release(&self.identity);
    }
}

impl Default for PeerLimits {
    fn default() -> Self {
        Self::new(PeerLimitConfig::default())
    }
}

impl PeerLimits {
    pub(crate) fn new(config: PeerLimitConfig) -> Self {
        Self {
            config: Mutex::new(config),
            state: Mutex::new(LimitState::default()),
        }
    }

    pub(crate) fn configure(&self, config: PeerLimitConfig) {
        *self.config.lock() = config;
    }

    pub(crate) fn config(&self) -> PeerLimitConfig {
        *self.config.lock()
    }

    /// Counts one inbound message. `RateLimited` once the window already holds
    /// `messages_per_hour`; the refused message is not counted.
    pub(crate) fn admit_message(&self, identity: &str, now: Instant) -> Result<(), PeerRefusal> {
        let config = self.config();
        let mut state = self.state.lock();
        let Some(window) = state.entry(identity, now) else {
            return Err(PeerRefusal::capacity(RefusalReason::PeerConcurrency));
        };
        if window.messages >= config.messages_per_hour {
            return Err(PeerRefusal {
                reason: RefusalReason::RateLimited,
                retry_after: window.remaining(now),
            });
        }
        window.messages += 1;
        Ok(())
    }

    /// Reserves one envoy run for the identity: concurrency first, then the token
    /// ceiling, then the cost ceiling when one is configured, and the slot is taken under
    /// the same lock, so two jobs arriving together cannot both pass. A refusal reserves
    /// nothing.
    pub(crate) fn try_reserve(
        self: &Arc<Self>,
        identity: &str,
        now: Instant,
    ) -> Result<Reservation, PeerRefusal> {
        let config = self.config();
        let mut state = self.state.lock();
        let Some(window) = state.entry(identity, now) else {
            return Err(PeerRefusal::capacity(RefusalReason::PeerConcurrency));
        };
        window.admissible(&config, now)?;
        window.in_flight = window.in_flight.saturating_add(1);
        Ok(Reservation {
            limits: Arc::clone(self),
            identity: identity.to_string(),
        })
    }

    /// `try_reserve` for a run that already holds a reservation: the ceilings alone,
    /// since its own slot would otherwise count against it.
    pub(crate) fn admit_reserved(&self, identity: &str, now: Instant) -> Result<(), PeerRefusal> {
        let config = self.config();
        let mut state = self.state.lock();
        let Some(window) = state.entry(identity, now) else {
            return Err(PeerRefusal::capacity(RefusalReason::PeerConcurrency));
        };
        window.check_ceilings(&config, now)
    }

    /// Whether `try_reserve` would admit a run for the identity right now: a run already
    /// in flight, then the ceilings. Reserves no run and counts no message; it files the
    /// identity's window like any other look. A link can ask before it acknowledges, and
    /// the answer holds only until something else moves.
    pub(crate) fn check_run_admissible(
        &self,
        identity: &str,
        now: Instant,
    ) -> Result<(), PeerRefusal> {
        let config = self.config();
        let mut state = self.state.lock();
        let Some(window) = state.entry(identity, now) else {
            return Err(PeerRefusal::capacity(RefusalReason::PeerConcurrency));
        };
        window.admissible(&config, now)
    }

    fn release(&self, identity: &str) {
        if let Some(window) = self.state.lock().identities.get_mut(identity) {
            window.in_flight = window.in_flight.saturating_sub(1);
        }
    }

    /// Charges a finished run to the identity's current window: at least one token, and
    /// the cost only when it is a positive finite amount. A debit may push the window
    /// past a ceiling; the next `try_reserve` refuses.
    pub(crate) fn debit(&self, identity: &str, tokens: u64, cost_usd: Option<f64>, now: Instant) {
        if let Some(window) = self.state.lock().entry(identity, now) {
            window.tokens = window.tokens.saturating_add(tokens.max(1));
            if let Some(cost) = cost_usd.filter(|cost| cost.is_finite() && *cost > 0.0) {
                window.cost_usd += cost;
            }
        }
    }

    /// Records a refusal for folding; see [`FoldNotice`]. An identity with no slot is
    /// always surfaced, since nothing can fold it.
    pub(crate) fn note_refusal(
        &self,
        identity: &str,
        reason: RefusalReason,
        now: Instant,
    ) -> FoldNotice {
        let mut state = self.state.lock();
        let Some(window) = state.entry(identity, now) else {
            return FoldNotice {
                surface: true,
                reports: Vec::new(),
            };
        };
        let reports = std::mem::take(&mut window.pending);
        let slot = reason.slot();
        let surface = !window.surfaced[slot];
        if surface {
            window.surfaced[slot] = true;
        } else {
            window.folded[slot] = window.folded[slot].saturating_add(1);
        }
        FoldNotice { surface, reports }
    }

    /// Takes the one typed reply the store-and-forward path owes `identity` for
    /// `reason` this window: true exactly once per identity, per reason, per window,
    /// whatever `note_refusal` has surfaced or folded. An identity with no slot is
    /// always told, since nothing can remember that it was.
    pub(crate) fn claim_peer_reply(
        &self,
        identity: &str,
        reason: RefusalReason,
        now: Instant,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(window) = state.entry(identity, now) else {
            return true;
        };
        let slot = reason.slot();
        let claimed = !window.told[slot];
        window.told[slot] = true;
        claimed
    }

    /// Drains the pending rollover reports without recording anything, for the caller to
    /// print when an identity's message is admitted after a refused window.
    pub(crate) fn take_fold_reports(
        &self,
        identity: &str,
        now: Instant,
    ) -> Vec<(RefusalReason, u32)> {
        self.state
            .lock()
            .entry(identity, now)
            .map(|window| std::mem::take(&mut window.pending))
            .unwrap_or_default()
    }

    /// The identity's current window, if it is known.
    #[cfg(test)]
    pub(crate) fn window_of(&self, identity: &str, now: Instant) -> Option<WindowView> {
        let mut state = self.state.lock();
        let window = state.identities.get_mut(identity)?;
        window.roll(now);
        Some(window.view())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    fn limits(config: PeerLimitConfig) -> Arc<PeerLimits> {
        Arc::new(PeerLimits::new(config))
    }

    fn default_limits() -> Arc<PeerLimits> {
        limits(PeerLimitConfig::default())
    }

    fn refusal_of<T>(result: Result<T, PeerRefusal>) -> PeerRefusal {
        result.err().expect("expected a refusal")
    }

    fn reason_of<T>(result: Result<T, PeerRefusal>) -> RefusalReason {
        refusal_of(result).reason
    }

    #[test]
    fn config_maps_the_four_mesh_knobs_and_defaults_match() {
        let mesh = MeshConfig {
            peer_max_messages_per_hour: 7,
            peer_max_concurrent: 3,
            peer_max_tokens_per_hour: 4_000,
            peer_max_cost_usd_per_hour: 2.5,
            ..Default::default()
        };
        assert_eq!(
            PeerLimitConfig::from(&mesh),
            PeerLimitConfig {
                messages_per_hour: 7,
                concurrency: 3,
                tokens_per_hour: 4_000,
                cost_usd_per_hour: 2.5,
            }
        );
        assert_eq!(
            PeerLimitConfig::default(),
            PeerLimitConfig {
                messages_per_hour: 60,
                concurrency: 1,
                tokens_per_hour: 100_000,
                cost_usd_per_hour: 0.0,
            }
        );
        assert_eq!(PeerLimits::default().config(), PeerLimitConfig::default());
    }

    #[test]
    fn configure_replaces_the_config() {
        let limits = default_limits();
        let tight = PeerLimitConfig {
            messages_per_hour: 1,
            ..PeerLimitConfig::default()
        };
        limits.configure(tight);
        assert_eq!(limits.config(), tight);
        let start = now();
        assert!(limits.admit_message("a", start).is_ok());
        assert_eq!(
            reason_of(limits.admit_message("a", start)),
            RefusalReason::RateLimited
        );
    }

    #[test]
    fn capacity_retry_after_is_the_envoy_run_ceiling() {
        assert_eq!(
            PEER_RETRY_AFTER_CAPACITY.as_secs(),
            crate::config::mesh_envoy::ENVOY_RUN_TIMEOUT_SECS
        );
    }

    #[test]
    fn refusal_reasons_spell_and_speak_distinctly() {
        let spellings: Vec<&str> = RefusalReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            spellings,
            [
                "rate_limited",
                "envoy_busy",
                "envoy_stopping",
                "peer_concurrency",
                "token_ceiling",
                "cost_ceiling",
                "loop_guard",
            ]
        );
        assert!(
            RefusalReason::ALL
                .iter()
                .enumerate()
                .all(|(i, r)| r.slot() == i)
        );
        for reason in RefusalReason::ALL {
            let text = reason.peer_text();
            assert!(text.ends_with('.'), "{text}");
            assert!(text.is_ascii(), "{text}");
        }
        assert_eq!(
            RefusalReason::PeerConcurrency.peer_text(),
            "This node is not taking another message from you until the current one finishes."
        );
    }

    #[test]
    fn refusal_fields_carry_the_reason_and_a_ceiled_retry_after() {
        let refusal = PeerRefusal {
            reason: RefusalReason::TokenCeiling,
            retry_after: Duration::from_millis(1_500),
        };
        assert_eq!(
            refusal.fields().to_string(),
            r#"{"refusal":"token_ceiling","retry_after_secs":2}"#
        );
        let instant = PeerRefusal {
            reason: RefusalReason::RateLimited,
            retry_after: Duration::ZERO,
        };
        assert_eq!(
            instant.fields().to_string(),
            r#"{"refusal":"rate_limited","retry_after_secs":1}"#
        );
        let exact = PeerRefusal {
            reason: RefusalReason::PeerConcurrency,
            retry_after: PEER_RETRY_AFTER_CAPACITY,
        };
        assert_eq!(exact.fields()["retry_after_secs"], 120);
    }

    #[test]
    fn admit_message_refuses_the_sixty_first_in_an_hour_and_resets_after_rollover() {
        let limits = default_limits();
        let start = now();
        for n in 0..60 {
            let at = start + Duration::from_secs(n);
            assert!(limits.admit_message("a", at).is_ok(), "message {n}");
        }
        let at = start + Duration::from_secs(90);
        let refusal = limits.admit_message("a", at).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::RateLimited);
        assert_eq!(refusal.retry_after, PEER_WINDOW - Duration::from_secs(90));
        assert_eq!(limits.window_of("a", at).unwrap().messages, 60);

        let rolled = start + PEER_WINDOW;
        assert!(limits.admit_message("a", rolled).is_ok());
        assert_eq!(limits.window_of("a", rolled).unwrap().messages, 1);
    }

    #[test]
    fn admit_message_keeps_identities_apart() {
        let limits = limits(PeerLimitConfig {
            messages_per_hour: 1,
            ..PeerLimitConfig::default()
        });
        let start = now();
        assert!(limits.admit_message("a", start).is_ok());
        assert!(limits.admit_message("b", start).is_ok());
        assert_eq!(
            reason_of(limits.admit_message("a", start)),
            RefusalReason::RateLimited
        );
    }

    #[test]
    fn rollover_skips_whole_windows_and_keeps_the_grid() {
        let limits = default_limits();
        let start = now();
        assert!(limits.admit_message("a", start).is_ok());
        let late = start + PEER_WINDOW * 3 + Duration::from_secs(10);
        assert!(limits.admit_message("a", late).is_ok());
        limits.configure(PeerLimitConfig {
            messages_per_hour: 1,
            ..PeerLimitConfig::default()
        });
        let refusal = limits.admit_message("a", late).unwrap_err();
        assert_eq!(refusal.retry_after, PEER_WINDOW - Duration::from_secs(10));
    }

    #[test]
    fn try_reserve_refuses_while_a_run_is_in_flight_and_admits_once_the_guard_drops() {
        let limits = default_limits();
        let start = now();
        let first = limits.try_reserve("a", start).unwrap();
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 1);
        let refusal = refusal_of(limits.try_reserve("a", start));
        assert_eq!(refusal.reason, RefusalReason::PeerConcurrency);
        assert_eq!(refusal.retry_after, PEER_RETRY_AFTER_CAPACITY);
        assert_eq!(
            limits.window_of("a", start).unwrap().in_flight,
            1,
            "a refusal reserves nothing"
        );
        let other = limits.try_reserve("b", start).unwrap();
        assert_eq!(limits.window_of("b", start).unwrap().in_flight, 1);

        drop(first);
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 0);
        let second = limits.try_reserve("a", start).unwrap();
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 1);
        drop(second);
        drop(other);
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 0);
        assert_eq!(limits.window_of("b", start).unwrap().in_flight, 0);
    }

    #[test]
    fn a_ceiling_refusal_reserves_nothing() {
        let limits = limits(PeerLimitConfig {
            tokens_per_hour: 10,
            ..PeerLimitConfig::default()
        });
        let start = now();
        limits.debit("a", 10, None, start);
        assert_eq!(
            reason_of(limits.try_reserve("a", start)),
            RefusalReason::TokenCeiling
        );
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 0);
    }

    /// The admissibility check answers what `try_reserve` would, in its order, and
    /// leaves the window exactly as it found it: asked twice it says yes twice.
    #[test]
    fn check_run_admissible_mirrors_try_reserve_without_reserving_or_counting() {
        let limits = limits(PeerLimitConfig {
            tokens_per_hour: 100,
            cost_usd_per_hour: 0.5,
            ..PeerLimitConfig::default()
        });
        let start = now();
        assert!(limits.check_run_admissible("a", start).is_ok());
        assert!(limits.check_run_admissible("a", start).is_ok());
        assert!(limits.check_run_admissible("b", start).is_ok());
        assert_eq!(
            limits.window_of("a", start).unwrap(),
            WindowView {
                messages: 0,
                tokens: 0,
                cost_usd: 0.0,
                in_flight: 0,
            }
        );

        let held = limits.try_reserve("a", start).unwrap();
        let busy = refusal_of(limits.check_run_admissible("a", start));
        assert_eq!(busy.reason, RefusalReason::PeerConcurrency);
        assert_eq!(busy.retry_after, PEER_RETRY_AFTER_CAPACITY);
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 1);
        drop(held);
        assert!(limits.check_run_admissible("a", start).is_ok());

        let at = start + Duration::from_secs(600);
        limits.debit("a", 100, None, at);
        let spent = refusal_of(limits.check_run_admissible("a", at));
        assert_eq!(spent.reason, RefusalReason::TokenCeiling);
        assert_eq!(spent.retry_after, PEER_WINDOW - Duration::from_secs(600));

        limits.debit("b", 1, Some(0.5), at);
        let costly = refusal_of(limits.check_run_admissible("b", at));
        assert_eq!(costly.reason, RefusalReason::CostCeiling);
        assert_eq!(costly.retry_after, PEER_WINDOW - Duration::from_secs(600));

        let rolled = start + PEER_WINDOW;
        assert!(limits.check_run_admissible("a", rolled).is_ok());
        assert!(limits.check_run_admissible("b", rolled).is_ok());
        assert_eq!(limits.window_of("a", rolled).unwrap().in_flight, 0);
    }

    /// The pre-ack look counts the sender's own runs against the configured concurrency
    /// (MESH-MSG-041): under a concurrency of two, one run in flight still admits, two
    /// refuse; another identity's runs never count. When the slot and a ceiling are both
    /// spent the slot is named first, as `try_reserve` would, and the ceiling shows once
    /// a run ends.
    #[test]
    fn usage_probe_check_run_admissible_counts_the_senders_runs_against_the_concurrency() {
        let limits = limits(PeerLimitConfig {
            concurrency: 2,
            tokens_per_hour: 100,
            ..PeerLimitConfig::default()
        });
        let start = now();

        let first = limits.try_reserve("a", start).unwrap();
        assert!(
            limits.check_run_admissible("a", start).is_ok(),
            "one run in flight under a concurrency of two admits"
        );
        let second = limits.try_reserve("a", start).unwrap();
        let at_cap = refusal_of(limits.check_run_admissible("a", start));
        assert_eq!(at_cap.reason, RefusalReason::PeerConcurrency);
        assert_eq!(at_cap.retry_after, PEER_RETRY_AFTER_CAPACITY);
        assert!(
            limits.check_run_admissible("b", start).is_ok(),
            "another identity's runs do not count against b"
        );
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 2);
        assert_eq!(
            limits.window_of("b", start).unwrap().in_flight,
            0,
            "asking reserves nothing for b"
        );

        // Both the slot and the token ceiling spent: the slot is named, and the answer
        // is the one `try_reserve` gives.
        let at = start + Duration::from_secs(60);
        limits.debit("a", 100, None, at);
        let both = refusal_of(limits.check_run_admissible("a", at));
        assert_eq!(both.reason, RefusalReason::PeerConcurrency);
        assert_eq!(
            reason_of(limits.try_reserve("a", at)),
            RefusalReason::PeerConcurrency
        );

        drop(second);
        let ceiling = refusal_of(limits.check_run_admissible("a", at));
        assert_eq!(ceiling.reason, RefusalReason::TokenCeiling);
        assert_eq!(ceiling.retry_after, PEER_WINDOW - Duration::from_secs(60));
        assert_eq!(
            reason_of(limits.try_reserve("a", at)),
            RefusalReason::TokenCeiling
        );
        drop(first);
        assert_eq!(limits.window_of("a", at).unwrap().in_flight, 0);
        assert!(
            limits.check_run_admissible("b", at).is_ok(),
            "b's window is untouched by a's spend"
        );
    }

    #[test]
    fn in_flight_survives_rollover() {
        let limits = default_limits();
        let start = now();
        let _held = limits.try_reserve("a", start).unwrap();
        let rolled = start + PEER_WINDOW;
        assert_eq!(
            reason_of(limits.try_reserve("a", rolled)),
            RefusalReason::PeerConcurrency
        );
        assert_eq!(limits.window_of("a", rolled).unwrap().in_flight, 1);
    }

    #[test]
    fn admit_reserved_skips_the_concurrency_check_but_not_the_ceilings() {
        let limits = limits(PeerLimitConfig {
            concurrency: 1,
            tokens_per_hour: 100,
            ..PeerLimitConfig::default()
        });
        let start = now();
        let _held = limits.try_reserve("a", start).unwrap();
        assert_eq!(
            reason_of(limits.try_reserve("a", start)),
            RefusalReason::PeerConcurrency
        );
        assert!(limits.admit_reserved("a", start).is_ok());
        limits.debit("a", 100, None, start);
        let refusal = limits.admit_reserved("a", start).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::TokenCeiling);
        assert_eq!(refusal.retry_after, PEER_WINDOW);
        assert_eq!(limits.window_of("a", start).unwrap().in_flight, 1);
    }

    #[test]
    fn try_reserve_refuses_past_the_token_ceiling_until_rollover() {
        let limits = limits(PeerLimitConfig {
            tokens_per_hour: 100,
            ..PeerLimitConfig::default()
        });
        let start = now();
        drop(limits.try_reserve("a", start).unwrap());
        limits.debit("a", 400, None, start);
        let at = start + Duration::from_secs(600);
        let refusal = refusal_of(limits.try_reserve("a", at));
        assert_eq!(refusal.reason, RefusalReason::TokenCeiling);
        assert_eq!(refusal.retry_after, PEER_WINDOW - Duration::from_secs(600));
        assert_eq!(limits.window_of("a", at).unwrap().tokens, 400);

        let rolled = start + PEER_WINDOW;
        assert!(limits.try_reserve("a", rolled).is_ok());
        assert_eq!(limits.window_of("a", rolled).unwrap().tokens, 0);
    }

    #[test]
    fn cost_ceiling_is_off_at_zero_and_ignores_unpriced_debits() {
        let start = now();
        let off = default_limits();
        off.debit("a", 1, Some(999.0), start);
        assert!(off.try_reserve("a", start).is_ok());

        let priced = limits(PeerLimitConfig {
            cost_usd_per_hour: 0.5,
            ..PeerLimitConfig::default()
        });
        priced.debit("a", 1, None, start);
        priced.debit("a", 1, None, start);
        assert!(priced.try_reserve("a", start).is_ok());
        priced.debit("a", 1, Some(0.25), start);
        assert!(priced.try_reserve("a", start).is_ok());
        priced.debit("a", 1, Some(0.25), start);
        let refusal = refusal_of(priced.try_reserve("a", start));
        assert_eq!(refusal.reason, RefusalReason::CostCeiling);
        assert_eq!(refusal.retry_after, PEER_WINDOW);
        assert_eq!(priced.window_of("a", start).unwrap().cost_usd, 0.5);

        let rolled = start + PEER_WINDOW;
        assert!(priced.try_reserve("a", rolled).is_ok());
        assert_eq!(priced.window_of("a", rolled).unwrap().cost_usd, 0.0);
    }

    #[test]
    fn debit_charges_at_least_one_token_and_only_a_positive_finite_cost() {
        let limits = default_limits();
        let start = now();
        limits.debit("a", 0, Some(0.0), start);
        limits.debit("a", 0, Some(-1.0), start);
        limits.debit("a", 0, Some(f64::NAN), start);
        limits.debit("a", 0, Some(f64::INFINITY), start);
        assert_eq!(
            limits.window_of("a", start),
            Some(WindowView {
                messages: 0,
                tokens: 4,
                cost_usd: 0.0,
                in_flight: 0,
            })
        );
        limits.debit("a", 5, Some(0.25), start);
        let window = limits.window_of("a", start).unwrap();
        assert_eq!((window.tokens, window.cost_usd), (9, 0.25));
    }

    #[test]
    fn concurrency_is_checked_before_either_ceiling() {
        let limits = limits(PeerLimitConfig {
            tokens_per_hour: 1,
            cost_usd_per_hour: 0.01,
            ..PeerLimitConfig::default()
        });
        let start = now();
        let held = limits.try_reserve("a", start).unwrap();
        limits.debit("a", 5, Some(1.0), start);
        assert_eq!(
            reason_of(limits.try_reserve("a", start)),
            RefusalReason::PeerConcurrency
        );
        drop(held);
        assert_eq!(
            reason_of(limits.try_reserve("a", start)),
            RefusalReason::TokenCeiling
        );
    }

    #[test]
    fn release_of_an_unknown_identity_creates_nothing() {
        let limits = default_limits();
        limits.release("ghost");
        assert_eq!(limits.window_of("ghost", now()), None);
    }

    #[test]
    fn note_refusal_surfaces_once_per_reason_per_identity_per_window() {
        let limits = default_limits();
        let start = now();
        let first = limits.note_refusal("a", RefusalReason::RateLimited, start);
        assert_eq!(
            first,
            FoldNotice {
                surface: true,
                reports: Vec::new()
            }
        );
        for n in 1..5 {
            let folded = limits.note_refusal(
                "a",
                RefusalReason::RateLimited,
                start + Duration::from_secs(n),
            );
            assert_eq!(folded, FoldNotice::default(), "refusal {n}");
        }
        assert!(
            limits
                .note_refusal("a", RefusalReason::TokenCeiling, start)
                .surface
        );
        assert!(
            !limits
                .note_refusal("a", RefusalReason::TokenCeiling, start)
                .surface
        );
        assert!(
            limits
                .note_refusal("b", RefusalReason::RateLimited, start)
                .surface
        );

        let rolled = start + PEER_WINDOW;
        let notice = limits.note_refusal("a", RefusalReason::RateLimited, rolled);
        assert!(notice.surface);
        assert_eq!(
            notice.reports,
            vec![
                (RefusalReason::RateLimited, 4),
                (RefusalReason::TokenCeiling, 1),
            ]
        );
        assert_eq!(
            limits.note_refusal("a", RefusalReason::RateLimited, rolled),
            FoldNotice::default()
        );
        assert!(limits.take_fold_reports("a", rolled).is_empty());
        assert!(limits.take_fold_reports("b", rolled).is_empty());
    }

    #[test]
    fn claim_peer_reply_is_true_once_per_identity_per_reason_per_window_apart_from_the_fold() {
        let limits = default_limits();
        let start = now();
        assert!(limits.claim_peer_reply("a", RefusalReason::RateLimited, start));
        for n in 1..4 {
            assert!(
                !limits.claim_peer_reply(
                    "a",
                    RefusalReason::RateLimited,
                    start + Duration::from_secs(n)
                ),
                "claim {n}"
            );
        }
        assert!(limits.claim_peer_reply("a", RefusalReason::TokenCeiling, start));
        assert!(limits.claim_peer_reply("b", RefusalReason::RateLimited, start));

        assert!(
            limits
                .note_refusal("a", RefusalReason::EnvoyBusy, start)
                .surface
        );
        assert!(limits.claim_peer_reply("a", RefusalReason::EnvoyBusy, start));
        assert!(limits.claim_peer_reply("c", RefusalReason::EnvoyBusy, start));
        assert!(
            limits
                .note_refusal("c", RefusalReason::EnvoyBusy, start)
                .surface
        );

        let rolled = start + PEER_WINDOW;
        assert!(limits.claim_peer_reply("a", RefusalReason::RateLimited, rolled));
        assert!(!limits.claim_peer_reply("a", RefusalReason::RateLimited, rolled));
        assert_eq!(
            limits.note_refusal("a", RefusalReason::RateLimited, rolled),
            FoldNotice {
                surface: true,
                reports: Vec::new()
            },
            "claims neither surface nor fold"
        );
    }

    #[test]
    fn take_fold_reports_drains_rollover_counts_exactly_once() {
        let limits = default_limits();
        let start = now();
        for _ in 0..3 {
            limits.note_refusal("a", RefusalReason::EnvoyBusy, start);
        }
        assert!(limits.take_fold_reports("a", start).is_empty());

        let rolled = start + PEER_WINDOW;
        assert_eq!(
            limits.take_fold_reports("a", rolled),
            vec![(RefusalReason::EnvoyBusy, 2)]
        );
        assert!(limits.take_fold_reports("a", rolled).is_empty());
        assert!(
            limits
                .note_refusal("a", RefusalReason::EnvoyBusy, rolled)
                .surface
        );
    }

    #[test]
    fn the_cap_evicts_the_least_recently_seen_idle_identity() {
        let limits = default_limits();
        let start = now();
        for n in 0..PEER_LIMITS_MAX_IDENTITIES {
            let at = start + Duration::from_secs(n as u64);
            assert!(limits.admit_message(&format!("id-{n}"), at).is_ok());
        }
        let at = start + Duration::from_secs(PEER_LIMITS_MAX_IDENTITIES as u64);
        assert!(limits.admit_message("newcomer", at).is_ok());
        assert_eq!(limits.window_of("id-0", at), None);
        assert!(limits.window_of("id-1", at).is_some());
        assert_eq!(limits.window_of("newcomer", at).unwrap().messages, 1);
    }

    #[test]
    fn the_cap_skips_identities_in_flight_and_refuses_when_all_are() {
        let limits = limits(PeerLimitConfig {
            concurrency: 2,
            ..PeerLimitConfig::default()
        });
        let start = now();
        let mut held: Vec<Reservation> = (0..PEER_LIMITS_MAX_IDENTITIES)
            .map(|n| {
                let at = start + Duration::from_secs(n as u64);
                limits.try_reserve(&format!("id-{n}"), at).unwrap()
            })
            .collect();
        let at = start + Duration::from_secs(PEER_LIMITS_MAX_IDENTITIES as u64);
        let refusal = limits.admit_message("newcomer", at).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::PeerConcurrency);
        assert_eq!(refusal.retry_after, PEER_RETRY_AFTER_CAPACITY);
        assert_eq!(
            reason_of(limits.try_reserve("newcomer", at)),
            RefusalReason::PeerConcurrency
        );
        assert_eq!(limits.window_of("newcomer", at), None);
        assert_eq!(
            limits.note_refusal("newcomer", RefusalReason::PeerConcurrency, at),
            FoldNotice {
                surface: true,
                reports: Vec::new()
            }
        );
        assert!(limits.take_fold_reports("newcomer", at).is_empty());

        drop(held.remove(7));
        assert!(limits.admit_message("newcomer", at).is_ok());
        assert_eq!(limits.window_of("id-7", at), None);
        assert!(limits.window_of("id-0", at).is_some());
    }

    #[test]
    fn window_of_reports_the_current_window_only_for_known_identities() {
        let limits = default_limits();
        let start = now();
        assert_eq!(limits.window_of("a", start), None);
        assert!(limits.admit_message("a", start).is_ok());
        limits.debit("a", 10, Some(0.1), start);
        let _held = limits.try_reserve("a", start).unwrap();
        assert_eq!(
            limits.window_of("a", start),
            Some(WindowView {
                messages: 1,
                tokens: 10,
                cost_usd: 0.1,
                in_flight: 1,
            })
        );
        assert_eq!(
            limits.window_of("a", start + PEER_WINDOW),
            Some(WindowView {
                messages: 0,
                tokens: 0,
                cost_usd: 0.0,
                in_flight: 1,
            })
        );
    }
}
