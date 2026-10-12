//! Runs mesh work between REPL turns, while the editor sits in `read_line` and nothing
//! holds the session context. Human lines go out through the mesh slot's notifier; model
//! notes go into whatever notification queue the context holds at the moment of delivery.
//!
//! Ownership rule: the driver holds the shared lock, never clones of what is behind it.
//! `use_agent` and `exit_agent` replace the notification queue and the supervisor, so a
//! clone taken at start-up would deliver into a queue nobody drains. Every touch is a
//! `try_read`/`try_write` inside a synchronous block that ends before the next `.await`:
//! the blocking forms would park a tokio worker for the length of a turn, and a guard held
//! across a model call would stall the user's next line.
//!
//! The driver also runs the automatic propagation fetch: once when the node joins the
//! slot and then every `mesh.propagation_sync_interval_secs`, on its own task since a
//! fetch may wait minutes on the node. `stop` gives an in-flight fetch a grace period and
//! then aborts it. An abort that lands inside the fetch's body loop skips the
//! persist of its dedup store, so the node keeps holding those bodies and the next fetch
//! hands some of them to the sink again: the sink is at-least-once, and nothing is lost.

use crate::config::{AppState, RequestContext};
use crate::mesh::idle::{
    Coalescer, IDLE_COALESCE_TICK, IDLE_QUEUE_CAPACITY, IdleNotify, IdleSink, Origin, RateLimiter,
    SyncSchedule,
};
use crate::mesh::notify::{Notification, Source};
use crate::mesh::{FetchError, FetchReport, LoggingInboundSink, redact_hashes, short};
use crate::supervisor::notification::{
    MESH_NOTIFICATION_QUEUE_CAPACITY, SystemNotification, mesh_events_dropped,
};

use log::{debug, warn};
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::{JoinError, JoinHandle, JoinSet};
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;

/// Backoff between lock attempts while a turn holds the context write-locked.
pub(crate) const IDLE_LOCK_RETRY_TICK: Duration = Duration::from_millis(50);
/// How long `stop` waits for a propagation fetch in flight before aborting it.
pub(crate) const IDLE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Model notes held back while a turn owns the context. One short of the queue's own
/// mesh cap so the held notes fit in an empty queue without evicting each other; the
/// drop summary that follows them is pushed outside the cap.
pub(crate) const IDLE_PENDING_NOTES_MAX: usize = MESH_NOTIFICATION_QUEUE_CAPACITY - 1;

pub(crate) enum IdleEvent {
    Notify(IdleNotify),
    /// The node joined the slot: fetch what a propagation node holds for it.
    SyncNow,
}

/// Producer side of the driver's queue. Never waits: a full or closed queue hands the
/// event back, and only a full one counts as overflow. A sync request the full queue
/// refuses is left as a flag the loop reads whenever it wakes, so it is not lost; one
/// that gets into the queue is delivered once, by the queue alone.
#[derive(Clone)]
pub(crate) struct IdleHandle {
    tx: mpsc::Sender<IdleEvent>,
    overflow: Arc<AtomicUsize>,
    sync_requested: Arc<AtomicBool>,
}

impl IdleHandle {
    fn new(tx: mpsc::Sender<IdleEvent>) -> Self {
        Self {
            tx,
            overflow: Arc::new(AtomicUsize::new(0)),
            sync_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    fn send(&self, event: IdleEvent) -> Result<(), IdleEvent> {
        self.tx.try_send(event).map_err(|err| {
            if matches!(err, TrySendError::Full(_)) {
                self.overflow.fetch_add(1, Ordering::AcqRel);
            }
            err.into_inner()
        })
    }

    pub(crate) fn overflow(&self) -> usize {
        self.overflow.load(Ordering::Acquire)
    }
}

impl IdleSink for IdleHandle {
    fn push(&self, note: IdleNotify) -> Result<(), IdleNotify> {
        match self.send(IdleEvent::Notify(note)) {
            Ok(()) => Ok(()),
            Err(IdleEvent::Notify(note)) => Err(note),
            Err(IdleEvent::SyncNow) => {
                unreachable!("try_send hands back the event it was given")
            }
        }
    }

    fn request_sync(&self) {
        match self.tx.try_send(IdleEvent::SyncNow) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.sync_requested.store(true, Ordering::Release);
                debug!(
                    "Idle driver queue full; the propagation sync runs when the driver next wakes"
                );
            }
            Err(TrySendError::Closed(_)) => {
                debug!("Idle driver stopped; the propagation sync waits for the next install");
            }
        }
    }
}

pub(crate) struct IdleDriver {
    handle: IdleHandle,
    cancel: CancellationToken,
    tasks: JoinSet<Option<SyncTask>>,
    app: Arc<AppState>,
}

impl IdleDriver {
    /// Installs the driver as the mesh slot's idle sink and starts its loop. Must be called
    /// from inside the runtime.
    pub(crate) fn start(ctx: Arc<RwLock<RequestContext>>, app: Arc<AppState>) -> Self {
        let (tx, rx) = mpsc::channel(IDLE_QUEUE_CAPACITY);
        let handle = IdleHandle::new(tx);
        app.mesh
            .set_idle(Arc::new(handle.clone()) as Arc<dyn IdleSink>);
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(
            DriverLoop::new(
                ctx,
                Arc::clone(&app),
                rx,
                Arc::clone(&handle.overflow),
                Arc::clone(&handle.sync_requested),
                cancel.clone(),
            )
            .run(),
        );
        Self {
            handle,
            cancel,
            tasks,
            app,
        }
    }

    #[cfg(test)]
    pub(crate) fn handle(&self) -> IdleHandle {
        self.handle.clone()
    }

    pub(crate) async fn stop(self) {
        self.stop_with_timeout(IDLE_STOP_TIMEOUT).await
    }

    /// Detaches from the slot, stops the loop and waits for a propagation fetch in
    /// flight. A fetch still waiting on its node after `timeout` is aborted outright.
    /// Returns only once nothing the driver started is still running.
    pub(crate) async fn stop_with_timeout(mut self, timeout: Duration) {
        self.app.mesh.clear_idle();
        self.cancel.cancel();
        let mut sync = None;
        while let Some(joined) = self.tasks.join_next().await {
            match joined {
                Ok(in_flight_sync) => sync = in_flight_sync,
                Err(join_error) => warn!("Idle driver loop did not exit cleanly: {join_error}"),
            }
        }
        if let Some(mut sync) = sync
            && tokio::time::timeout(timeout, &mut sync).await.is_err()
        {
            sync.abort();
            let _ = sync.await;
        }
        let overflow = self.handle.overflow();
        if overflow > 0 {
            warn!("Idle driver dropped {overflow} mesh event(s) on a full queue");
        }
    }
}

/// The synchronous part of `stop`, so an early return from the REPL that never reaches
/// `stop` still detaches the slot and takes the loop down with the fetch it owns
/// (`DriverLoop::drop` aborts that). Every step is idempotent, so running again after
/// `stop_with_timeout` changes nothing.
impl Drop for IdleDriver {
    fn drop(&mut self) {
        self.app.mesh.clear_idle();
        self.cancel.cancel();
        self.tasks.abort_all();
    }
}

