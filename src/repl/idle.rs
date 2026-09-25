//! Runs mesh work between REPL turns, while the editor sits in `read_line` and nothing
//! holds the session context. Human lines go out through the mesh slot's notifier; model
//! notes go into whatever notification queue the context holds at the moment of delivery;
//! child agents are minted from the context and run on their own tasks.
//!
//! Ownership rule: the driver holds the shared lock, never clones of what is behind it.
//! `use_agent` and `exit_agent` replace the notification queue and the supervisor, so a
//! clone taken at start-up would deliver into a queue nobody drains. Every touch is a
//! `try_read`/`try_write` inside a synchronous block that ends before the next `.await`:
//! the blocking forms would park a tokio worker for the length of a turn, and a guard held
//! across a model call would stall the user's next line.
//!
//! An agent switch cancels in-flight driver children along with the rest of the old
//! supervisor's tree, as Ctrl-C does; their completion notes still land in the queue the
//! switch installed.

use crate::config::{AppState, RequestContext};
use crate::function::agents::child_app_state;
use crate::mesh::idle::{
    Coalescer, IDLE_COALESCE_MAX_PEERS, IDLE_COALESCE_TICK, IDLE_QUEUE_CAPACITY, IdleNotify,
    IdleSink, Origin, OtherNames, RateLimiter,
};
use crate::mesh::notify::{Notification, Source};
use crate::supervisor::mailbox::Inbox;
use crate::supervisor::notification::{
    MESH_NOTIFICATION_QUEUE_CAPACITY, SystemNotification, mesh_events_dropped, mesh_notification,
};
use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
use crate::utils::{AbortSignal, create_abort_signal};

use anyhow::Result;
use log::{debug, warn};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Backoff between lock attempts while a turn holds the context write-locked.
pub(crate) const IDLE_LOCK_RETRY_TICK: Duration = Duration::from_millis(50);
/// How long `stop` waits for cancelled children to wind down before aborting them.
pub(crate) const IDLE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// Children the driver runs at once; the cap is the driver's own and does not touch the
/// supervisor's spawn budget, which is the model's.
pub(crate) const IDLE_MAX_CHILDREN: usize = 1;
/// Model notes held back while a turn owns the context. One short of the queue's own
/// mesh cap so the held notes fit in an empty queue without evicting each other; the
/// drop summary that follows them is pushed outside the cap.
pub(crate) const IDLE_PENDING_NOTES_MAX: usize = MESH_NOTIFICATION_QUEUE_CAPACITY - 1;
/// Characters of a child's output shown under its completion line.
pub(crate) const IDLE_RESULT_PREVIEW_CHARS: usize = 200;

const AGENT_COMPLETED_EVENT: &str = "mesh_agent_completed";
const AGENT_FAILED_EVENT: &str = "mesh_agent_failed";
const COLLECT_TOOL: &str = "agent__collect";
const UNCOLLECTABLE_NEXT_ACTION: &str =
    "output shown at the prompt; no collect tool is declared in this context";
const UNREGISTERED_NEXT_ACTION: &str =
    "output shown at the prompt; the handle is no longer registered";
const AT_CAPACITY: &str = "at capacity";

/// The body of a driver-spawned child: given an owned child context and its abort signal,
/// runs to completion and yields the child's output. The driver mints a bare child
/// context and nothing more, so the closure owns agent set-up and hook parity with the
/// in-turn spawn path (AgentStarted and the result hooks).
///
/// The child context arrives with its supervisor installed and registered as the
/// `child_supervisor` of the parent's handle, so grandchildren and jobs parked under it
/// are reached by `cancel_recursive` and seen by `has_active_tasks`. The closure must not
/// replace it: anything registered on a replacement would escape both. And it must be
/// built only after `agent_name` has been checked against the configured agents: the
/// name flows into the agent id and the log lines unsanitised.
pub(crate) type SpawnWork = Box<
    dyn FnOnce(RequestContext, AbortSignal) -> Pin<Box<dyn Future<Output = Result<String>> + Send>>
        + Send,
>;

pub(crate) struct IdleSpawn {
    pub(crate) agent_name: String,
    /// The agent's own jobs cap as its config resolves it, supplied by whoever validated
    /// `agent_name`: the driver's child context carries no agent, so it cannot resolve
    /// the cap itself. `None` gives the child the cap the context's config allows, the
    /// same one a top-level context gets when it first backgrounds a job. Agent spawning
    /// stays disabled for the child either way.
    pub(crate) max_concurrent_jobs: Option<usize>,
    pub(crate) work: SpawnWork,
}

pub(crate) enum IdleEvent {
    Notify(IdleNotify),
    // Constructed by the mesh request handlers once they land.
    #[allow(dead_code)]
    Spawn(IdleSpawn),
}

/// Producer side of the driver's queue. Never waits: a full or closed queue hands the
/// event back; only a full one counts as overflow.
#[derive(Clone)]
pub(crate) struct IdleHandle {
    tx: mpsc::Sender<IdleEvent>,
    overflow: Arc<AtomicUsize>,
}

impl IdleHandle {
    fn send(&self, event: IdleEvent) -> Result<(), IdleEvent> {
        self.tx.try_send(event).map_err(|err| {
            if matches!(err, TrySendError::Full(_)) {
                self.overflow.fetch_add(1, Ordering::AcqRel);
            }
            err.into_inner()
        })
    }

    // Reached by the mesh request handlers once they land.
    #[allow(dead_code)]
    pub(crate) fn spawn(&self, spawn: IdleSpawn) -> Result<(), IdleSpawn> {
        match self.send(IdleEvent::Spawn(spawn)) {
            Ok(()) => Ok(()),
            Err(IdleEvent::Spawn(spawn)) => Err(spawn),
            Err(IdleEvent::Notify(_)) => unreachable!("try_send hands back the event it was given"),
        }
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
            Err(IdleEvent::Spawn(_)) => unreachable!("try_send hands back the event it was given"),
        }
    }
}

/// Children the driver has started and not yet seen finish. The count is entered before
/// a child's task is spawned and left by a guard the task owns, so an aborted task still
/// leaves and `stop` can wait on the count deterministically.
#[derive(Default)]
struct InFlight {
    count: AtomicUsize,
    changed: Notify,
    children: parking_lot::Mutex<Vec<(AbortSignal, AbortHandle)>>,
}