struct DriverLoop {
    ctx: Arc<RwLock<RequestContext>>,
    app: Arc<AppState>,
    rx: mpsc::Receiver<IdleEvent>,
    /// Shared with the producers' handle; read and reset at each coalesce tick so the
    /// prompt hears about a full queue while the driver runs.
    overflow: Arc<AtomicUsize>,
    /// Shared with the producers' handle; set by a sync request the full queue refused
    /// and read at each wake, so that request still starts a fetch.
    sync_requested: Arc<AtomicBool>,
    cancel: CancellationToken,
    rate_limiter: RateLimiter,
    coalescer: Coalescer,
    /// Model notes wait here while a turn holds the context, and are retried every
    /// `IDLE_LOCK_RETRY_TICK`. Notes past `IDLE_PENDING_NOTES_MAX` push out the oldest,
    /// which is counted and reported with the next flush. Whatever is still waiting when
    /// the driver stops is dropped with it.
    pending_model_notes: VecDeque<SystemNotification>,
    dropped_model_notes: usize,
    coalesce_at: Option<tokio::time::Instant>,
    retry_at: Option<tokio::time::Instant>,
    /// Built from the interval and `announce` in the config the REPL started with:
    /// `update_app_config` replaces `ctx.app`, but the driver keeps the `AppState` it was
    /// started on, so the schedule reflects config.yaml as loaded at start.
    sync: SyncSchedule,
    sync_task: Option<SyncTask>,
    /// Whether the wait for a propagation node has been logged since the last fetch
    /// started, so a mesh without one says so once per streak rather than every retry.
    no_node_logged: bool,
}

type SyncTask = JoinHandle<Result<FetchReport, FetchError>>;

/// The slot must not outlive the loop that drains it: a loop that panics or is aborted
/// would otherwise leave every later event queued for nobody. Idempotent with `stop`
/// and `IdleDriver::drop`, which clear the slot first. A fetch still in flight is
/// aborted with the loop that would have reported it.
impl Drop for DriverLoop {
    fn drop(&mut self) {
        self.app.mesh.clear_idle();
        if let Some(task) = &self.sync_task {
            task.abort();
        }
    }
}

impl DriverLoop {
    fn new(
        ctx: Arc<RwLock<RequestContext>>,
        app: Arc<AppState>,
        rx: mpsc::Receiver<IdleEvent>,
        overflow: Arc<AtomicUsize>,
        sync_requested: Arc<AtomicBool>,
        cancel: CancellationToken,
    ) -> Self {
        let sync = SyncSchedule::new(
            app.config.mesh.propagation_sync_interval_secs,
            app.config.mesh.announce,
        );
        Self {
            ctx,
            app,
            rx,
            overflow,
            sync_requested,
            cancel,
            rate_limiter: RateLimiter::new(),
            coalescer: Coalescer::default(),
            pending_model_notes: VecDeque::new(),
            dropped_model_notes: 0,
            coalesce_at: None,
            retry_at: None,
            sync,
            sync_task: None,
            no_node_logged: false,
        }
    }

    /// Runs until cancelled or the producers are gone; hands back the propagation fetch
    /// still in flight, if any, for `stop` to wait on.
    async fn run(mut self) -> Option<SyncTask> {
        loop {
            if self.sync_requested.swap(false, Ordering::AcqRel) {
                self.on_sync_request();
            }
            let coalesce_at = self.coalesce_at.unwrap_or_else(tokio::time::Instant::now);
            let retry_at = self.retry_at.unwrap_or_else(tokio::time::Instant::now);
            let sync_at = self
                .sync
                .due_at()
                .map_or_else(tokio::time::Instant::now, tokio::time::Instant::from_std);
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                event = self.rx.recv() => match event {
                    Some(IdleEvent::Notify(note)) => self.on_notify(note),
                    Some(IdleEvent::SyncNow) => self.on_sync_request(),
                    None => break,
                },
                _ = sleep_until(coalesce_at), if self.coalesce_at.is_some() => {
                    self.coalesce_at = None;
                    self.flush_summaries();
                }
                _ = sleep_until(retry_at), if self.retry_at.is_some() => {
                    self.retry_at = None;
                }
                _ = sleep_until(sync_at), if self.sync.due_at().is_some() => {
                    self.on_sync_request();
                }
                joined = sync_outcome(&mut self.sync_task) => {
                    self.sync_task = None;
                    self.on_sync_finished(joined);
                }
            }
            self.flush_ctx_work();
            self.arm_timers();
        }
        self.sync_task.take()
    }

    /// Local notes always go through, as do peer notes from a source bounded upstream.
    /// Other peer notes spend a token: admitted ones go to the prompt at once and queue
    /// their model copy; rejected ones are folded into the next summary line and their
    /// model copy is dropped, since a flood must not reach the transcript any more than
    /// the prompt.
    fn on_notify(&mut self, note: IdleNotify) {
        let IdleNotify {
            source,
            text,
            origin,
            model_note,
        } = note;
        match origin {
            Origin::Local => {
                self.deliver(source, text, model_note.map(|note| *note));
            }
            Origin::Peer(peer) => {
                let now = tokio::time::Instant::now().into_std();
                if exempt_from_folding(source) || self.rate_limiter.admit(source, now) {
                    self.deliver(source, text, model_note.map(|note| *note));
                } else {
                    self.coalescer.fold(&peer);
                }
            }
        }
    }

    fn deliver(&mut self, source: Source, text: String, model_note: Option<SystemNotification>) {
        self.app.mesh.notify(Notification::new(source, text));
        if let Some(model_note) = model_note {
            if self.pending_model_notes.len() >= IDLE_PENDING_NOTES_MAX
                && self.pending_model_notes.pop_front().is_some()
            {
                self.dropped_model_notes += 1;
            }
            self.pending_model_notes.push_back(model_note);
        }
    }

    fn flush_summaries(&mut self) {
        for line in self.coalescer.flush() {
            self.app.mesh.notify(line);
        }
        let overflow = self.overflow.swap(0, Ordering::AcqRel);
        if overflow > 0 {
            warn!("Idle driver dropped {overflow} mesh event(s) on a full queue");
            self.app.mesh.notify(Notification::new(
                Source::Mesh,
                format!("({overflow} events dropped on a full queue)"),
            ));
        }
    }

    /// Timers run only while there is something for them to do, and a running one is
    /// left alone so a steady trickle of events cannot keep pushing it back. The sync
    /// timer is the exception: an automatic schedule with nothing armed while a node is
    /// installed has lost its request, and is given the no-node retry.
    fn arm_timers(&mut self) {
        let now = tokio::time::Instant::now();
        let summaries_pending =
            !self.coalescer.is_empty() || self.overflow.load(Ordering::Acquire) > 0;
        if summaries_pending && self.coalesce_at.is_none() {
            self.coalesce_at = Some(now + IDLE_COALESCE_TICK);
        }
        if !self.pending_model_notes.is_empty() && self.retry_at.is_none() {
            self.retry_at = Some(now + IDLE_LOCK_RETRY_TICK);
        }
        if self.app.mesh.get().is_some() {
            self.sync.heal(now.into_std());
        }
    }

    /// Everything that needs the context, done under one short guard that this synchronous
    /// block drops before the loop awaits again. Notes need only the read side, since the
    /// queue's `push` takes `&self`.
    fn flush_ctx_work(&mut self) {
        let ctx = Arc::clone(&self.ctx);
        if !self.pending_model_notes.is_empty()
            && let Some(guard) = ctx.try_read()
        {
            self.flush_model_notes(&guard);
        }
    }

    fn flush_model_notes(&mut self, ctx: &RequestContext) {
        for note in self.pending_model_notes.drain(..) {
            ctx.notification_queue.push_mesh(note);
        }
        if self.dropped_model_notes > 0 {
            let dropped = std::mem::take(&mut self.dropped_model_notes);
            // The summary is the model's only word about the notes it stands for, so it
            // goes in outside the mesh cap: pushed under it, it could evict one more
            // genuine event and need a summary of its own.
            ctx.notification_queue.push(mesh_events_dropped(
                dropped,
                "idle-driver",
                "while a turn held the context",
            ));
        }
    }

    /// Starts a propagation fetch when the schedule allows one and there is a node to
    /// ask.
    fn on_sync_request(&mut self) {
        self.sync_requested.store(false, Ordering::Release);
        if !self.sync.accepts_request() {
            return;
        }
        let now = tokio::time::Instant::now().into_std();
        let Some(runtime) = self.app.mesh.get() else {
            self.sync.mesh_off();
            self.no_node_logged = false;
            return;
        };
        let Ok(node) = runtime.propagation_nodes().select() else {
            if !self.no_node_logged {
                debug!("Propagation sync waits for a propagation node to announce itself");
                self.no_node_logged = true;
            }
            self.sync.no_node(now);
            return;
        };
        let node = node.destination.address_hash.to_hex_string();
        debug!("Propagation sync from {} starting", short(&node));
        self.no_node_logged = false;
        self.sync.started(node);
        self.sync_task = Some(tokio::spawn(async move {
            runtime.fetch_propagated(&LoggingInboundSink).await
        }));
    }

    /// A fetch that ended after `.mesh off` emptied the slot arms nothing: the schedule is
    /// stopped before the outcome reaches it.
    fn on_sync_finished(&mut self, joined: Result<Result<FetchReport, FetchError>, JoinError>) {
        let now = tokio::time::Instant::now().into_std();
        let outcome = match joined {
            Ok(outcome) => outcome,
            Err(join_error) => {
                warn!("Propagation sync task did not finish: {join_error}");
                Err(FetchError::Cancelled)
            }
        };
        match &outcome {
            Ok(report) => debug!(
                "Propagation sync from {} done: {} listed, {} wanted, {} received, {} delivered, {} duplicates, {} discarded, {} deferred",
                short(&report.node),
                report.listed,
                report.wanted,
                report.received,
                report.delivered,
                report.duplicates,
                report.discarded,
                report.deferred
            ),
            Err(err) => debug!(
                "Propagation sync failed: {}",
                redact_hashes(&err.to_string())
            ),
        }
        if self.app.mesh.get().is_none() {
            self.sync.mesh_off();
            self.no_node_logged = false;
        }
        if let Some(text) = self.sync.finished(&outcome, now) {
            self.app
                .mesh
                .notify(Notification::new(Source::Propagation, text));
        }
    }
}