struct InFlightGuard(Arc<InFlight>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

impl InFlight {
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.count.fetch_add(1, Ordering::AcqRel);
        InFlightGuard(Arc::clone(self))
    }

    fn running(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    fn register(&self, abort: AbortSignal, handle: AbortHandle) {
        let mut children = self.children.lock();
        children.retain(|(_, handle)| !handle.is_finished());
        children.push((abort, handle));
    }

    fn cancel_children(&self) {
        for (abort, _) in self.children.lock().iter() {
            abort.set_ctrlc();
        }
    }

    fn abort_children(&self) {
        for (_, handle) in self.children.lock().iter() {
            handle.abort();
        }
    }

    async fn wait_empty(&self) {
        loop {
            let changed = self.changed.notified();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) struct IdleDriver {
    handle: IdleHandle,
    cancel: CancellationToken,
    tasks: JoinSet<()>,
    in_flight: Arc<InFlight>,
    app: Arc<AppState>,
}

impl IdleDriver {
    /// Installs the driver as the mesh slot's idle sink and starts its loop. Must be called
    /// from inside the runtime.
    pub(crate) fn start(ctx: Arc<RwLock<RequestContext>>, app: Arc<AppState>) -> Self {
        let (tx, rx) = mpsc::channel(IDLE_QUEUE_CAPACITY);
        let handle = IdleHandle {
            tx,
            overflow: Arc::new(AtomicUsize::new(0)),
        };
        app.mesh
            .set_idle(Arc::new(handle.clone()) as Arc<dyn IdleSink>);
        let cancel = CancellationToken::new();
        let in_flight = Arc::new(InFlight::default());
        let mut tasks = JoinSet::new();
        tasks.spawn(
            DriverLoop::new(
                ctx,
                Arc::clone(&app),
                rx,
                Arc::clone(&handle.overflow),
                cancel.clone(),
                Arc::clone(&in_flight),
            )
            .run(),
        );
        Self {
            handle,
            cancel,
            tasks,
            in_flight,
            app,
        }
    }

    // Reached by the mesh request handlers once they land.
    #[allow(dead_code)]
    pub(crate) fn handle(&self) -> IdleHandle {
        self.handle.clone()
    }

    pub(crate) async fn stop(self) {
        self.stop_with_timeout(IDLE_STOP_TIMEOUT).await
    }

    /// Detaches from the slot, stops the loop, cancels every child and waits for them.
    /// A child that ignores its abort signal for `timeout` is aborted outright. Returns
    /// only once nothing the driver started is still running.
    pub(crate) async fn stop_with_timeout(mut self, timeout: Duration) {
        self.app.mesh.clear_idle();
        self.cancel.cancel();
        while let Some(joined) = self.tasks.join_next().await {
            if let Err(join_error) = joined {
                warn!("Idle driver loop did not exit cleanly: {join_error}");
            }
        }
        // Only after the loop has exited: it is the one place children are registered.
        self.in_flight.cancel_children();
        if tokio::time::timeout(timeout, self.in_flight.wait_empty())
            .await
            .is_err()
        {
            self.in_flight.abort_children();
            self.in_flight.wait_empty().await;
        }
        let overflow = self.handle.overflow();
        if overflow > 0 {
            warn!("Idle driver dropped {overflow} mesh event(s) on a full queue");
        }
    }
}

/// The synchronous part of `stop`, so an early return from the REPL that never reaches
/// `stop` still detaches the slot and signals the children. Every step is idempotent, so
/// running again after `stop_with_timeout` changes nothing.
impl Drop for IdleDriver {
    fn drop(&mut self) {
        self.app.mesh.clear_idle();
        self.cancel.cancel();
        self.in_flight.cancel_children();
    }
}

struct DriverLoop {
    ctx: Arc<RwLock<RequestContext>>,
    app: Arc<AppState>,
    rx: mpsc::Receiver<IdleEvent>,
    /// Shared with the producers' handle; read and reset at each coalesce tick so the
    /// prompt hears about a full queue while the driver runs.
    overflow: Arc<AtomicUsize>,
    cancel: CancellationToken,
    in_flight: Arc<InFlight>,
    rate_limiter: RateLimiter,
    coalescer: Coalescer,
    /// Model notes and spawns wait here while a turn holds the context, and are retried
    /// every `IDLE_LOCK_RETRY_TICK`. Notes past `IDLE_PENDING_NOTES_MAX` push out the
    /// oldest, which is counted and reported with the next flush. Whatever is still
    /// waiting when the driver stops is dropped with it.
    pending_model_notes: VecDeque<PendingNote>,
    dropped_model_notes: usize,
    /// Ids of the driver's own completions left in the queue for the model to collect.
    /// A completion later evicted from the queue is reaped only if its id is here: the
    /// evicted note's own id and event name are not trusted, since a peer may have
    /// forged them.
    reapable_ids: HashSet<String>,
    /// Reapable completions pushed out of `pending_model_notes` before any flush. The
    /// model will never see them, so their handles are taken with the next flush.
    orphaned_completions: Vec<String>,
    pending_spawns: VecDeque<IdleSpawn>,
    refusals: Refusals,
    coalesce_at: Option<tokio::time::Instant>,
    retry_at: Option<tokio::time::Instant>,
}

/// The slot must not outlive the loop that drains it: a loop that panics or is aborted
/// would otherwise leave every later event queued for nobody. Idempotent with `stop`
/// and `IdleDriver::drop`, which clear the slot first.
impl Drop for DriverLoop {
    fn drop(&mut self) {
        self.app.mesh.clear_idle();
    }
}

/// A model note on its way to the context's queue. `reapable` marks the completions the
/// driver itself produced: only those may take their handle off the supervisor when the
/// model will never be told to collect it, whether at delivery or, through the driver's
/// record of what it delivered, on a later eviction. A peer's note is never trusted that
/// far, whatever event name and id it carries.
struct PendingNote {
    note: SystemNotification,
    reapable: bool,
}

/// Spawns turned away since the last coalesce tick. The first `IDLE_COALESCE_MAX_PEERS`
/// agent names get a line of their own, with attempts and the last reason; the rest share
/// one closing line and their distinct names are bounded like the coalescer's other peers.
#[derive(Default)]
struct Refusals {
    named: BTreeMap<String, (usize, String)>,
    other_agents: OtherNames,
    other_attempts: usize,
}

impl Refusals {
    fn record(&mut self, agent_name: String, reason: &str) {
        if let Some((attempts, last_reason)) = self.named.get_mut(&agent_name) {
            *attempts += 1;
            *last_reason = reason.to_string();
        } else if self.named.len() < IDLE_COALESCE_MAX_PEERS {
            self.named.insert(agent_name, (1, reason.to_string()));
        } else {
            self.other_agents.insert(&agent_name);
            self.other_attempts += 1;
        }
    }

    fn is_empty(&self) -> bool {
        self.named.is_empty() && self.other_attempts == 0
    }

    fn attempts(&self) -> usize {
        self.named
            .values()
            .map(|(attempts, _)| attempts)
            .sum::<usize>()
            + self.other_attempts
    }

    /// Distinct agent names refused, with a `+` once the unnamed set stopped growing.
    fn agents_label(&self) -> String {
        let counted = self.named.len() + self.other_agents.len();
        if self.other_agents.is_saturated() {
            format!("{counted}+")
        } else {
            counted.to_string()
        }
    }

    fn flush(&mut self) -> Vec<Notification> {
        let mut lines: Vec<Notification> = std::mem::take(&mut self.named)
            .into_iter()
            .map(|(agent_name, (attempts, reason))| {
                let text = if attempts == 1 {
                    format!("{agent_name} not started: {reason}")
                } else {
                    format!("{agent_name} not started: {reason} ({attempts} attempts)")
                };
                Notification::new(Source::Mesh, text)
            })
            .collect();
        let other_agents = std::mem::take(&mut self.other_agents).count_label();
        let other_attempts = std::mem::take(&mut self.other_attempts);
        if other_attempts > 0 {
            lines.push(Notification::new(
                Source::Mesh,
                format!("({other_attempts} more spawn refusals for {other_agents} other agents)"),
            ));
        }
        lines
    }
}

impl DriverLoop {
    fn new(
        ctx: Arc<RwLock<RequestContext>>,
        app: Arc<AppState>,
        rx: mpsc::Receiver<IdleEvent>,
        overflow: Arc<AtomicUsize>,
        cancel: CancellationToken,
        in_flight: Arc<InFlight>,
    ) -> Self {
        Self {
            ctx,
            app,
            rx,
            overflow,
            cancel,
            in_flight,
            rate_limiter: RateLimiter::new(),
            coalescer: Coalescer::default(),
            pending_model_notes: VecDeque::new(),
            dropped_model_notes: 0,
            reapable_ids: HashSet::new(),
            orphaned_completions: Vec::new(),
            pending_spawns: VecDeque::new(),
            refusals: Refusals::default(),
            coalesce_at: None,
            retry_at: None,
        }
    }

    async fn run(mut self) {
        loop {
            let coalesce_at = self.coalesce_at.unwrap_or_else(tokio::time::Instant::now);
            let retry_at = self.retry_at.unwrap_or_else(tokio::time::Instant::now);
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                event = self.rx.recv() => match event {
                    Some(IdleEvent::Notify(note)) => self.on_notify(note),
                    Some(IdleEvent::Spawn(spawn)) => self.on_spawn(spawn),
                    None => break,
                },
                _ = sleep_until(coalesce_at), if self.coalesce_at.is_some() => {
                    self.coalesce_at = None;
                    self.flush_summaries();
                }
                _ = sleep_until(retry_at), if self.retry_at.is_some() => {
                    self.retry_at = None;
                }
            }
            self.flush_ctx_work();
            self.arm_timers();
        }
    }

    /// Local notes always go through. Peer notes spend a token: admitted ones go to the
    /// prompt at once and queue their model copy; rejected ones are folded into the next
    /// summary line and their model copy is dropped, since a flood must not reach the
    /// transcript any more than the prompt.
    fn on_notify(&mut self, note: IdleNotify) {
        let IdleNotify {
            source,
            text,
            origin,
            model_note,
        } = note;
        match origin {
            Origin::Local => {
                let pending = model_note.map(|note| PendingNote {
                    reapable: is_completion(&note),
                    note: *note,
                });
                self.deliver(source, text, pending);
            }
            Origin::Peer(peer) => {
                let now = tokio::time::Instant::now().into_std();
                if self.rate_limiter.admit(source, now) {
                    let pending = model_note.map(|note| PendingNote {
                        note: *note,
                        reapable: false,
                    });
                    self.deliver(source, text, pending);
                } else {
                    self.coalescer.fold(&peer);
                }
            }
        }
    }

    fn deliver(&mut self, source: Source, text: String, model_note: Option<PendingNote>) {
        self.app.mesh.notify(Notification::new(source, text));
        if let Some(model_note) = model_note {
            if self.pending_model_notes.len() >= IDLE_PENDING_NOTES_MAX
                && let Some(popped) = self.pending_model_notes.pop_front()
            {
                self.dropped_model_notes += 1;
                if popped.reapable {
                    self.orphaned_completions.push(popped.note.id);
                }
            }
            self.pending_model_notes.push_back(model_note);
        }
    }

    fn on_spawn(&mut self, spawn: IdleSpawn) {
        if self.pending_spawns.len() >= IDLE_MAX_CHILDREN {
            self.record_refusal(spawn.agent_name, AT_CAPACITY);
            return;
        }
        self.pending_spawns.push_back(spawn);
    }

    fn flush_summaries(&mut self) {
        for line in self.coalescer.flush() {
            self.app.mesh.notify(line);
        }
        if !self.refusals.is_empty() {
            warn!(
                "Idle driver refused {} spawn(s) for {} agent(s) since the last summary",
                self.refusals.attempts(),
                self.refusals.agents_label()
            );
            for line in self.refusals.flush() {
                self.app.mesh.notify(line);
            }
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
    /// left alone so a steady trickle of events cannot keep pushing it back.
    fn arm_timers(&mut self) {
        let now = tokio::time::Instant::now();
        let summaries_pending = !self.coalescer.is_empty()
            || !self.refusals.is_empty()
            || self.overflow.load(Ordering::Acquire) > 0;
        if summaries_pending && self.coalesce_at.is_none() {
            self.coalesce_at = Some(now + IDLE_COALESCE_TICK);
        }
        let ctx_work_pending =
            !self.pending_spawns.is_empty() || !self.pending_model_notes.is_empty();
        if ctx_work_pending && self.retry_at.is_none() {
            self.retry_at = Some(now + IDLE_LOCK_RETRY_TICK);
        }
    }

    /// Everything that needs the context, done under one short guard that this synchronous
    /// block drops before the loop awaits again. A spawn needs the write side (it may
    /// install the supervisor); notes alone need only the read side, since the queue's
    /// `push` takes `&self`.
    fn flush_ctx_work(&mut self) {
        let ctx = Arc::clone(&self.ctx);
        if !self.pending_spawns.is_empty() {
            if let Some(mut guard) = ctx.try_write() {
                self.flush_model_notes(&guard);
                while let Some(spawn) = self.pending_spawns.pop_front() {
                    self.start_child(&mut guard, spawn);
                }
            }
        } else if !self.pending_model_notes.is_empty()
            && let Some(guard) = ctx.try_read()
        {
            self.flush_model_notes(&guard);
        }
    }

    fn flush_model_notes(&mut self, ctx: &RequestContext) {
        for id in self.orphaned_completions.drain(..) {
            reap(ctx, &id);
        }
        match ctx.supervisor.as_ref() {
            Some(sup) => {
                let sup = sup.read();
                self.reapable_ids.retain(|id| sup.has_agent(id));
            }
            None => self.reapable_ids.clear(),
        }
        for note in self.pending_model_notes.drain(..) {
            deliver_model_note(ctx, note, &mut self.reapable_ids);
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

    /// Mints an owned child context and registers the child with the current supervisor
    /// outside its spawn budget, installing a budget-less one when the top level has none
    /// so the model's own `agent__spawn` stays as disabled as it was. The child gets a
    /// supervisor of the same budget-less shape up front, with the jobs cap the spawn
    /// carries when it has one and the context's config cap otherwise, registered as the
    /// handle's `child_supervisor`: a lazily installed one, as `job__start` would
    /// otherwise add, would hold jobs and grandchildren the parent's tree never reaches.
    /// The work runs on its own task, so the guard the caller holds is released as soon
    /// as this returns.
    fn start_child(&mut self, ctx: &mut RequestContext, spawn: IdleSpawn) {
        let IdleSpawn {
            agent_name,
            max_concurrent_jobs,
            work,
        } = spawn;
        if self.in_flight.running() >= IDLE_MAX_CHILDREN {
            self.record_refusal(agent_name, AT_CAPACITY);
            return;
        }
        let supervisor = ctx.ensure_supervisor();
        let depth = ctx.current_depth + 1;

        let agent_id = format!("agent_{agent_name}_{}", &Uuid::new_v4().to_string()[..8]);
        let child_inbox = Arc::new(Inbox::new());
        let child_abort = create_abort_signal();
        ctx.ensure_root_escalation_queue();
        ctx.ensure_inbox();
        let mut child_ctx = RequestContext::new_for_child(
            child_app_state(&ctx.app),
            ctx,
            depth,
            Arc::clone(&child_inbox),
            agent_id.clone(),
        );
        let child_supervisor = child_ctx.ensure_supervisor_with_jobs_cap(max_concurrent_jobs);

        debug!("Idle driver spawning child agent '{agent_name}' as '{agent_id}' at depth {depth}");

        let child = ChildRun {
            ctx: child_ctx,
            work,
            abort: child_abort.clone(),
            id: agent_id.clone(),
            name: agent_name.clone(),
            app: Arc::clone(&self.app),
            in_flight: self.in_flight.enter(),
        };
        let task = tokio::spawn(child.run());
        self.in_flight
            .register(child_abort.clone(), task.abort_handle());
        supervisor.write().register_unmetered(AgentHandle {
            id: agent_id,
            agent_name,
            depth,
            inbox: child_inbox,
            abort_signal: child_abort,
            join_handle: task,
            child_supervisor: Some(child_supervisor),
        });
    }

    fn record_refusal(&mut self, agent_name: String, reason: &str) {
        debug!("Idle driver refused to start '{agent_name}': {reason}");
        self.refusals.record(agent_name, reason);
    }
}

fn is_completion(note: &SystemNotification) -> bool {
    note.event == AGENT_COMPLETED_EVENT || note.event == AGENT_FAILED_EVENT
}

/// Pushes a note into the queue the context holds now. A completion points the model at
/// `agent__collect` only where that tool is declared and the supervisor the context holds
/// now still has the handle; an agent switch mid-flight leaves the handle on a supervisor
/// nobody can reach. Elsewhere the output shown at the prompt is all the model gets, and
/// a reapable handle is taken so a long idle session does not pile up results nobody can
/// collect. A reapable completion left for the model is remembered in `reapable_ids`, and
/// a completion the push evicts from a full queue is reaped only if it is remembered
/// there: the evicted note's id alone is no proof of who produced it.
fn deliver_model_note(
    ctx: &RequestContext,
    pending: PendingNote,
    reapable_ids: &mut HashSet<String>,
) {
    let PendingNote { mut note, reapable } = pending;
    if is_completion(&note) {
        let collectable = ctx.declared_function_names.contains(COLLECT_TOOL);
        let registered = ctx
            .supervisor
            .as_ref()
            .is_some_and(|sup| sup.read().has_agent(&note.id));
        match (collectable, registered) {
            (true, true) => {
                if reapable {
                    reapable_ids.insert(note.id.clone());
                }
            }
            (true, false) => note.next_action = UNREGISTERED_NEXT_ACTION.to_string(),
            (false, _) => {
                note.next_action = UNCOLLECTABLE_NEXT_ACTION.to_string();
                if reapable && registered {
                    reap(ctx, &note.id);
                }
            }
        }
    }
    if let Some(evicted) = ctx.notification_queue.push_mesh(note)
        && is_completion(&evicted)
        && reapable_ids.remove(&evicted.id)
    {
        reap(ctx, &evicted.id);
    }
}

/// The driver only ever registers unmetered handles, so a metered one is the model's to
/// collect whatever note names it. The handle is the only path to the child's supervisor
/// and whatever the child parked under it, so its tree is signalled before the handle is
/// dropped, with the parent's lock already released.
fn reap(ctx: &RequestContext, id: &str) {
    let Some(supervisor) = ctx.supervisor.as_ref() else {
        return;
    };
    let handle = {
        let mut sup = supervisor.write();
        if !sup.is_unmetered(id) {
            return;
        }
        sup.take(id)
    };
    if let Some(handle) = handle {
        if let Some(child) = handle.child_supervisor.as_ref() {
            child.read().cancel_recursive();
        }
        handle.abort_signal.set_ctrlc();
    }
}

/// The first non-blank line of a child's output, cut to `IDLE_RESULT_PREVIEW_CHARS`.
/// Sanitising is left to `render`, which every notifier line passes through.
fn result_preview(output: &str) -> String {
    output
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.chars().take(IDLE_RESULT_PREVIEW_CHARS).collect())
        .unwrap_or_default()
}

/// One child's task. Its completion goes back through the slot's idle sink, that is the
/// driver's own queue, so the model note lands in the queue the context holds then, not
/// the one it held at spawn; with the driver gone only the human line survives.
struct ChildRun {
    ctx: RequestContext,
    work: SpawnWork,
    abort: AbortSignal,
    id: String,
    name: String,
    app: Arc<AppState>,
    in_flight: InFlightGuard,
}

impl ChildRun {
    async fn run(self) -> Result<AgentResult> {
        let Self {
            ctx,
            work,
            abort,
            id,
            name,
            app,
            in_flight,
        } = self;
        let _in_flight = in_flight;
        let agent_result = match work(ctx, abort).await {
            Ok(output) => AgentResult {
                id: id.clone(),
                agent_name: name.clone(),
                output,
                exit_status: AgentExitStatus::Completed,
            },
            Err(e) => AgentResult {
                id: id.clone(),
                agent_name: name.clone(),
                output: String::new(),
                exit_status: AgentExitStatus::Failed(e.to_string()),
            },
        };
        let (event, mut text) = match &agent_result.exit_status {
            AgentExitStatus::Completed => (AGENT_COMPLETED_EVENT, format!("{name} finished")),
            AgentExitStatus::Failed(e) => (AGENT_FAILED_EVENT, format!("{name} failed: {e}")),
        };
        let preview = result_preview(&agent_result.output);
        if !preview.is_empty() {
            text.push('\n');
            text.push_str(&preview);
        }
        let success = agent_result.exit_status == AgentExitStatus::Completed;
        app.mesh.push_idle(IdleNotify {
            source: Source::Mesh,
            text,
            origin: Origin::Local,
            model_note: Some(Box::new(mesh_notification(
                event,
                &id,
                &name,
                success,
                format!("{COLLECT_TOOL} --id {id} for output"),
            ))),
        });
        Ok(agent_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{WorkingMode, effective_max_concurrent_jobs};
    use crate::function::agents::{
        GuardrailAction, check_pending_tasks_guardrail, handle_agent_tool,
    };
    use crate::function::drain_live_notifications;
    use crate::mesh::idle::{
        IDLE_COALESCE_MAX_OTHER_PEERS, IDLE_NOTIFY_BURST, IDLE_NOTIFY_REFILL_INTERVAL,
    };
    use crate::mesh::notify::{NotificationSink, RenderedNotification};
    use crate::repl::latch_prompt_interrupt;
    use crate::supervisor::Supervisor;
    use crate::supervisor::notification::{Channel, MESH_EVENTS_DROPPED_EVENT, NotificationQueue};
    use anyhow::anyhow;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc as std_mpsc;
    use std::thread;
    use tokio::sync::oneshot;
    use tokio::time::sleep;

    /// Ceiling on any one wait; the driver reacts within a lock-retry tick when healthy.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const POLL: Duration = Duration::from_millis(10);
    /// Short enough to keep the abort-on-timeout test quick, long enough that a
    /// cooperative child would have wound down.
    const SHORT_STOP_TIMEOUT: Duration = Duration::from_millis(100);

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

    fn has_active_tasks(ctx: &Arc<RwLock<RequestContext>>) -> bool {
        ctx.try_read().is_some_and(|guard| {
            guard
                .supervisor
                .as_ref()
                .is_some_and(|sup| sup.read().has_active_tasks())
        })
    }

    /// Blocks on the lock, which test code may do: the driver never holds it across an
    /// await, so the wait is bounded by one synchronous block.
    fn registered_agent_ids(ctx: &Arc<RwLock<RequestContext>>) -> Vec<String> {
        ctx.read()
            .supervisor
            .as_ref()
            .map(|sup| {
                sup.read()
                    .list_agents()
                    .into_iter()
                    .map(|(id, _)| id.to_string())
                    .collect()
            })
            .unwrap_or_default()
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

    /// A spawn whose work parks on a oneshot and then returns `Ok("done")`.
    fn parked_spawn(name: &str) -> (IdleSpawn, oneshot::Sender<()>) {
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let spawn = IdleSpawn {
            agent_name: name.to_string(),
            max_concurrent_jobs: None,
            work: Box::new(move |_ctx, _abort| {
                Box::pin(async move {
                    let _ = go_rx.await;
                    Ok("done".to_string())
                })
            }),
        };
        (spawn, go_tx)
    }

    /// A spawn whose work polls its abort signal, records seeing it, and returns.
    fn cancellable_spawn(name: &str) -> (IdleSpawn, Arc<AtomicBool>) {
        let observed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&observed);
        let spawn = IdleSpawn {
            agent_name: name.to_string(),
            max_concurrent_jobs: None,
            work: Box::new(move |_ctx, abort| {
                Box::pin(async move {
                    while !abort.aborted_ctrlc() {
                        sleep(POLL).await;
                    }
                    flag.store(true, Ordering::SeqCst);
                    Ok(String::new())
                })
            }),
        };
        (spawn, observed)
    }

    /// A spawn whose work sets a flag the moment it runs, to prove a refusal ran nothing.
    fn flagged_spawn(name: &str) -> (IdleSpawn, Arc<AtomicBool>) {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        let spawn = IdleSpawn {
            agent_name: name.to_string(),
            max_concurrent_jobs: None,
            work: Box::new(move |_ctx, _abort| {
                Box::pin(async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok(String::new())
                })
            }),
        };
        (spawn, ran)
    }

    /// Fills the driver to `IDLE_MAX_CHILDREN` parked children and waits until every one
    /// is registered. The senders keep them parked until dropped.
    async fn fill_children(
        ctx: &Arc<RwLock<RequestContext>>,
        driver: &IdleDriver,
    ) -> Vec<oneshot::Sender<()>> {
        let mut releases = Vec::new();
        for i in 0..IDLE_MAX_CHILDREN {
            let (spawn, go) = parked_spawn(&format!("parked{i}"));
            driver.handle().spawn(spawn).ok().expect("queued");
            releases.push(go);
            wait_until("the parked child to be registered", || {
                registered_agent_ids(ctx).len() == i + 1
            })
            .await;
        }
        releases
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_runs_a_spawn_to_completion_with_no_turn_active() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        go.send(()).unwrap();

        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(completed.channel, Channel::Mesh);
        assert_eq!(completed.tool_or_agent, "envoy");
        assert_eq!(completed.status, "success");
        assert_eq!(completed.to_value()["channel"], "mesh");
        assert!(completed.id.starts_with("agent_envoy_"), "{}", completed.id);
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failing_spawn_reports_mesh_agent_failed_to_both_channels() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        driver
            .handle()
            .spawn(IdleSpawn {
                agent_name: "envoy".into(),
                max_concurrent_jobs: None,
                work: Box::new(|_ctx, _abort| Box::pin(async { Err(anyhow!("relay gone")) })),
            })
            .ok()
            .expect("queued");

        let failed = wait_for_note(&ctx, |note| note.event == "mesh_agent_failed").await;
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.channel, Channel::Mesh);
        wait_until("the human line", || {
            sink.lines()
                .iter()
                .any(|line| line == "[mesh] envoy failed: relay gone")
        })
        .await;
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_child_is_registered_and_built_on_an_empty_mesh_slot() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let parent_app = Arc::clone(&ctx.try_read().unwrap().app);
        let driver = start_driver(&ctx);

        let seen_app: Arc<parking_lot::Mutex<Option<Arc<AppState>>>> = Default::default();
        let (go_tx, go_rx) = oneshot::channel::<()>();
        let capture = Arc::clone(&seen_app);
        driver
            .handle()
            .spawn(IdleSpawn {
                agent_name: "envoy".into(),
                max_concurrent_jobs: None,
                work: Box::new(move |child_ctx, _abort| {
                    Box::pin(async move {
                        *capture.lock() = Some(Arc::clone(&child_ctx.app));
                        let _ = go_rx.await;
                        Ok(String::new())
                    })
                }),
            })
            .ok()
            .expect("queued");

        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        wait_until("the child to start", || seen_app.lock().is_some()).await;
        let child_app = seen_app.lock().clone().unwrap();
        assert!(child_app.mesh.get().is_none());
        assert!(
            !Arc::ptr_eq(&child_app, &parent_app),
            "the child must not share the parent's app state"
        );
        assert!(has_active_tasks(&ctx));

        go_tx.send(()).unwrap();
        wait_until("the child to finish", || !has_active_tasks(&ctx)).await;
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_installs_a_supervisor_when_the_top_level_has_none() {
        let ctx = test_ctx(None);
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");

        wait_until("a supervisor with the child registered", || {
            has_active_tasks(&ctx)
        })
        .await;
        {
            let guard = ctx.try_read().unwrap();
            let sup = guard.supervisor.as_ref().unwrap().read();
            assert_eq!(sup.max_concurrent(), 0);
            assert_eq!(sup.max_depth(), 0);
            assert_eq!(sup.list_agents().len(), 1);
            assert_eq!(sup.list_agents()[0].1, "envoy");
            assert_eq!(
                sup.effective_active_count(),
                0,
                "a driver child must not spend the model's spawn budget"
            );
            assert!(sup.has_active_tasks());
        }

        go.send(()).unwrap();
        driver.stop().await;
    }

    /// The installed supervisor has no spawn budget, so the model-side tool is refused
    /// after the driver's spawn exactly as it was before it.
    #[test]
    fn an_idle_spawn_at_top_level_does_not_enable_model_side_agent_spawning() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let ctx = test_ctx(None);
        let (driver, go) = rt.block_on(async {
            let driver = start_driver(&ctx);
            let (spawn, go) = parked_spawn("envoy");
            driver.handle().spawn(spawn).ok().expect("queued");
            wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
            (driver, go)
        });

        let refused = {
            let mut guard = ctx.write();
            rt.block_on(handle_agent_tool(
                &mut guard,
                "agent__spawn",
                &json!({"agent": "explore", "prompt": "hi"}),
            ))
            .unwrap()
        };
        assert_eq!(refused["status"], "error");
        let message = refused["message"].as_str().unwrap();
        assert!(message.contains("Agent spawning not enabled"), "{message}");
        assert!(
            !message.contains("Wait for one to finish"),
            "a zero budget must not read as a full one: {message}"
        );

        go.send(()).unwrap();
        rt.block_on(driver.stop());
    }

    /// The child's supervisor is installed by the driver and hung on the parent's handle,
    /// so anything the child parks under it (a grandchild here; a job just as well) stays
    /// in the tree with no help from the child's own abort signal: `has_active_tasks`
    /// sees it after the child itself has finished, and `cancel_recursive` reaches it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grandchild_parked_under_the_child_supervisor_is_reached_through_the_handle() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        ctx.try_write()
            .unwrap()
            .declared_function_names
            .insert(COLLECT_TOOL.to_string());
        let driver = start_driver(&ctx);

        let seen: Arc<parking_lot::Mutex<Option<Arc<RwLock<Supervisor>>>>> = Default::default();
        let grandchild_abort = create_abort_signal();
        let spawn = IdleSpawn {
            agent_name: "envoy".to_string(),
            max_concurrent_jobs: Some(3),
            work: {
                let seen = Arc::clone(&seen);
                let grandchild_abort = grandchild_abort.clone();
                Box::new(move |mut child_ctx, _abort| {
                    Box::pin(async move {
                        let sup = child_ctx.ensure_supervisor();
                        sup.write().register_unmetered(AgentHandle {
                            id: "grandchild".to_string(),
                            agent_name: "envoy".to_string(),
                            depth: 2,
                            inbox: Arc::new(Inbox::new()),
                            abort_signal: grandchild_abort,
                            join_handle: tokio::spawn(std::future::pending()),
                            child_supervisor: None,
                        });
                        *seen.lock() = Some(sup);
                        Ok("done".to_string())
                    })
                })
            },
        };
        driver.handle().spawn(spawn).ok().expect("queued");

        let landed = wait_for_note(&ctx, |note| note.event == AGENT_COMPLETED_EVENT).await;
        let child_id = landed.id;
        wait_until("the child task to finish", || {
            ctx.try_read().is_some_and(|guard| {
                guard
                    .supervisor
                    .as_ref()
                    .unwrap()
                    .read()
                    .is_finished(&child_id)
                    == Some(true)
            })
        })
        .await;
        assert!(
            has_active_tasks(&ctx),
            "the parked grandchild is the only activity and must be seen through the handle"
        );

        ctx.try_read()
            .unwrap()
            .supervisor
            .as_ref()
            .unwrap()
            .read()
            .cancel_recursive();
        assert!(
            grandchild_abort.aborted_ctrlc(),
            "cancelling the tree must reach the grandchild"
        );

        let handle = ctx
            .try_read()
            .unwrap()
            .supervisor
            .as_ref()
            .unwrap()
            .write()
            .take(&child_id)
            .expect("a collectable completion leaves its handle registered");
        let child_sup = handle
            .child_supervisor
            .as_ref()
            .expect("the child's supervisor is registered on its handle");
        let seen = seen.lock().clone().expect("the work ran");
        assert!(
            Arc::ptr_eq(child_sup, &seen),
            "the closure's lazy install must find the driver's supervisor, not add one"
        );
        assert!(child_sup.read().has_agent("grandchild"));
        assert_eq!(
            child_sup.read().max_concurrent(),
            0,
            "the child gets the same zero spawn budget as the top level"
        );
        assert_eq!(
            child_sup.read().max_concurrent_jobs(),
            3,
            "the jobs cap the spawn carries is the one the child's supervisor gets"
        );
        driver.stop().await;
    }

    /// Reaping a completion drops the only path to the child's supervisor, so whatever
    /// the child left running under it is signalled first: a grandchild parked there is
    /// aborted along with the reap, not detached where no cancel can find it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reaping_a_completion_cancels_the_tree_under_its_handle() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        assert!(
            ctx.try_read().unwrap().declared_function_names.is_empty(),
            "a fresh top-level context declares no tools, so the completion is reaped"
        );
        let driver = start_driver(&ctx);

        let grandchild_abort = create_abort_signal();
        let spawn = IdleSpawn {
            agent_name: "envoy".to_string(),
            max_concurrent_jobs: None,
            work: {
                let grandchild_abort = grandchild_abort.clone();
                Box::new(move |mut child_ctx, _abort| {
                    Box::pin(async move {
                        child_ctx
                            .ensure_supervisor()
                            .write()
                            .register_unmetered(AgentHandle {
                                id: "grandchild".to_string(),
                                agent_name: "envoy".to_string(),
                                depth: 2,
                                inbox: Arc::new(Inbox::new()),
                                abort_signal: grandchild_abort,
                                join_handle: tokio::spawn(std::future::pending()),
                                child_supervisor: None,
                            });
                        Ok("done".to_string())
                    })
                })
            },
        };
        driver.handle().spawn(spawn).ok().expect("queued");

        let completed = wait_for_note(&ctx, |note| note.event == AGENT_COMPLETED_EVENT).await;
        assert_eq!(completed.next_action, UNCOLLECTABLE_NEXT_ACTION);
        assert!(
            grandchild_abort.aborted_ctrlc(),
            "reaping the handle must signal the grandchild parked under it"
        );
        assert!(
            !ctx.try_read()
                .unwrap()
                .supervisor
                .as_ref()
                .unwrap()
                .read()
                .has_agent(&completed.id),
            "the uncollectable handle is reaped with its note"
        );
        assert!(
            !has_active_tasks(&ctx),
            "nothing the child parked may stay registered once its handle is gone"
        );
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawn_without_a_jobs_cap_gives_the_child_the_context_cap() {
        let ctx = test_ctx(None);
        ctx.try_write()
            .unwrap()
            .declared_function_names
            .insert(COLLECT_TOOL.to_string());
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        go.send(()).unwrap();

        let landed = wait_for_note(&ctx, |note| note.event == AGENT_COMPLETED_EVENT).await;
        let handle = ctx
            .try_read()
            .unwrap()
            .supervisor
            .as_ref()
            .unwrap()
            .write()
            .take(&landed.id)
            .expect("a collectable completion leaves its handle registered");
        let child_sup = handle
            .child_supervisor
            .as_ref()
            .expect("the child's supervisor is registered on its handle");
        assert_eq!(
            child_sup.read().max_concurrent_jobs(),
            effective_max_concurrent_jobs(None, &app.config),
            "with no cap on the spawn the child gets the one the context's config allows"
        );
        assert_eq!(child_sup.read().max_concurrent(), 0);
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_is_refused_past_the_driver_child_cap() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let releases = fill_children(&ctx, &driver).await;

        let (spawn, ran) = flagged_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");

        wait_until("the refusal line", || {
            sink.lines()
                .iter()
                .any(|line| line.starts_with("[mesh] envoy not started:"))
        })
        .await;
        assert!(!ran.load(Ordering::SeqCst), "refused work must never run");
        assert_eq!(registered_agent_ids(&ctx).len(), IDLE_MAX_CHILDREN);

        drop(releases);
        driver.stop().await;
        assert!(
            ctx.try_read()
                .unwrap()
                .notification_queue
                .drain()
                .iter()
                .all(|note| note.tool_or_agent != "envoy"),
            "a refused spawn must not report a completion or a failure"
        );
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

    /// A completion pushed out of the held-back notes is one the model will never read,
    /// so its handle goes with it at the next flush rather than lingering uncollected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_completion_pushed_out_of_the_pending_deque_reaps_its_handle() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        register_handle(&ctx, "u1", true);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let (release, holder) = hold_write_lock(&ctx);

        driver
            .handle()
            .push(note(
                "envoy finished",
                Some(mesh_notification(
                    AGENT_COMPLETED_EVENT,
                    "u1",
                    "envoy",
                    true,
                    String::new(),
                )),
            ))
            .expect("queued");
        for i in 0..IDLE_PENDING_NOTES_MAX {
            driver
                .handle()
                .push(note(
                    &format!("note {i}"),
                    Some(model_note(&format!("n{i}"))),
                ))
                .expect("queued");
        }
        wait_until("every human line to reach the sink", || {
            sink.lines().len() == IDLE_PENDING_NOTES_MAX + 1
        })
        .await;

        drop(release);
        holder.join().unwrap();
        wait_until("the pushed-out completion to reap its handle", || {
            ctx.try_read()
                .is_some_and(|guard| !guard.supervisor.as_ref().unwrap().read().has_agent("u1"))
        })
        .await;
        let landed = wait_for_note(&ctx, |note| note.event == MESH_EVENTS_DROPPED_EVENT).await;
        assert_eq!(
            landed.next_action,
            "1 older mesh event was dropped while a turn held the context"
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_child_counts_as_active_for_the_prompt_interrupt_latch() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let driver = start_driver(&ctx);

        let (spawn, observed) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        let supervisor = ctx.try_read().unwrap().supervisor.clone().unwrap();
        let abort = create_abort_signal();
        latch_prompt_interrupt(&abort, &supervisor);
        assert!(
            abort.aborted_ctrlc(),
            "a running driver child makes ctrl-c an interruption"
        );
        wait_until("the child to observe its abort signal", || {
            observed.load(Ordering::SeqCst)
        })
        .await;
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_awaits_every_child_before_returning() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let driver = start_driver(&ctx);

        let (spawn, finished) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        assert!(!finished.load(Ordering::SeqCst));

        driver.stop().await;
        assert!(
            finished.load(Ordering::SeqCst),
            "stop returned before the cancelled child finished"
        );
    }

    /// An early return from the REPL drops the driver without `stop`: the slot must not
    /// keep pointing at the dead queue, and the children must be told to wind down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_an_unstopped_driver_detaches_the_slot_and_cancels_children() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let (spawn, observed) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        drop(driver);

        wait_until("the child to observe its abort signal", || {
            observed.load(Ordering::SeqCst)
        })
        .await;
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
        let handle = IdleHandle {
            tx,
            overflow: Arc::new(AtomicUsize::new(0)),
        };
        app.mesh
            .set_idle(Arc::new(handle.clone()) as Arc<dyn IdleSink>);
        let driver_loop = DriverLoop::new(
            Arc::clone(&ctx),
            Arc::clone(&app),
            rx,
            Arc::clone(&handle.overflow),
            CancellationToken::new(),
            Arc::new(InFlight::default()),
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

    struct FlagOnDrop(Arc<AtomicBool>);

    impl Drop for FlagOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_aborts_a_child_that_ignores_cancellation() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let driver = start_driver(&ctx);

        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        driver
            .handle()
            .spawn(IdleSpawn {
                agent_name: "stubborn".into(),
                max_concurrent_jobs: None,
                work: Box::new(move |_ctx, _abort| {
                    Box::pin(async move {
                        let _flag = FlagOnDrop(flag);
                        std::future::pending::<()>().await;
                        Ok(String::new())
                    })
                }),
            })
            .ok()
            .expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        driver.stop_with_timeout(SHORT_STOP_TIMEOUT).await;
        assert!(
            dropped.load(Ordering::SeqCst),
            "stop returned while the stubborn child's future was still alive"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_turn_can_take_the_write_lock_while_a_spawned_child_is_mid_flight() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        let deadline = tokio::time::Instant::now() + TEST_TIMEOUT / 10;
        let mut attempts = 0;
        while tokio::time::Instant::now() < deadline {
            assert!(
                ctx.try_write().is_some(),
                "a user turn could not take the context while the child was mid-flight"
            );
            attempts += 1;
            sleep(POLL).await;
        }
        assert!(attempts > 1);
        assert!(has_active_tasks(&ctx), "the child must still be running");

        go.send(()).unwrap();
        driver.stop().await;
    }

    #[test]
    fn queue_overflow_is_counted_and_returns_the_note() {
        let (tx, _rx) = mpsc::channel(1);
        let handle = IdleHandle {
            tx,
            overflow: Arc::new(AtomicUsize::new(0)),
        };
        assert!(handle.push(note("fits", None)).is_ok());
        let returned = handle
            .push(note("overflows", None))
            .expect_err("a full queue hands the note back");
        assert_eq!(returned.text, "overflows");
        assert_eq!(handle.overflow(), 1);
    }

    /// What `exit_agent` does while a driver child is still running: the old supervisor's
    /// tree is cancelled, then the queue is replaced and the supervisor nulled. The child
    /// is cancelled with the rest, and its completion note must land in the queue the
    /// context holds at completion time; the missing supervisor must not lose it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_note_lands_in_the_queue_installed_while_the_child_was_mid_flight() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let (spawn, observed) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        let old_queue = {
            let mut guard = ctx.try_write().unwrap();
            guard.supervisor.as_ref().unwrap().read().cancel_recursive();
            let old = Arc::clone(&guard.notification_queue);
            guard.notification_queue = Arc::new(NotificationQueue::new());
            guard.supervisor = None;
            old
        };

        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert!(
            observed.load(Ordering::SeqCst),
            "the switch must cancel the in-flight child"
        );
        assert_eq!(completed.tool_or_agent, "envoy");
        assert_eq!(completed.status, "success");
        assert!(
            old_queue.drain().is_empty(),
            "the completion must not land in the queue that was replaced"
        );
        wait_until("the human completion line", || {
            sink.lines()
                .iter()
                .any(|line| line == "[mesh] envoy finished")
        })
        .await;
        driver.stop().await;
    }

    /// Without a collect tool in the context the model cannot act on a collect hint, so
    /// the note says where the output went and the handle is reaped rather than kept.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_note_at_top_level_drops_the_collect_hint_and_reaps_the_handle() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        assert!(
            ctx.try_read().unwrap().declared_function_names.is_empty(),
            "a fresh top-level context declares no tools"
        );
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        let id = registered_agent_ids(&ctx).remove(0);

        go.send(()).unwrap();
        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(completed.id, id);
        assert_eq!(completed.next_action, UNCOLLECTABLE_NEXT_ACTION);
        assert!(
            !completed.next_action.contains("agent__collect"),
            "{}",
            completed.next_action
        );
        assert!(
            !ctx.try_read()
                .unwrap()
                .supervisor
                .as_ref()
                .unwrap()
                .read()
                .has_agent(&id),
            "an uncollectable handle must be reaped with its note"
        );
        wait_until("the completion line and its preview", || {
            let lines = sink.lines();
            lines.contains(&"[mesh] envoy finished".to_string())
                && lines.contains(&"[mesh] done".to_string())
        })
        .await;
        driver.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_note_keeps_the_collect_hint_where_the_tool_is_declared() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        ctx.try_write()
            .unwrap()
            .declared_function_names
            .insert("agent__collect".to_string());
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        let id = registered_agent_ids(&ctx).remove(0);

        go.send(()).unwrap();
        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(
            completed.next_action,
            format!("agent__collect --id {id} for output")
        );
        assert!(
            ctx.try_read()
                .unwrap()
                .supervisor
                .as_ref()
                .unwrap()
                .read()
                .has_agent(&id),
            "a collectable handle stays registered until collected"
        );
        driver.stop().await;
    }

    /// What `use_agent` does while a driver child is still running: the old supervisor's
    /// tree is cancelled and a fresh supervisor installed, with the collect tool declared
    /// on the new agent. The completion then names a handle only the dropped supervisor
    /// held, so pointing the model at `agent__collect` would send it after nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_note_drops_the_collect_hint_when_the_new_supervisor_lacks_the_handle() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        ctx.try_write()
            .unwrap()
            .declared_function_names
            .insert(COLLECT_TOOL.to_string());
        let driver = start_driver(&ctx);

        let (spawn, observed) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        let id = registered_agent_ids(&ctx).remove(0);

        {
            let mut guard = ctx.try_write().unwrap();
            guard.supervisor.as_ref().unwrap().read().cancel_recursive();
            guard.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(4, 3))));
        }

        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert!(
            observed.load(Ordering::SeqCst),
            "the switch must cancel the in-flight child"
        );
        assert_eq!(completed.id, id);
        assert_eq!(completed.next_action, UNREGISTERED_NEXT_ACTION);
        assert!(
            !completed.next_action.contains(COLLECT_TOOL),
            "{}",
            completed.next_action
        );
        assert!(
            !has_agent(&ctx, &id),
            "the new supervisor never held the handle"
        );
        driver.stop().await;
    }

    #[test]
    fn result_preview_is_the_first_non_blank_line_cut_to_the_cap() {
        assert_eq!(result_preview(""), "");
        assert_eq!(result_preview("\n  \nsecond\nthird"), "second");
        let long = "x".repeat(IDLE_RESULT_PREVIEW_CHARS * 2);
        assert_eq!(
            result_preview(&long).chars().count(),
            IDLE_RESULT_PREVIEW_CHARS
        );
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

    /// Refusals do not each earn a prompt line: a burst against the child cap is one line
    /// per agent name at the coalesce tick, carrying the attempt count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refusals_in_one_burst_are_summarised_as_one_line_per_agent() {
        const ATTEMPTS: usize = 5;
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let releases = fill_children(&ctx, &driver).await;

        let mut flags = Vec::new();
        for _ in 0..ATTEMPTS {
            let (spawn, ran) = flagged_spawn("envoy");
            driver.handle().spawn(spawn).ok().expect("queued");
            flags.push(ran);
        }

        wait_until("the refusal line", || {
            sink.lines()
                .iter()
                .any(|line| line.starts_with("[mesh] envoy not started:"))
        })
        .await;
        // A second tick would be the only way for another refusal line to appear.
        sleep(IDLE_COALESCE_TICK + IDLE_LOCK_RETRY_TICK).await;
        let refusal_lines: Vec<String> = sink
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("[mesh] envoy not started:"))
            .collect();
        assert_eq!(refusal_lines.len(), 1, "{refusal_lines:?}");
        assert!(
            refusal_lines[0].ends_with(&format!("({ATTEMPTS} attempts)")),
            "{}",
            refusal_lines[0]
        );
        assert!(
            flags.iter().all(|ran| !ran.load(Ordering::SeqCst)),
            "refused work must never run"
        );

        drop(releases);
        driver.stop().await;
    }

    /// The child is minted one level below the context that spawned it, with its own
    /// notification queue, so nothing it does lands in the parent's queue by accident.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawned_child_runs_one_level_below_with_its_own_queue() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let parent_queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let driver = start_driver(&ctx);

        type Seen = parking_lot::Mutex<Option<(usize, Arc<NotificationQueue>)>>;
        let seen: Arc<Seen> = Default::default();
        let capture = Arc::clone(&seen);
        driver
            .handle()
            .spawn(IdleSpawn {
                agent_name: "envoy".into(),
                max_concurrent_jobs: None,
                work: Box::new(move |child_ctx, _abort| {
                    Box::pin(async move {
                        *capture.lock() = Some((
                            child_ctx.current_depth,
                            Arc::clone(&child_ctx.notification_queue),
                        ));
                        Ok(String::new())
                    })
                }),
            })
            .ok()
            .expect("queued");

        wait_until("the child to start", || seen.lock().is_some()).await;
        let (depth, child_queue) = seen.lock().clone().unwrap();
        assert_eq!(depth, 1);
        assert!(
            !Arc::ptr_eq(&child_queue, &parent_queue),
            "the child must not share the parent's notification queue"
        );
        wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
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

    /// The spawn half of the retry rule: a spawn that arrives while a turn holds the
    /// context runs nothing until the turn ends, then starts within a retry tick.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawn_queued_during_a_turn_starts_once_the_turn_releases_the_context() {
        let ctx = test_ctx(None);
        let queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let driver = start_driver(&ctx);
        let (release, holder) = hold_write_lock(&ctx);

        let (spawn, ran) = flagged_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        sleep(IDLE_LOCK_RETRY_TICK * 3).await;
        assert!(
            !ran.load(Ordering::SeqCst),
            "no child may start while the turn holds the context"
        );
        assert!(
            ctx.try_read().is_none(),
            "the turn must still hold the lock"
        );
        assert!(queue.drain().is_empty());

        drop(release);
        holder.join().unwrap();
        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(completed.tool_or_agent, "envoy");
        assert!(ran.load(Ordering::SeqCst));
        assert!(
            ctx.read().supervisor.is_some(),
            "the deferred spawn installs the top-level supervisor once it runs"
        );
        driver.stop().await;
    }

    /// The child cap bounds concurrency, not the session: a spawn that arrives after the
    /// previous child finished is started, not turned away as "at capacity".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capacity_freed_by_a_finished_child_admits_the_next_spawn() {
        let ctx = test_ctx(None);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let (first, go) = parked_spawn("first");
        driver.handle().spawn(first).ok().expect("queued");
        wait_until("the first child to be registered", || {
            has_active_tasks(&ctx)
        })
        .await;
        go.send(()).unwrap();
        let done = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(done.tool_or_agent, "first");
        wait_until("the first child to leave the in-flight count", || {
            driver.in_flight.running() == 0
        })
        .await;

        let (second, ran) = flagged_spawn("second");
        driver.handle().spawn(second).ok().expect("queued");
        let done = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(done.tool_or_agent, "second");
        assert!(ran.load(Ordering::SeqCst), "the second spawn must run");

        // Refusals are only reported at the coalesce tick; give one a chance to show.
        sleep(IDLE_COALESCE_TICK + IDLE_LOCK_RETRY_TICK).await;
        let lines = sink.lines();
        assert!(
            !lines.iter().any(|line| line.contains("not started")),
            "a sequential spawn must not be refused: {lines:?}"
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

    /// The REPL's ctrl-c arm at an idle prompt reads whatever supervisor the context holds
    /// at that moment. A running driver child at the top level, where there was no
    /// supervisor before, makes ctrl-c an interruption that reaches the child; once the
    /// child is gone, ctrl-c at the prompt is as inert as it was before the driver.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_c_at_the_top_level_prompt_reaches_a_driver_child_and_is_clear_after_it() {
        let ctx = test_ctx(None);
        let driver = start_driver(&ctx);
        assert!(ctx.read().supervisor.is_none());

        let (spawn, observed) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        // What `Signal::CtrlC` does in the REPL loop.
        let abort = create_abort_signal();
        if let Some(supervisor) = ctx.read().supervisor.clone() {
            latch_prompt_interrupt(&abort, &supervisor);
        }
        assert!(
            abort.aborted_ctrlc(),
            "a running driver child makes ctrl-c an interruption"
        );
        wait_until("the child to observe its abort signal", || {
            observed.load(Ordering::SeqCst)
        })
        .await;
        wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        wait_until("the child to leave the in-flight count", || {
            driver.in_flight.running() == 0
        })
        .await;

        let abort = create_abort_signal();
        if let Some(supervisor) = ctx.read().supervisor.clone() {
            latch_prompt_interrupt(&abort, &supervisor);
        }
        assert!(
            !abort.aborted_ctrlc(),
            "with no running driver child, ctrl-c at the prompt must stay clear"
        );
        driver.stop().await;
    }

    /// A handle whose task never finishes, registered on the context's supervisor the
    /// way the model's own spawns are (metered) or the driver's (unmetered).
    fn register_handle(ctx: &Arc<RwLock<RequestContext>>, id: &str, unmetered: bool) {
        let handle = AgentHandle {
            id: id.to_string(),
            agent_name: "envoy".to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle: tokio::spawn(std::future::pending()),
            child_supervisor: None,
        };
        let guard = ctx.try_read().unwrap();
        let mut sup = guard.supervisor.as_ref().unwrap().write();
        if unmetered {
            sup.register_unmetered(handle);
        } else {
            sup.register(handle).unwrap();
        }
    }

    fn has_agent(ctx: &Arc<RwLock<RequestContext>>, id: &str) -> bool {
        ctx.try_read()
            .unwrap()
            .supervisor
            .as_ref()
            .unwrap()
            .read()
            .has_agent(id)
    }

    /// The turn-end guardrail nags about background tasks the model started and left
    /// unreclaimed. A driver child, running or finished and awaiting collection, is not
    /// one of those, so a mesh session must not end every turn with a reminder or, three
    /// turns in, a cancellation of the child.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_driver_child_does_not_trip_the_turn_end_guardrail() {
        let ctx = test_ctx(None);
        ctx.try_write()
            .unwrap()
            .declared_function_names
            .insert(COLLECT_TOOL.to_string());
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        let id = registered_agent_ids(&ctx).remove(0);

        assert!(matches!(
            check_pending_tasks_guardrail(&mut ctx.try_write().unwrap()),
            GuardrailAction::NoAction
        ));
        assert!(
            has_active_tasks(&ctx),
            "the guardrail must not cancel the child"
        );

        go.send(()).unwrap();
        wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert!(
            has_agent(&ctx, &id),
            "a collectable handle stays registered until collected"
        );
        assert!(matches!(
            check_pending_tasks_guardrail(&mut ctx.try_write().unwrap()),
            GuardrailAction::NoAction
        ));
        assert!(has_agent(&ctx, &id));
        driver.stop().await;
    }

    /// A burst of refusals across many agent names gets a bounded number of lines: the
    /// first names each get one, the rest share one closing line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refusals_past_the_named_cap_share_one_closing_line() {
        const NAMES: usize = 20;
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let releases = fill_children(&ctx, &driver).await;

        let mut flags = Vec::new();
        for i in 0..NAMES {
            let (spawn, ran) = flagged_spawn(&format!("agent{i:02}"));
            driver.handle().spawn(spawn).ok().expect("queued");
            flags.push(ran);
        }

        let others = NAMES - IDLE_COALESCE_MAX_PEERS;
        let closing = format!("[mesh] ({others} more spawn refusals for {others} other agents)");
        wait_until("the closing refusal line", || {
            sink.lines().iter().any(|line| line == &closing)
        })
        .await;
        // A second tick would be the only way for another refusal line to appear.
        sleep(IDLE_COALESCE_TICK + IDLE_LOCK_RETRY_TICK).await;
        let lines = sink.lines();
        let named: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains(" not started: "))
            .collect();
        assert_eq!(named.len(), IDLE_COALESCE_MAX_PEERS, "{lines:?}");
        assert!(
            named
                .iter()
                .all(|line| line.ends_with(&format!("not started: {AT_CAPACITY}"))),
            "{named:?}"
        );
        let closing_lines = lines
            .iter()
            .filter(|line| line.contains("more spawn refusals"))
            .count();
        assert_eq!(closing_lines, 1, "{lines:?}");
        assert!(
            flags.iter().all(|ran| !ran.load(Ordering::SeqCst)),
            "refused work must never run"
        );

        drop(releases);
        driver.stop().await;
    }

    /// Past the named cap the distinct names are remembered up to the coalescer's bound
    /// and only counted after it, and every attempt is still accounted for.
    #[test]
    fn refusals_stop_remembering_other_names_at_the_cap_and_say_so() {
        const NAMES: usize = 100;
        let mut refusals = Refusals::default();
        assert!(refusals.is_empty());
        for i in 0..NAMES {
            refusals.record(format!("agent{i:03}"), AT_CAPACITY);
            assert!(refusals.other_agents.len() <= IDLE_COALESCE_MAX_OTHER_PEERS);
        }
        refusals.record("agent000".to_string(), "again");

        assert_eq!(refusals.attempts(), NAMES + 1);
        let remembered = IDLE_COALESCE_MAX_PEERS + IDLE_COALESCE_MAX_OTHER_PEERS;
        assert_eq!(refusals.agents_label(), format!("{remembered}+"));

        let lines: Vec<String> = refusals
            .flush()
            .into_iter()
            .flat_map(|note| note.render_lines())
            .collect();
        assert_eq!(lines.len(), IDLE_COALESCE_MAX_PEERS + 1);
        assert_eq!(lines[0], "[mesh] agent000 not started: again (2 attempts)");
        assert_eq!(
            lines[1],
            format!("[mesh] agent001 not started: {AT_CAPACITY}")
        );
        let others = NAMES - IDLE_COALESCE_MAX_PEERS;
        assert_eq!(
            lines[IDLE_COALESCE_MAX_PEERS],
            format!(
                "[mesh] ({others} more spawn refusals for {IDLE_COALESCE_MAX_OTHER_PEERS}+ other agents)"
            )
        );
        assert!(refusals.is_empty());
        assert!(refusals.flush().is_empty());
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

    /// A completion the queue evicts to make room is one the model will never be told
    /// to collect, so its handle is reaped with it, but only an unmetered one: a metered
    /// handle is the model's to collect whatever the queue drops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_evicted_completion_reaps_a_driver_handle_and_no_other() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        ctx.try_write()
            .unwrap()
            .declared_function_names
            .insert(COLLECT_TOOL.to_string());
        register_handle(&ctx, "u1", true);
        register_handle(&ctx, "a1", false);
        let mut reapable_ids = HashSet::new();
        let completion = |id: &str| PendingNote {
            note: mesh_notification(AGENT_COMPLETED_EVENT, id, "envoy", true, String::new()),
            reapable: true,
        };

        {
            let guard = ctx.try_read().unwrap();
            deliver_model_note(&guard, completion("u1"), &mut reapable_ids);
            deliver_model_note(&guard, completion("a1"), &mut reapable_ids);
        }
        assert!(has_agent(&ctx, "u1"), "a collectable completion is kept");
        assert!(has_agent(&ctx, "a1"));
        assert_eq!(
            reapable_ids,
            HashSet::from(["u1".to_string(), "a1".to_string()]),
            "both were left registered and collectable"
        );

        {
            let guard = ctx.try_read().unwrap();
            for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY {
                deliver_model_note(
                    &guard,
                    PendingNote {
                        note: model_note(&format!("m{i}")),
                        reapable: false,
                    },
                    &mut reapable_ids,
                );
            }
        }

        assert!(
            !has_agent(&ctx, "u1"),
            "the evicted driver completion must take its handle with it"
        );
        assert!(
            has_agent(&ctx, "a1"),
            "a metered handle is the model's to collect, whatever was evicted"
        );
        assert!(reapable_ids.is_empty(), "a reaped id is forgotten");
        let drained = ctx.try_read().unwrap().notification_queue.drain();
        assert!(
            drained
                .iter()
                .all(|note| note.id != "u1" && note.id != "a1")
        );
        assert_eq!(drained.last().unwrap().event, MESH_EVENTS_DROPPED_EVENT);
    }

    /// A peer note shaped like a completion and naming a live unmetered handle is not the
    /// driver's word about that handle, so its eviction reaps nothing even though the id
    /// would pass a supervisor lookup.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_evicted_peer_note_carrying_a_live_unmetered_handle_id_reaps_nothing() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        assert!(ctx.try_read().unwrap().declared_function_names.is_empty());
        register_handle(&ctx, "u1", true);
        let mut reapable_ids = HashSet::new();

        {
            let guard = ctx.try_read().unwrap();
            deliver_model_note(
                &guard,
                PendingNote {
                    note: mesh_notification(
                        AGENT_COMPLETED_EVENT,
                        "u1",
                        "envoy",
                        true,
                        String::new(),
                    ),
                    reapable: false,
                },
                &mut reapable_ids,
            );
            for i in 0..MESH_NOTIFICATION_QUEUE_CAPACITY {
                deliver_model_note(
                    &guard,
                    PendingNote {
                        note: model_note(&format!("m{i}")),
                        reapable: false,
                    },
                    &mut reapable_ids,
                );
            }
        }

        assert!(reapable_ids.is_empty());
        assert!(
            has_agent(&ctx, "u1"),
            "an evicted peer note must not reap the handle it names"
        );
        let drained = ctx.try_read().unwrap().notification_queue.drain();
        assert!(
            drained.iter().all(|note| note.id != "u1"),
            "the peer note was evicted"
        );
        assert_eq!(drained.last().unwrap().event, MESH_EVENTS_DROPPED_EVENT);
    }

    /// A peer's note is never grounds for touching the supervisor: a completion event
    /// name and a real handle id on a peer-origin note leave the handle alone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_peer_note_shaped_like_a_completion_reaps_nothing() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        assert!(ctx.try_read().unwrap().declared_function_names.is_empty());
        register_handle(&ctx, "a1", false);
        let driver = start_driver(&ctx);

        driver
            .handle()
            .push(peer_note(
                "deadbeef",
                "forged",
                Some(mesh_notification(
                    AGENT_COMPLETED_EVENT,
                    "a1",
                    "envoy",
                    true,
                    String::new(),
                )),
            ))
            .expect("queued");

        let landed = wait_for_note(&ctx, |note| note.id == "a1").await;
        assert_eq!(landed.event, AGENT_COMPLETED_EVENT);
        assert!(
            has_agent(&ctx, "a1"),
            "a peer-origin note must not reap a registered handle"
        );
        driver.stop().await;
    }

    /// The REPL clears the notifier before stopping the driver, so a completion emitted
    /// by a child winding down under `stop` takes the slot's fallback path rather than a
    /// printer nobody drains; the child is still waited for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_completion_emitted_during_stop_falls_back_past_the_cleared_notifier() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let app = Arc::clone(&ctx.try_read().unwrap().app);
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);
        let in_flight = Arc::clone(&driver.in_flight);

        let (spawn, observed) = cancellable_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;
        assert!(sink.lines().is_empty());

        app.mesh.clear_notifier();
        driver.stop().await;

        assert!(observed.load(Ordering::SeqCst));
        assert_eq!(in_flight.running(), 0);
        assert!(
            sink.lines().is_empty(),
            "nothing may reach the cleared printer: {:?}",
            sink.lines()
        );
    }

    /// The spawn half of the switch rule: a spawn queued while a turn holds the context
    /// must not carry the supervisor or queue that were current at queue time. The turn
    /// switches agents (cancel_recursive, queue swap, supervisor nulled) before releasing;
    /// the child must then register under a supervisor resolved at start time and report
    /// into the queue current at completion time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_queued_during_a_turn_that_switches_agents_resolves_the_new_context() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let old_supervisor = Arc::clone(ctx.try_read().unwrap().supervisor.as_ref().unwrap());
        let old_queue = Arc::clone(&ctx.try_read().unwrap().notification_queue);
        let driver = start_driver(&ctx);

        // A turn takes the context and, before letting go, performs the switch.
        let (locked_tx, locked_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let holder = {
            let ctx = Arc::clone(&ctx);
            thread::spawn(move || {
                let mut guard = ctx.write();
                locked_tx.send(()).unwrap();
                let _ = release_rx.recv();
                guard.supervisor.as_ref().unwrap().read().cancel_recursive();
                guard.notification_queue = Arc::new(NotificationQueue::new());
                guard.supervisor = None;
            })
        };
        locked_rx.recv().unwrap();

        let (spawn, ran) = flagged_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        sleep(IDLE_LOCK_RETRY_TICK * 3).await;
        assert!(
            !ran.load(Ordering::SeqCst),
            "no child may start during the turn"
        );

        drop(release_tx);
        holder.join().unwrap();

        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(completed.tool_or_agent, "envoy");
        assert_eq!(completed.channel, Channel::Mesh);
        assert!(ran.load(Ordering::SeqCst));
        assert!(
            old_queue.drain().is_empty(),
            "the completion must not land in the queue replaced by the switch"
        );
        assert!(
            old_supervisor.read().list_agents().is_empty(),
            "the child must not be registered on the supervisor the switch discarded"
        );
        let new_supervisor = ctx
            .read()
            .supervisor
            .clone()
            .expect("the driver installs a fresh top-level supervisor");
        assert!(
            !Arc::ptr_eq(&new_supervisor, &old_supervisor),
            "the driver must resolve the supervisor at start time, not hold the old one"
        );
        assert_eq!(
            new_supervisor.read().max_concurrent(),
            0,
            "the top-level supervisor keeps model-side spawning disabled"
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

    /// A peer flood that has emptied the bucket must not mute the driver's own
    /// completion. `Origin::Local` is never limited, so the human line and the model note
    /// of a child that finishes mid-flood both land, unfolded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_completion_lands_while_a_peer_flood_saturates_the_bucket() {
        let ctx = test_ctx(Some(Supervisor::new(4, 3)));
        let sink = install_sink(&ctx);
        let driver = start_driver(&ctx);

        let (spawn, go) = parked_spawn("envoy");
        driver.handle().spawn(spawn).ok().expect("queued");
        wait_until("the child to be registered", || has_active_tasks(&ctx)).await;

        // Drain the bucket and keep it empty with a flood from one peer.
        for i in 0..(IDLE_NOTIFY_BURST as usize * 10) {
            driver
                .handle()
                .push(peer_note(
                    "deadbeef",
                    &format!("flood {i}"),
                    Some(model_note("flood")),
                ))
                .expect("queued");
        }
        wait_until("the flood to be summarised", || {
            sink.lines()
                .iter()
                .any(|line| line.contains("more from deadbeef)"))
        })
        .await;

        go.send(()).unwrap();
        let completed = wait_for_note(&ctx, |note| note.event == "mesh_agent_completed").await;
        assert_eq!(completed.tool_or_agent, "envoy");
        assert_eq!(completed.status, "success");
        wait_until("the human completion line despite the empty bucket", || {
            sink.lines()
                .iter()
                .any(|line| line == "[mesh] envoy finished")
        })
        .await;
        assert!(
            !sink
                .lines()
                .iter()
                .any(|line| line.contains("finished") && line.contains("more from")),
            "a local completion is never folded: {:?}",
            sink.lines()
        );
        driver.stop().await;
    }
}