/// Resolves when the fetch in flight ends; pending forever while there is none, so the
/// loop's `select!` needs no precondition that would leave an unpolled handle behind.
async fn sync_outcome(
    task: &mut Option<SyncTask>,
) -> Result<Result<FetchReport, FetchError>, JoinError> {
    match task {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

/// Knock, reply and access lines are never folded into a summary: each is bounded
/// upstream and carries something a fold would lose, a line the peer cannot earn again.
/// A knock is admitted once per identity per process, only for an identity already
/// trusted at the identity tier (`KnockGate::admit` drops unknown ones), and its line
/// carries the full hash the `.mesh trust` hint needs. A reply answers a question this
/// node asked, so replies are bounded by our own asks. An access request is capped per
/// identity (`ACCESS_MAX_PENDING_PER_IDENTITY`) and its line carries the id the grant
/// verb needs.
fn exempt_from_folding(source: Source) -> bool {
    matches!(source, Source::Knock | Source::Reply | Source::Access)
}

/// Fixtures for tests outside this module that need a driver on a slot without naming
/// the session context themselves, such as the mesh conformance suite. That suite's
/// `interop` module is `#[cfg(unix)]`, so the fixture follows it.
#[cfg(all(test, unix))]
pub(crate) mod testing {
    use super::IdleDriver;
    use crate::config::{AppState, RequestContext, WorkingMode};

    use parking_lot::RwLock;
    use std::sync::Arc;

    /// A driver on a fresh top-level REPL context; the returned state's `mesh` slot is
    /// the one it watches.
    pub(crate) fn driver_on_a_fresh_state() -> (Arc<AppState>, IdleDriver) {
        let app = Arc::new(AppState::test_default());
        let ctx = RequestContext::new(Arc::clone(&app), WorkingMode::Repl);
        let driver = IdleDriver::start(Arc::new(RwLock::new(ctx)), Arc::clone(&app));
        (app, driver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::WorkingMode;
    use crate::function::drain_live_notifications;
    use crate::mesh::idle::{
        IDLE_COALESCE_MAX_PEERS, IDLE_NOTIFY_BURST, IDLE_NOTIFY_REFILL_INTERVAL,
    };
    use crate::mesh::notify::{NotificationSink, RenderedNotification};
    use crate::supervisor::Supervisor;
    use crate::supervisor::notification::{
        Channel, MESH_EVENTS_DROPPED_EVENT, NotificationQueue, mesh_notification,
    };
    use std::sync::mpsc as std_mpsc;
    use std::thread;
    use tokio::time::sleep;

    /// Ceiling on any one wait; the driver reacts within a lock-retry tick when healthy.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(10);

    #[derive(Default)]
    struct RecordingSink(parking_lot::Mutex<Vec<RenderedNotification>>);

    impl NotificationSink for RecordingSink {
        fn notify(&self, rendered: RenderedNotification) {
            self.0.lock().push(rendered);
        }
    }

    impl RecordingSink {
        fn lines(&self) -> Vec<String> {
            self.0
                .lock()
                .iter()
                .flat_map(|rendered| rendered.lines().to_vec())
                .collect()
        }
    }

    fn test_ctx(supervisor: Option<Supervisor>) -> Arc<RwLock<RequestContext>> {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.supervisor = supervisor.map(|sup| Arc::new(RwLock::new(sup)));
        Arc::new(RwLock::new(ctx))
    }

    fn start_driver(ctx: &Arc<RwLock<RequestContext>>) -> IdleDriver {
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        IdleDriver::start(Arc::clone(ctx), app)
    }

    fn install_sink(ctx: &Arc<RwLock<RequestContext>>) -> Arc<RecordingSink> {
        let sink = Arc::new(RecordingSink::default());
        ctx.try_read()
            .unwrap()
            .app
            .mesh
            .set_notifier(Arc::clone(&sink) as Arc<dyn NotificationSink>);
        sink
    }

    async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + TEST_TIMEOUT;
        while !condition() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            sleep(POLL).await;
        }
    }

    /// Polls the context's queue through `try_read`, never the blocking `read`, and
    /// returns the first drained note matching `pick`.
    async fn wait_for_note(
        ctx: &Arc<RwLock<RequestContext>>,
        mut pick: impl FnMut(&SystemNotification) -> bool,
    ) -> SystemNotification {
        let mut found = None;
        wait_until("a model note in the context's queue", || {
            if let Some(guard) = ctx.try_read() {
                found = guard
                    .notification_queue
                    .drain()
                    .into_iter()
                    .find(|note| pick(note));
            }
            found.is_some()
        })
        .await;
        found.unwrap()
    }

    fn note(text: &str, model_note: Option<SystemNotification>) -> IdleNotify {
        IdleNotify {
            source: Source::Mesh,
            text: text.to_string(),
            origin: Origin::Local,
            model_note: model_note.map(Box::new),
        }
    }

    fn peer_note(peer: &str, text: &str, model_note: Option<SystemNotification>) -> IdleNotify {
        IdleNotify {
            source: Source::Message,
            text: text.to_string(),
            origin: Origin::Peer(peer.to_string()),
            model_note: model_note.map(Box::new),
        }
    }

    fn model_note(id: &str) -> SystemNotification {
        mesh_notification("mesh_message", id, "peer", true, "read it".into())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_note_lands_in_the_queue_installed_after_an_agent_switch() {
        let ctx = test_ctx(None);
        let driver = start_driver(&ctx);

        driver
            .handle()
            .push(note("first", Some(model_note("first"))))
            .expect("queued");
        let first = wait_for_note(&ctx, |note| note.id == "first").await;
        assert_eq!(first.channel, Channel::Mesh);

        // What `use_agent` and `exit_agent` do to the context mid-session.
        let old_queue = {
            let mut guard = ctx.try_write().unwrap();
            let old = Arc::clone(&guard.notification_queue);
            guard.notification_queue = Arc::new(NotificationQueue::new());
            old
        };

        driver
            .handle()
            .push(note("second", Some(model_note("second"))))
            .expect("queued");
        let second = wait_for_note(&ctx, |note| note.id == "second").await;
        assert_eq!(second.id, "second");
        assert!(
            old_queue.drain().is_empty(),
            "the note must not land in the queue that was replaced"
        );
        driver.stop().await;
    }

    /// Spawns a thread that takes and holds the context write lock, the way the REPL does
    /// for a whole turn. Returns once the lock is held; dropping the sender releases it.
    fn hold_write_lock(
        ctx: &Arc<RwLock<RequestContext>>,
    ) -> (std_mpsc::Sender<()>, thread::JoinHandle<()>) {
        let (locked_tx, locked_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let holder = {
            let ctx = Arc::clone(ctx);
            thread::spawn(move || {
                let _guard = ctx.write();
                locked_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
        };
        locked_rx.recv().unwrap();
        (release_tx, holder)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn model_notes_wait_while_a_turn_holds_the_write_lock_and_land_on_release() {
        let ctx = test_ctx(None);
        // The test may keep a clone of the queue; the driver may not.
        let queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let driver = start_driver(&ctx);
        let (release, holder) = hold_write_lock(&ctx);

        driver
            .handle()
            .push(note("during the turn", Some(model_note("turn"))))
            .expect("queued");
        sleep(IDLE_LOCK_RETRY_TICK * 3).await;
        assert!(
            queue.drain().is_empty(),
            "nothing may land while the turn holds the context"
        );
        assert!(
            ctx.try_read().is_none(),
            "the turn must still hold the lock"
        );

        drop(release);
        holder.join().unwrap();
        let mut landed = Vec::new();
        wait_until("the note to land after the turn", || {
            landed.extend(queue.drain());
            !landed.is_empty()
        })
        .await;
        assert_eq!(landed[0].id, "turn");
        driver.stop().await;
    }

    /// Notes held back during a turn are bounded: the oldest give way, and the model is
    /// told how many with the flush that follows.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_model_notes_are_capped_while_a_turn_holds_the_context() {
        const EXCESS: usize = 5;
        let ctx = test_ctx(None);
        let queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let (release, holder) = hold_write_lock(&ctx);

        let pushed = IDLE_PENDING_NOTES_MAX + EXCESS;
        for i in 0..pushed {
            driver
                .handle()
                .push(note(
                    &format!("note {i}"),
                    Some(model_note(&format!("n{i}"))),
                ))
                .expect("queued");
        }
        wait_until("every human line to reach the sink", || {
            sink.lines().len() == pushed
        })
        .await;

        drop(release);
        holder.join().unwrap();
        let mut landed = Vec::new();
        wait_until("the held notes to land", || {
            landed.extend(queue.drain());
            landed.len() > IDLE_PENDING_NOTES_MAX
        })
        .await;
        assert_eq!(landed.len(), IDLE_PENDING_NOTES_MAX + 1);
        let survivors: Vec<&str> = landed[..IDLE_PENDING_NOTES_MAX]
            .iter()
            .map(|note| note.id.as_str())
            .collect();
        let expected: Vec<String> = (EXCESS..pushed).map(|i| format!("n{i}")).collect();
        assert_eq!(survivors, expected, "the oldest notes give way");
        let summary = landed.last().unwrap();
        assert_eq!(summary.event, "mesh_events_dropped");
        assert_eq!(summary.tool_or_agent, "idle-driver");
        assert_eq!(summary.status, "success", "a drop summary is not a failure");
        assert_eq!(
            summary.next_action,
            format!("{EXCESS} older mesh events were dropped while a turn held the context")
        );
        driver.stop().await;
    }

    /// With one event already queued, the held notes fill the queue to exactly its mesh
    /// cap. A summary pushed under the cap would then evict the seed and earn the queue's
    /// own drop summary; pushed outside it, both the seed and the notes survive. The
    /// driver flushes under a read guard on the context, so draining only once the write
    /// side is free keeps the drain from splitting the flush, and a poll that finds only
    /// the seed puts it back so the flush still meets a queue one short of the cap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_drivers_held_notes_summary_is_pushed_outside_the_mesh_cap() {
        const EXCESS: usize = 2;
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let seed = || mesh_notification("seed", "seed", "test", true, "x".into());
        assert!(
            ctx.try_read()
                .unwrap()
                .notification_queue
                .push_mesh(seed())
                .is_none()
        );
        let (release, holder) = hold_write_lock(&ctx);

        let pushed = IDLE_PENDING_NOTES_MAX + EXCESS;
        for i in 0..pushed {
            driver
                .handle()
                .push(note(
                    &format!("note {i}"),
                    Some(model_note(&format!("n{i}"))),
                ))
                .expect("queued");
        }
        wait_until("every human line to reach the sink", || {
            sink.lines().len() == pushed
        })
        .await;

        drop(release);
        holder.join().unwrap();
        let mut landed = Vec::new();
        wait_until("the held notes to land", || {
            let Some(guard) = ctx.try_write() else {
                return false;
            };
            let drained = guard.notification_queue.drain();
            if drained.len() == 1 && drained[0].id == "seed" {
                guard.notification_queue.push_mesh(seed());
                return false;
            }
            landed.extend(drained);
            landed.len() > IDLE_PENDING_NOTES_MAX
        })
        .await;
        assert_eq!(landed.len(), 1 + IDLE_PENDING_NOTES_MAX + 1);
        assert_eq!(landed[0].id, "seed", "the seed was not evicted");
        let held = landed
            .iter()
            .filter(|note| note.event == "mesh_message")
            .count();
        assert_eq!(held, IDLE_PENDING_NOTES_MAX);
        let summaries: Vec<&str> = landed
            .iter()
            .filter(|note| note.event == MESH_EVENTS_DROPPED_EVENT)
            .map(|note| note.tool_or_agent.as_str())
            .collect();
        assert_eq!(
            summaries,
            ["idle-driver"],
            "a notification-queue summary would mean the driver's summary took the cap"
        );
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn top_level_mesh_note_survives_drain_live_notifications() {
        let ctx = test_ctx(None);
        let driver = start_driver(&ctx);

        driver
            .handle()
            .push(note("peer said hi", Some(model_note("deadbeef"))))
            .expect("queued");

        let mut live = Vec::new();
        wait_until("drain_live_notifications to return the note", || {
            if let Some(guard) = ctx.try_read() {
                assert!(guard.supervisor.is_none());
                live = drain_live_notifications(&guard);
            }
            !live.is_empty()
        })
        .await;
        assert_eq!(live.len(), 1);
        assert_eq!(live[0]["channel"], "mesh");
        assert_eq!(live[0]["id"], "deadbeef");
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_burst_from_one_peer_is_rate_limited_and_summarised_on_the_slot() {
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        for i in 0..100 {
            driver
                .handle()
                .push(peer_note("deadbeef", &format!("message {i}"), None))
                .expect("queued");
        }

        wait_until("the summary line", || {
            sink.lines()
                .last()
                .is_some_and(|line| line.contains("more from deadbeef)"))
        })
        .await;
        {
            let received = sink.0.lock();
            assert!(
                received.len() <= IDLE_NOTIFY_BURST as usize + 1,
                "{} notifications reached the sink",
                received.len()
            );
            assert_eq!(received[0].lines(), &["[mesh:message] message 0"]);
            let summary = &received.last().unwrap().lines()[0];
            let folded: usize = summary
                .strip_prefix("[mesh] (")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|count| count.parse().ok())
                .unwrap_or_else(|| panic!("{summary}"));
            let admitted = received.len() - 1;
            assert_eq!(
                admitted + folded,
                100,
                "every event is either shown or counted"
            );
        }
        driver.stop().await;
    }

    /// Crate-produced events are bounded by what the driver runs, so a burst of them
    /// past the peer bucket's capacity loses neither its lines nor its model notes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_notes_bypass_the_bucket_and_keep_their_model_notes() {
        let ctx = test_ctx(None);
        let queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let pushed = IDLE_NOTIFY_BURST as usize + 3;
        for i in 0..pushed {
            driver
                .handle()
                .push(note(
                    &format!("local {i}"),
                    Some(model_note(&format!("l{i}"))),
                ))
                .expect("queued");
        }

        let mut landed = Vec::new();
        wait_until("every model note to land", || {
            landed.extend(queue.drain());
            landed.len() == pushed
        })
        .await;
        let expected_lines: Vec<String> =
            (0..pushed).map(|i| format!("[mesh] local {i}")).collect();
        assert_eq!(sink.lines(), expected_lines);
        let expected_ids: Vec<String> = (0..pushed).map(|i| format!("l{i}")).collect();
        let landed_ids: Vec<String> = landed.into_iter().map(|note| note.id).collect();
        assert_eq!(landed_ids, expected_ids);
        driver.stop().await;
    }

    fn sourced_peer_note(source: Source, peer: &str, text: &str) -> IdleNotify {
        IdleNotify {
            source,
            ..peer_note(peer, text, None)
        }
    }

    fn fold_summaries(lines: &[String]) -> Vec<&String> {
        lines
            .iter()
            .filter(|line| line.starts_with("[mesh] (") && line.contains(" more from "))
            .collect()
    }

    /// Knocks are gated per identity upstream, replies by this node's own asks and access
    /// requests by the pending cap, so a burst of any past the bucket's capacity shows
    /// every line and folds none.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn knock_reply_and_access_lines_are_never_folded() {
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let per_source = IDLE_NOTIFY_BURST as usize + 3;
        let mut expected = Vec::new();
        for source in [Source::Knock, Source::Reply, Source::Access] {
            for i in 0..per_source {
                let peer = format!("{:08x}", i + 1);
                let text = format!("{source:?} {i}");
                driver
                    .handle()
                    .push(sourced_peer_note(source, &peer, &text))
                    .expect("queued");
                expected.push(format!("{} {text}", source.prefix()));
            }
        }

        wait_until("every line to reach the sink", || {
            sink.lines().len() >= expected.len()
        })
        .await;
        assert_eq!(sink.lines(), expected);
        sleep(IDLE_COALESCE_TICK * 2).await;
        let lines = sink.lines();
        assert!(
            fold_summaries(&lines).is_empty(),
            "a knock, reply or access line was folded: {lines:?}"
        );
        assert_eq!(lines, expected);
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn message_lines_still_fold_past_the_burst() {
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let pushed = IDLE_NOTIFY_BURST as usize + 3;
        for i in 0..pushed {
            driver
                .handle()
                .push(sourced_peer_note(
                    Source::Message,
                    "deadbeef",
                    &format!("message {i}"),
                ))
                .expect("queued");
        }

        wait_until("the fold summary", || {
            !fold_summaries(&sink.lines()).is_empty()
        })
        .await;
        let lines = sink.lines();
        let summaries = fold_summaries(&lines);
        assert_eq!(summaries.len(), 1, "{lines:?}");
        assert_eq!(summaries[0], "[mesh] (3 more from deadbeef)");
        let shown: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with("[mesh:message] "))
            .collect();
        assert_eq!(shown.len(), IDLE_NOTIFY_BURST as usize, "{lines:?}");
        assert_eq!(lines.len(), IDLE_NOTIFY_BURST as usize + 1);
        driver.stop().await;
    }

    /// An early return from the REPL drops the driver without `stop`: the slot must not
    /// keep pointing at the dead queue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_an_unstopped_driver_detaches_the_slot() {
        let ctx = test_ctx(None);
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        drop(driver);

        app.mesh.push_idle(note("after the drop", None));
        assert!(
            sink.lines().contains(&"[mesh] after the drop".to_string()),
            "with the slot cleared the line must reach the notifier directly: {:?}",
            sink.lines()
        );
    }

    /// The loop, not only the driver handle, owns the slot: a loop that panics or is
    /// aborted without `stop` must not leave the slot pointing at a queue nobody drains.
    #[test]
    fn a_dropped_driver_loop_clears_the_idle_slot() {
        let ctx = test_ctx(None);
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let sink = install_sink(&ctx);
        let (tx, rx) = mpsc::channel(IDLE_QUEUE_CAPACITY);
        let handle = IdleHandle::new(tx);
        app.mesh
            .set_idle(Arc::new(handle.clone()) as Arc<dyn IdleSink>);
        let driver_loop = DriverLoop::new(
            Arc::clone(&ctx),
            Arc::clone(&app),
            rx,
            Arc::clone(&handle.overflow),
            Arc::clone(&handle.sync_requested),
            CancellationToken::new(),
        );

        app.mesh.push_idle(note("before the drop", None));
        assert!(
            sink.lines().is_empty(),
            "with the slot set the line goes to the loop's queue: {:?}",
            sink.lines()
        );

        drop(driver_loop);
        app.mesh.push_idle(note("after the drop", None));
        assert_eq!(sink.lines(), vec!["[mesh] after the drop".to_string()]);
    }

    #[test]
    fn queue_overflow_is_counted_and_returns_the_note() {
        let (tx, _rx) = mpsc::channel(1);
        let handle = IdleHandle::new(tx);
        assert!(handle.push(note("fits", None)).is_ok());
        let returned = handle
            .push(note("overflows", None))
            .expect_err("a full queue hands the note back");
        assert_eq!(returned.text, "overflows");
        assert_eq!(handle.overflow(), 1);
    }

    /// A flood's model notes are folded with their human lines: a note reaches the queue
    /// exactly when its line reached the sink, and the summary line carries no note.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rate_limited_events_drop_their_model_notes_with_their_lines() {
        let ctx = test_ctx(None);
        let queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        for i in 0..100 {
            driver
                .handle()
                .push(peer_note(
                    "deadbeef",
                    &format!("message {i}"),
                    Some(model_note(&format!("m{i}"))),
                ))
                .expect("queued");
        }

        wait_until("the summary line", || {
            sink.lines()
                .iter()
                .any(|line| line.contains("more from deadbeef)"))
        })
        .await;
        // The notes travel through the lock separately from the lines; give them a tick.
        sleep(IDLE_LOCK_RETRY_TICK * 3).await;

        let admitted_lines: Vec<String> = sink
            .lines()
            .iter()
            .filter_map(|line| line.strip_prefix("[mesh:message] message "))
            .map(str::to_string)
            .collect();
        let landed_notes: Vec<String> = queue
            .drain()
            .into_iter()
            .map(|note| note.id.trim_start_matches('m').to_string())
            .collect();
        assert!(
            !admitted_lines.is_empty() && admitted_lines.len() <= IDLE_NOTIFY_BURST as usize + 1,
            "{} lines admitted",
            admitted_lines.len()
        );
        assert_eq!(
            landed_notes, admitted_lines,
            "a model note must land exactly when its human line was admitted"
        );
        driver.stop().await;
    }

    /// After `stop`, the driver's queue is closed: a late producer gets its event back,
    /// and since nothing was lost to a full queue the overflow counter stays put.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pushes_after_stop_are_handed_back_without_counting_as_overflow() {
        let ctx = test_ctx(None);
        let driver = start_driver(&ctx);
        let handle = driver.handle();
        driver.stop().await;

        let returned = handle
            .push(note("late", None))
            .expect_err("a stopped driver hands the note back");
        assert_eq!(returned.text, "late");
        assert_eq!(handle.overflow(), 0);
        assert!(
            ctx.try_read()
                .unwrap()
                .notification_queue
                .drain()
                .is_empty(),
            "nothing may land after stop"
        );
    }

    /// The one-line-per-flood summary is the bucket's "+1": it goes out even though the
    /// source that caused it has no tokens left, and each peer gets its own line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flood_from_two_peers_is_summarised_per_peer_past_an_empty_bucket() {
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        for peer in ["deadbeef", "cafef00d"] {
            for i in 0..50 {
                driver
                    .handle()
                    .push(peer_note(peer, &format!("{peer} {i}"), None))
                    .expect("queued");
            }
        }

        wait_until("both summary lines", || {
            let lines = sink.lines();
            lines
                .iter()
                .any(|line| line.contains("more from deadbeef)"))
                && lines
                    .iter()
                    .any(|line| line.contains("more from cafef00d)"))
        })
        .await;
        let lines = sink.lines();
        let summaries = lines
            .iter()
            .filter(|line| line.starts_with("[mesh] (") && line.contains(" more from "))
            .count();
        assert_eq!(summaries, 2, "one summary line per peer: {lines:?}");
        let folded: usize = lines
            .iter()
            .filter_map(|line| line.strip_prefix("[mesh] ("))
            .filter_map(|rest| rest.split(' ').next()?.parse::<usize>().ok())
            .sum();
        let admitted = lines.len() - summaries;
        assert_eq!(
            folded + admitted,
            100,
            "every event is either shown or counted"
        );
        driver.stop().await;
    }

    /// Flood control mutes, it does not ban. By the time the coalesce tick has reported
    /// the fold at least one refill interval has passed, so the peer's next line and its
    /// model note both go through again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_muted_peer_is_heard_again_once_its_bucket_refills() {
        assert!(
            IDLE_COALESCE_TICK >= IDLE_NOTIFY_REFILL_INTERVAL,
            "this test relies on the summary tick being no shorter than a refill"
        );
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        for i in 0..20 {
            driver
                .handle()
                .push(peer_note("deadbeef", &format!("message {i}"), None))
                .expect("queued");
        }
        wait_until("the summary line", || {
            sink.lines()
                .iter()
                .any(|line| line.contains("more from deadbeef)"))
        })
        .await;
        let lines = sink.lines();
        let summary = lines
            .iter()
            .find(|line| line.contains("more from deadbeef)"))
            .unwrap();
        let folded: usize = summary
            .strip_prefix("[mesh] (")
            .and_then(|rest| rest.split(' ').next())
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("{summary}"));
        let admitted = lines.len() - 1;
        assert_eq!(
            admitted + folded,
            20,
            "every event is either shown or counted: {lines:?}"
        );
        let before = lines.len();

        driver
            .handle()
            .push(peer_note(
                "deadbeef",
                "after the flood",
                Some(model_note("after")),
            ))
            .expect("queued");
        wait_until("the peer's next line", || {
            sink.lines()
                .iter()
                .any(|line| line == "[mesh:message] after the flood")
        })
        .await;
        assert_eq!(sink.lines().len(), before + 1, "{:?}", sink.lines());
        let note = wait_for_note(&ctx, |note| note.id == "after").await;
        assert_eq!(note.channel, Channel::Mesh);
        driver.stop().await;
    }

    /// Events lost to a full driver queue are reported at the prompt with the next
    /// summary tick, and the count starts over so each tick reports its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overflow_counted_while_running_is_reported_at_the_next_tick() {
        const DROPPED: usize = 3;
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        driver.handle.overflow.fetch_add(DROPPED, Ordering::AcqRel);
        driver
            .handle()
            .push(note("after the flood", None))
            .expect("queued");

        let expected = format!("[mesh] ({DROPPED} events dropped on a full queue)");
        wait_until("the overflow line", || {
            sink.lines().iter().any(|line| line == &expected)
        })
        .await;
        assert_eq!(driver.handle().overflow(), 0, "the tick resets the count");
        sleep(IDLE_COALESCE_TICK + IDLE_LOCK_RETRY_TICK).await;
        assert_eq!(
            sink.lines()
                .iter()
                .filter(|line| line.contains("dropped on a full queue"))
                .count(),
            1,
            "{:?}",
            sink.lines()
        );
        driver.stop().await;
    }

    /// A flood from more peers than the coalescer names produces at most
    /// `IDLE_COALESCE_MAX_PEERS + 1` summary lines per tick, admits exactly one burst past
    /// the shared bucket, and accounts for every event either shown or counted. Only the
    /// admitted burst reaches the model.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_flood_from_many_peers_is_capped_to_named_plus_one_summary_lines() {
        const PEERS: usize = IDLE_COALESCE_MAX_PEERS + 2;
        const PER_PEER: usize = 7;
        let ctx = test_ctx(None);
        let queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        for p in 0..PEERS {
            let peer = format!("{p:08x}");
            for i in 0..PER_PEER {
                driver
                    .handle()
                    .push(peer_note(
                        &peer,
                        &format!("{peer} {i}"),
                        Some(model_note(&peer)),
                    ))
                    .expect("queued");
            }
        }

        wait_until("the others summary line", || {
            sink.lines()
                .iter()
                .any(|line| line.contains(" other peers)"))
        })
        .await;
        // Give the tick a chance to emit anything beyond the cap before counting.
        sleep(IDLE_COALESCE_TICK / 4).await;

        let lines = sink.lines();
        let summaries: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with("[mesh] (") && line.contains(" more from "))
            .collect();
        assert!(
            summaries.len() <= IDLE_COALESCE_MAX_PEERS + 1,
            "summary lines per tick are capped at named + 1: {summaries:?}"
        );
        assert_eq!(
            summaries
                .iter()
                .filter(|line| line.contains(" other peers)"))
                .count(),
            1,
            "exactly one closing line for the peers past the named cap: {summaries:?}"
        );
        let admitted = lines.len() - summaries.len();
        assert_eq!(
            admitted, IDLE_NOTIFY_BURST as usize,
            "one burst is admitted past the shared bucket: {lines:?}"
        );
        let folded: usize = summaries
            .iter()
            .filter_map(|line| line.strip_prefix("[mesh] ("))
            .filter_map(|rest| rest.split(' ').next()?.parse::<usize>().ok())
            .sum();
        assert_eq!(
            folded + admitted,
            PEERS * PER_PEER,
            "every event is shown or counted"
        );

        let model_notes: Vec<String> = queue.drain().into_iter().map(|n| n.id).collect();
        assert_eq!(
            model_notes.len(),
            IDLE_NOTIFY_BURST as usize,
            "only admitted peer notes keep their model note: {model_notes:?}"
        );
        driver.stop().await;
    }

    /// The automatic propagation fetch against a fake node, driven from the slot the way
    /// `.mesh on` and `.mesh off` drive it. Unix-only with the loopback fixtures.
    #[cfg(unix)]
    mod sync {
        use super::*;
        use crate::config::{AppConfig, MeshConfig};
        use crate::mesh::MeshSlot;
        use crate::mesh::idle::PROPAGATION_SYNC_SHORT_RETRY;
        use crate::mesh::test_support::{FakeNode, StartedRuntime, started_runtime_on};
        use rmpv::Value;
        use rns_transport::iface::tcp_server::TcpServer;
        use tokio::time::timeout;

        /// Ceiling on one fetch over loopback: a link, an identify and a round.
        const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

        fn sync_ctx(interval_secs: u64) -> Arc<RwLock<RequestContext>> {
            ctx_for(MeshConfig {
                propagation_sync_interval_secs: interval_secs,
                ..MeshConfig::default()
            })
        }

        fn ctx_for(mesh: MeshConfig) -> Arc<RwLock<RequestContext>> {
            let mut app = AppState::test_default();
            app.config = Arc::new(AppConfig {
                mesh,
                ..AppConfig::default()
            });
            Arc::new(RwLock::new(RequestContext::new(
                Arc::new(app),
                WorkingMode::Cmd,
            )))
        }

        fn slot(ctx: &Arc<RwLock<RequestContext>>) -> Arc<MeshSlot> {
            Arc::clone(&ctx.try_read().unwrap().app.mesh)
        }

        /// A runtime joined to a fake node that has announced itself, with the node filed.
        async fn runtime_with_node(tag: &str) -> (FakeNode, StartedRuntime) {
            // A `MeshRuntime` joins with `TcpClient`'s default MTU, so the fake matches it.
            let fake = FakeNode::listen_with_mtu(TcpServer::DEFAULT_CLIENT_MTU).await;
            let started = started_runtime_on(tag, fake.listener.port).await;
            fake.announce().await;
            let runtime = started.runtime.clone();
            wait_until("the runtime to file the propagation node", || {
                runtime.propagation_nodes().select().is_ok()
            })
            .await;
            (fake, started)
        }

        async fn wait_for_rounds(fake: &FakeNode, count: usize, within: Duration) {
            let deadline = tokio::time::Instant::now() + within;
            while fake.script.seen().len() < count {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for {count} /get rounds; saw {}",
                    fake.script.seen().len()
                );
                sleep(POLL).await;
            }
        }

        fn propagation_lines(sink: &RecordingSink) -> Vec<String> {
            sink.lines()
                .into_iter()
                .filter(|line| line.starts_with("[mesh:propagation]"))
                .collect()
        }

        async fn tear_down(
            driver: IdleDriver,
            slot: &MeshSlot,
            started: StartedRuntime,
            fake: FakeNode,
        ) {
            driver.stop().await;
            slot.stop().await.unwrap();
            started.relay_handle.abort();
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn installing_the_node_starts_a_sync_and_the_interval_runs_the_next_one() {
            let ctx = sync_ctx(1);
            let sink = install_sink(&ctx);
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-interval").await;
            fake.script
                .reply_with([Value::Array(vec![]), Value::Array(vec![])]);

            slot(&ctx).install(started.runtime.clone()).unwrap();

            wait_for_rounds(&fake, 1, FETCH_TIMEOUT).await;
            wait_for_rounds(&fake, 2, FETCH_TIMEOUT).await;
            assert!(
                propagation_lines(&sink).is_empty(),
                "a fetch that received nothing says nothing: {:?}",
                sink.lines()
            );
            tear_down(driver, &slot(&ctx), started, fake).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_zero_interval_never_fetches_automatically() {
            let ctx = sync_ctx(0);
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-zero").await;

            slot(&ctx).install(started.runtime.clone()).unwrap();

            sleep(Duration::from_millis(1500)).await;
            assert!(
                fake.script.seen().is_empty(),
                "nothing is fetched on its own with a zero interval"
            );
            fake.script.reply_with([Value::Array(vec![])]);
            let report = timeout(
                FETCH_TIMEOUT,
                started.runtime.fetch_propagated(&LoggingInboundSink),
            )
            .await
            .expect("a manual fetch finishes within FETCH_TIMEOUT")
            .unwrap();
            assert_eq!(report.node, fake.hex());
            assert_eq!(fake.script.seen().len(), 1);
            tear_down(driver, &slot(&ctx), started, fake).await;
        }

        /// A fetch identifies the node to the propagation node, so a node whose operator
        /// chose not to announce is never identified on its own; `.mesh sync` still is.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_node_that_does_not_announce_never_fetches_automatically() {
            let ctx = ctx_for(MeshConfig {
                announce: false,
                propagation_sync_interval_secs: 1,
                ..MeshConfig::default()
            });
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-quiet").await;

            slot(&ctx).install(started.runtime.clone()).unwrap();

            sleep(Duration::from_millis(1500)).await;
            assert!(
                fake.script.seen().is_empty(),
                "nothing is fetched on its own while announce is false"
            );
            fake.script.reply_with([Value::Array(vec![])]);
            let report = timeout(
                FETCH_TIMEOUT,
                started.runtime.fetch_propagated(&LoggingInboundSink),
            )
            .await
            .expect("a manual fetch finishes within FETCH_TIMEOUT")
            .unwrap();
            assert_eq!(report.node, fake.hex());
            assert_eq!(fake.script.seen().len(), 1);
            tear_down(driver, &slot(&ctx), started, fake).await;
        }

        /// The loop is built on a one-slot queue already holding a note, so the install's
        /// `SyncNow` is refused by the queue; the flag it leaves behind starts the fetch
        /// when the loop wakes, and the refusal is not counted as a dropped peer event.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_sync_request_that_misses_the_queue_still_starts_a_fetch() {
            let ctx = sync_ctx(3600);
            let app = Arc::clone(&ctx.try_read().unwrap().app);
            let (fake, started) = runtime_with_node("idle-sync-lost-request").await;
            fake.script.reply_with([Value::Array(vec![])]);
            let (tx, rx) = mpsc::channel(1);
            let handle = IdleHandle::new(tx);
            app.mesh
                .set_idle(Arc::new(handle.clone()) as Arc<dyn IdleSink>);
            assert!(app.mesh.push_idle(note("fills the queue", None)));

            app.mesh.install(started.runtime.clone()).unwrap();

            assert!(handle.sync_requested.load(Ordering::Acquire));
            assert_eq!(
                handle.overflow(),
                0,
                "a lost sync request is not a peer event"
            );
            let cancel = CancellationToken::new();
            let driver_loop = tokio::spawn(
                DriverLoop::new(
                    Arc::clone(&ctx),
                    Arc::clone(&app),
                    rx,
                    Arc::clone(&handle.overflow),
                    Arc::clone(&handle.sync_requested),
                    cancel.clone(),
                )
                .run(),
            );

            wait_for_rounds(&fake, 1, FETCH_TIMEOUT).await;
            assert!(
                !handle.sync_requested.load(Ordering::Acquire),
                "the loop consumes the flag it acted on"
            );
            cancel.cancel();
            if let Some(task) = driver_loop.await.unwrap() {
                let _ = task.await;
            }
            app.mesh.stop().await.unwrap();
            started.relay_handle.abort();
            fake.stop().await;
        }

        /// The install's `SyncNow` queues behind notes the loop is still working through,
        /// so it is acted on from the queue once the loop gets to it; nothing else asks
        /// for the same fetch, and no second one follows the first.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_sync_request_is_started_once_even_when_the_loop_is_busy() {
            let ctx = sync_ctx(3600);
            let app = Arc::clone(&ctx.try_read().unwrap().app);
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-once").await;
            fake.script.reply_with([Value::Array(vec![])]);
            for index in 0..8 {
                assert!(app.mesh.push_idle(note(&format!("note {index}"), None)));
            }

            app.mesh.install(started.runtime.clone()).unwrap();

            wait_for_rounds(&fake, 1, FETCH_TIMEOUT).await;
            sleep(Duration::from_millis(1500)).await;
            assert_eq!(
                fake.script.seen().len(),
                1,
                "one request, one fetch: the queue delivers it and the flag is not also set"
            );
            tear_down(driver, &slot(&ctx), started, fake).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_sync_that_arrives_before_any_node_is_heard_is_retried_when_one_announces() {
            let ctx = sync_ctx(1);
            let driver = start_driver(&ctx);
            let fake = FakeNode::listen_with_mtu(TcpServer::DEFAULT_CLIENT_MTU).await;
            let started = started_runtime_on("idle-sync-no-node", fake.listener.port).await;
            fake.script.reply_with([Value::Array(vec![])]);

            slot(&ctx).install(started.runtime.clone()).unwrap();
            sleep(Duration::from_millis(500)).await;
            assert!(
                fake.script.seen().is_empty(),
                "nothing to fetch from before a node announces"
            );

            fake.announce().await;
            let runtime = started.runtime.clone();
            wait_until("the runtime to file the propagation node", || {
                runtime.propagation_nodes().select().is_ok()
            })
            .await;
            // The retry waits the shorter of the 1 s interval and the 15 s no-node retry,
            // so a ceiling under 15 s proves the interval won.
            assert!(PROPAGATION_SYNC_SHORT_RETRY > TEST_TIMEOUT);
            wait_for_rounds(&fake, 1, TEST_TIMEOUT).await;
            tear_down(driver, &slot(&ctx), started, fake).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stopping_the_mesh_ends_the_schedule() {
            let ctx = sync_ctx(1);
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-stop").await;
            fake.script.reply_with([Value::Array(vec![])]);

            slot(&ctx).install(started.runtime.clone()).unwrap();
            wait_for_rounds(&fake, 1, FETCH_TIMEOUT).await;

            assert!(slot(&ctx).stop().await.unwrap());
            let rounds = fake.script.seen().len();
            sleep(Duration::from_millis(2500)).await;
            assert_eq!(
                fake.script.seen().len(),
                rounds,
                "no fetch runs once the node is off"
            );
            driver.stop().await;
            started.relay_handle.abort();
            fake.stop().await;
        }

        /// `.mesh off` while a fetch is parked on a silent node: the fetch ends after the
        /// slot is already empty, and its outcome must not re-arm the 1 s interval.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn stopping_the_mesh_during_a_fetch_arms_nothing_afterwards() {
            let ctx = sync_ctx(1);
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-stop-in-flight").await;

            slot(&ctx).install(started.runtime.clone()).unwrap();
            wait_for_rounds(&fake, 1, FETCH_TIMEOUT).await;

            assert!(
                timeout(FETCH_TIMEOUT, slot(&ctx).stop())
                    .await
                    .expect("stopping the slot does not wait on the parked fetch")
                    .unwrap()
            );
            sleep(Duration::from_millis(2500)).await;
            assert_eq!(
                fake.script.seen().len(),
                1,
                "the fetch cut short by .mesh off arms no follow-up"
            );
            driver.stop().await;
            started.relay_handle.abort();
            fake.stop().await;
        }

        /// The `.mesh off` / `.mesh on` cycle: leaving the mesh disarms the schedule with
        /// nothing owed; the next install asks again on its own, so a rejoined node gets
        /// its join-time fetch without `.mesh sync`, and a fetch that receives nothing
        /// stays silent across both joins.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn rejoining_after_mesh_off_starts_the_join_time_sync_again() {
            let ctx = sync_ctx(3600);
            let sink = install_sink(&ctx);
            let driver = start_driver(&ctx);
            let (fake, started) = runtime_with_node("idle-sync-rejoin-1").await;
            fake.script
                .reply_with([Value::Array(vec![]), Value::Array(vec![])]);

            slot(&ctx).install(started.runtime.clone()).unwrap();
            wait_for_rounds(&fake, 1, FETCH_TIMEOUT).await;

            assert!(slot(&ctx).stop().await.unwrap());
            started.relay_handle.abort();
            sleep(Duration::from_millis(500)).await;
            assert_eq!(
                fake.script.seen().len(),
                1,
                "an hour-long interval fires nothing while the node is off"
            );

            let rejoined = started_runtime_on("idle-sync-rejoin-2", fake.listener.port).await;
            fake.announce().await;
            let runtime = rejoined.runtime.clone();
            wait_until("the rejoined runtime to file the propagation node", || {
                runtime.propagation_nodes().select().is_ok()
            })
            .await;
            slot(&ctx).install(rejoined.runtime.clone()).unwrap();

            wait_for_rounds(&fake, 2, FETCH_TIMEOUT).await;
            assert!(
                propagation_lines(&sink).is_empty(),
                "two empty fetches say nothing: {:?}",
                sink.lines()
            );
            tear_down(driver, &slot(&ctx), rejoined, fake).await;
        }
    }
}
