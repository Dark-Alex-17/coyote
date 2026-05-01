# Phase 5 Implementation Plan: Tool Scope Pooling and Lifecycle

## Overview

Phase 5 turns the trivial no-pool `McpFactory` from Phase 1 Step 6.5 into a production-grade pooling layer with idle timeouts, a background reaper, health checks, and graceful shutdown integration. The architecture doesn't change — `McpFactory::acquire()` is still the only entry point, `Arc<McpServerHandle>` is still the reference type — but the factory now aggressively shares MCP subprocesses across scopes to keep warm-path latency near zero.

**Estimated effort:** ~1 week
**Risk:** Medium. The pooling logic has subtle ordering concerns (handle Drop → idle pool vs teardown → reaper eviction). Get those wrong and you leak processes or double-free.
**Depends on:** Phases 1–4 complete. Phase 4 is important because it's the first workload where pooling actually matters — CLI and REPL don't generate enough concurrent scope transitions to justify the complexity.

---

## Why Phase 5 Exists

After Phase 4 lands, the API server works correctly but has a performance problem: every API session activates its own MCP processes, and when the session closes, those processes tear down immediately. A realistic production workload — 20 concurrent users each sending a burst of requests — spawns and kills MCP subprocesses at an unsustainable rate. For servers like `github` that take 1–2 seconds to start (subprocess + stdio handshake + OAuth + `tools/list`), every API call adds visible cold-start latency.

The architectural framing for the fix was already designed in Phase 1 Step 6.5 and Phase 1's "MCP Lifecycle Policy" section:

1. **Layer 1: active Arc reference counting.** Already done in Phase 1. Scopes hold `Arc<McpServerHandle>`; the last drop triggers teardown.
2. **Layer 2: idle grace period.** Not yet implemented. After the last Arc drops, the handle moves to an idle pool with a timestamp instead of tearing down. A background reaper evicts entries that have been idle past the configured threshold.
3. **Acquisition order.** `acquire(key)` checks the active map first, then the idle pool (revival = zero latency), then spawns fresh.

Phase 5 implements Layer 2 + the reaper + the revival logic + the health check + graceful shutdown integration. No changes to the caller API. No changes to any other phase's code.

**This is a pure optimization phase.** Correctness is unchanged; only performance improves.

---

## The Architecture After Phase 5

```
      ┌─────────────────────────────────────────────────┐
      │                  McpFactory                     │
      │                                                 │
      │   ┌──────────────┐       ┌──────────────────┐   │
      │   │    active:   │       │      idle:       │   │
      │   │  HashMap<K,  │       │  HashMap<K,      │   │
      │   │    Weak<H>>  │       │  IdleEntry>      │   │
      │   └──────┬───────┘       └────────┬─────────┘   │
      │          │                        │             │
      │          │ upgrade()              │ remove()    │
      │          │                        │             │
      │          ▼                        ▼             │
      │   ┌──────────────────────────────────────┐      │
      │   │           acquire(key):              │      │
      │   │    1. Try active.upgrade() → share   │      │
      │   │    2. Try idle.remove() → revive     │      │
      │   │    3. Spawn fresh subprocess         │      │
      │   └──────────────────────────────────────┘      │
      │                                                 │
      │   ┌──────────────────────────────────────┐      │
      │   │  Background reaper (tokio::spawn):    │     │
      │   │    every cleanup_interval:            │     │
      │   │      walk idle, evict stale entries   │     │
      │   │      (optional: health check)         │     │
      │   └──────────────────────────────────────┘      │
      └─────────────────────────────────────────────────┘
                               │
                               │  Arc<McpServerHandle>
                               ▼
                  ┌────────────────────────┐
                  │  scope's ToolScope     │
                  │  (CLI/REPL/API request)│
                  └────────────────────────┘
```

---

## Core Types

### `McpFactory` (expanded)

```rust
pub struct McpFactory {
    active: Mutex<HashMap<McpServerKey, Weak<McpServerHandleInner>>>,
    idle: Mutex<HashMap<McpServerKey, IdleEntry>>,
    config: McpFactoryConfig,
    shutdown: Arc<AtomicBool>,
    reaper_handle: Mutex<Option<JoinHandle<()>>>,
}

struct IdleEntry {
    handle: Arc<McpServerHandleInner>,
    idle_since: Instant,
    last_health_check: Option<Instant>,
}

pub struct McpFactoryConfig {
    pub idle_timeout: Duration,
    pub cleanup_interval: Duration,
    pub max_idle_servers: Option<usize>,
    pub health_check: Option<HealthCheckPolicy>,
}

pub struct HealthCheckPolicy {
    pub interval: Duration,
    pub timeout: Duration,
    pub on_failure: HealthFailureAction,
}

pub enum HealthFailureAction {
    Evict,
    EvictAndLog,
    LogOnly,
}
```

The factory grows three new pieces of state compared to Phase 1's stub:

- **`idle` map** — stores handles that nobody currently owns but that we've decided to keep warm.
- **`shutdown` flag** — tells the reaper to exit and prevents new inserts into `idle` during drain.
- **`reaper_handle`** — the `JoinHandle` of the background task, awaited during graceful shutdown.

### `McpServerHandle` (refined)

Phase 1's `Arc<McpServerHandle>` becomes `Arc<McpServerHandleInner>`, and we add a `Drop` impl on the inner type that handles the "return to idle pool" logic:

```rust
pub struct McpServerHandleInner {
    key: McpServerKey,
    service: RwLock<RunningService<RoleClient, ()>>,
    factory: Weak<McpFactory>,
    spawned_at: Instant,
    returning_to_pool: AtomicBool,
}

impl Drop for McpServerHandleInner {
    fn drop(&mut self) {
        // If we're already returning to pool (revived from idle),
        // don't re-insert — the factory is handling it.
        if self.returning_to_pool.load(Ordering::Acquire) {
            return;
        }

        let Some(factory) = self.factory.upgrade() else {
            // Factory is gone — just let the service die via its own drop.
            return;
        };

        if factory.shutdown.load(Ordering::Acquire) {
            // Shutting down — don't put it back in idle, just die.
            return;
        }

        // Take ownership of self.service and move to idle pool.
        // This requires unsafe or a different ownership structure — see
        // "The Drop trick" section below.
        factory.return_to_idle(self);
    }
}
```

**The Drop trick** — the issue is that `Drop::drop` can't actually move `self`'s fields out without `unsafe`, but we need to move the `RunningService` into the idle pool. The clean solution is to wrap the service in an `Option<RunningService>`:

```rust
pub struct McpServerHandleInner {
    key: McpServerKey,
    service: Mutex<Option<RunningService<RoleClient, ()>>>,  // Option so we can take() in Drop
    factory: Weak<McpFactory>,
    spawned_at: Instant,
}

impl Drop for McpServerHandleInner {
    fn drop(&mut self) {
        let Some(factory) = self.factory.upgrade() else { return; };
        if factory.shutdown.load(Ordering::Acquire) { return; }

        // Take the service out. After this, self.service is None.
        let service = match self.service.get_mut().take() {
            Some(s) => s,
            None => return,  // Already taken — e.g., by shutdown drain.
        };

        // Spawn a task to move it into the idle pool (can't await in Drop).
        let key = self.key.clone();
        let factory = factory.clone();
        tokio::spawn(async move {
            factory.accept_returning_handle(key, service).await;
        });
    }
}
```

This has the right shape but introduces a subtle race: the `tokio::spawn` inside `Drop` runs asynchronously, so if a new `acquire(key)` arrives between the Drop and the spawned task completing, it won't find the handle in `idle` yet and will spawn a fresh subprocess. That's acceptable — it's slightly wasteful but not incorrect, and the race window is microseconds.

An alternative that avoids the race: use a dedicated `return_tx: mpsc::UnboundedSender<ReturningHandle>` on the factory, push synchronously into it from Drop, and a single "idle manager" task owns the idle map. This is cleaner because the idle map only mutates from one task, but it adds a coordination point. **Recommendation: start with the `tokio::spawn` approach; switch to the mpsc pattern only if the race causes visible issues.**

### `McpServerHandle` (the public Arc wrapper)

```rust
pub struct McpServerHandle(Arc<McpServerHandleInner>);

impl McpServerHandle {
    pub async fn call_tool(&self, tool: &str, args: Value) -> Result<ToolResult> {
        let guard = self.0.service.lock().await;
        let service = guard.as_ref().ok_or(McpError::HandleDrained)?;
        service.call_tool(tool, args).await
    }

    pub async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        let guard = self.0.service.lock().await;
        let service = guard.as_ref().ok_or(McpError::HandleDrained)?;
        service.list_tools().await
    }
}

impl Clone for McpServerHandle {
    fn clone(&self) -> Self { Self(self.0.clone()) }
}
```

Callers get a `McpServerHandle` (which is `Arc<Inner>` internally) from `acquire()`. Cloning is cheap. Dropping the last clone fires the `Drop` on `Inner`, which returns the underlying service to the idle pool or kills it.

---

## The `acquire` Path

Three cases in order:

```rust
impl McpFactory {
    pub async fn acquire(&self, key: &McpServerKey) -> Result<McpServerHandle> {
        // Case 1: Active share
        {
            let active = self.active.lock();
            if let Some(weak) = active.get(key) {
                if let Some(inner) = weak.upgrade() {
                    metrics::mcp_acquire_hit_active();
                    return Ok(McpServerHandle(inner));
                }
                // Weak is dangling; let it fall through.
            }
        }

        // Case 2: Revive from idle
        {
            let mut idle = self.idle.lock();
            if let Some(entry) = idle.remove(key) {
                metrics::mcp_acquire_hit_idle(entry.idle_since.elapsed());
                let inner = self.revive_idle_entry(entry);
                // Re-register in active map.
                self.active.lock().insert(key.clone(), Arc::downgrade(&inner));
                return Ok(McpServerHandle(inner));
            }
        }

        // Case 3: Spawn fresh
        metrics::mcp_acquire_miss();
        let inner = self.spawn_new(key).await?;
        self.active.lock().insert(key.clone(), Arc::downgrade(&inner));
        Ok(McpServerHandle(inner))
    }

    fn revive_idle_entry(&self, entry: IdleEntry) -> Arc<McpServerHandleInner> {
        // Wrap the handle in a fresh Arc. The IdleEntry held an Arc; we're
        // just transferring ownership here.
        entry.handle
    }

    async fn spawn_new(&self, key: &McpServerKey) -> Result<Arc<McpServerHandleInner>> {
        let spec = self.resolve_spec(key)?;
        let service = McpServer::start(&spec).await?;
        let inner = Arc::new(McpServerHandleInner {
            key: key.clone(),
            service: Mutex::new(Some(service)),
            factory: Arc::downgrade(&self.weak_self()),
            spawned_at: Instant::now(),
        });
        Ok(inner)
    }
}
```

**Concurrency in `acquire`:** the `active.lock()` critical section is short — just a hashmap lookup and maybe an insert. It never holds across an `.await`. The `idle.lock()` critical section is equally short. The `spawn_new` path is the expensive one (subprocess spawn + stdio handshake + `tools/list`), and it runs OUTSIDE any lock. This means two concurrent `acquire(key)` calls that both miss can both spawn fresh, producing two subprocesses for the same key briefly. Once both register themselves in `active`, the second insert clobbers the first, and the first handle's Drop returns it to the idle pool. The net effect is one "wasted" spawn per race, which is acceptable.

If you want to eliminate the race entirely, add a per-key `OnceCell`-style coordinator:

```rust
pending: Mutex<HashMap<McpServerKey, broadcast::Receiver<Arc<McpServerHandleInner>>>>,
```

A caller that misses both active and idle checks `pending` — if another task is already spawning, it subscribes to the broadcast and waits. The first spawner publishes the result. Clean but adds a layer of complexity. Start simple; add this if races become a problem in practice.

---

## The Reaper Task

```rust
async fn reaper_loop(factory: Arc<McpFactory>) {
    let mut ticker = interval(factory.config.cleanup_interval);
    loop {
        ticker.tick().await;

        if factory.shutdown.load(Ordering::Acquire) {
            info!("Reaper exiting (shutdown requested)");
            return;
        }

        factory.evict_stale_idle().await;

        if let Some(policy) = &factory.config.health_check {
            factory.run_health_checks(policy).await;
        }
    }
}

impl McpFactory {
    async fn evict_stale_idle(&self) {
        let now = Instant::now();
        let timeout = self.config.idle_timeout;

        // Phase 1: collect stale keys while holding the lock briefly.
        let stale: Vec<McpServerKey> = {
            let idle = self.idle.lock();
            idle.iter()
                .filter(|(_, entry)| now.duration_since(entry.idle_since) >= timeout)
                .map(|(k, _)| k.clone())
                .collect()
        };

        // Phase 2: remove them from the idle map and terminate.
        for key in stale {
            let entry = {
                let mut idle = self.idle.lock();
                idle.remove(&key)
            };
            if let Some(entry) = entry {
                self.terminate_idle_handle(entry).await;
                metrics::mcp_idle_evicted();
            }
        }

        // Phase 3: enforce max_idle_servers cap via LRU.
        if let Some(max) = self.config.max_idle_servers {
            self.enforce_max_idle(max).await;
        }
    }

    async fn enforce_max_idle(&self, max: usize) {
        let victims: Vec<(McpServerKey, Instant)> = {
            let idle = self.idle.lock();
            if idle.len() <= max {
                return;
            }
            let mut entries: Vec<_> = idle.iter()
                .map(|(k, v)| (k.clone(), v.idle_since))
                .collect();
            entries.sort_by_key(|(_, t)| *t);  // oldest first
            entries.into_iter().take(idle.len() - max).collect()
        };

        for (key, _) in victims {
            let entry = self.idle.lock().remove(&key);
            if let Some(entry) = entry {
                self.terminate_idle_handle(entry).await;
                metrics::mcp_lru_evicted();
            }
        }
    }

    async fn terminate_idle_handle(&self, entry: IdleEntry) {
        // Take the service out of the Arc<Inner> and cancel it.
        // At this point, there are no other Arc refs — it's just us.
        if let Ok(inner) = Arc::try_unwrap(entry.handle) {
            if let Some(service) = inner.service.into_inner().take() {
                service.cancel().await.ok();
            }
        }
        // If try_unwrap fails, something else grabbed a ref — skip, it'll
        // return to idle on its own Drop.
    }
}
```

**Ordering:** `cleanup_interval` runs on a tokio `interval` ticker. Default is 30 seconds. Setting it too low wastes CPU; too high means idle servers linger slightly longer than `idle_timeout`. A tolerance of `idle_timeout + cleanup_interval` worst case is the tradeoff.

**`Arc::try_unwrap`** is the key to safe teardown. By the time the reaper decides to evict an entry, the only Arc to that `Inner` is the one in the `IdleEntry`. Any subsequent `acquire(key)` would have removed it from the idle map first. So `try_unwrap` should always succeed — but if it doesn't (e.g., because of the Drop-race described earlier), we just skip this eviction and catch it next cycle.

---

## The Health Check Path

```rust
impl McpFactory {
    async fn run_health_checks(&self, policy: &HealthCheckPolicy) {
        let now = Instant::now();
        let candidates: Vec<McpServerKey> = {
            let idle = self.idle.lock();
            idle.iter()
                .filter(|(_, entry)| {
                    entry.last_health_check
                        .map(|t| now.duration_since(t) >= policy.interval)
                        .unwrap_or(true)
                })
                .map(|(k, _)| k.clone())
                .collect()
        };

        for key in candidates {
            let handle = {
                let idle = self.idle.lock();
                idle.get(&key).map(|e| e.handle.clone())
            };
            let Some(handle) = handle else { continue };

            let result = tokio::time::timeout(
                policy.timeout,
                self.ping_handle(&handle),
            ).await;

            match result {
                Ok(Ok(())) => {
                    let mut idle = self.idle.lock();
                    if let Some(entry) = idle.get_mut(&key) {
                        entry.last_health_check = Some(now);
                    }
                    metrics::mcp_health_ok();
                }
                Ok(Err(e)) | Err(_) => {
                    metrics::mcp_health_failed();
                    match policy.on_failure {
                        HealthFailureAction::Evict | HealthFailureAction::EvictAndLog => {
                            let entry = self.idle.lock().remove(&key);
                            if let Some(entry) = entry {
                                self.terminate_idle_handle(entry).await;
                            }
                            if matches!(policy.on_failure, HealthFailureAction::EvictAndLog) {
                                warn!(key = ?key, error = ?e, "evicted unhealthy MCP server");
                            }
                        }
                        HealthFailureAction::LogOnly => {
                            warn!(key = ?key, error = ?e, "MCP server failed health check");
                        }
                    }
                }
            }
        }
    }

    async fn ping_handle(&self, handle: &Arc<McpServerHandleInner>) -> Result<()> {
        let guard = handle.service.lock().await;
        let service = guard.as_ref().ok_or(McpError::HandleDrained)?;
        // `list_tools` is cheap and standard across all MCP servers.
        service.list_tools().await?;
        Ok(())
    }
}
```

Health checks are optional (`health_check: None` disables them). When enabled, they run on the same interval as the reaper and only check idle entries whose last check was more than `policy.interval` ago. This avoids hammering servers that are currently in active use.

---

## Graceful Shutdown Integration

The factory coordinates with the process shutdown signal (Ctrl-C for CLI, SIGTERM for server mode). When shutdown fires:

1. Set `factory.shutdown = true`. Any subsequent `acquire()` still works but new handles won't be returned to idle on Drop.
2. Cancel the reaper's `JoinHandle`.
3. Drain the idle pool: walk it, call `terminate_idle_handle` for each entry.
4. Wait for active handles to drop naturally as their scopes finish. If there's a shutdown grace period (Phase 4's `shutdown_grace_seconds`), bound the wait with that.

```rust
impl McpFactory {
    pub async fn shutdown(&self, grace: Duration) {
        info!("McpFactory entering shutdown");
        self.shutdown.store(true, Ordering::Release);

        // Stop the reaper.
        if let Some(handle) = self.reaper_handle.lock().take() {
            handle.abort();
            let _ = handle.await;
        }

        // Drain the idle pool immediately.
        let idle_entries: Vec<IdleEntry> = {
            let mut idle = self.idle.lock();
            idle.drain().map(|(_, v)| v).collect()
        };
        for entry in idle_entries {
            self.terminate_idle_handle(entry).await;
        }

        // Wait for active scopes to release their handles.
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.active_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Force-terminate any remaining active handles.
        let remaining = self.active_count();
        if remaining > 0 {
            warn!(count = remaining, "force-terminating MCP servers after grace period");
            self.force_terminate_active().await;
        }

        info!("McpFactory shutdown complete");
    }

    fn active_count(&self) -> usize {
        let active = self.active.lock();
        active.values().filter(|w| w.strong_count() > 0).count()
    }

    async fn force_terminate_active(&self) {
        // Walk the active map, upgrade the weak refs, and call cancel
        // directly on the underlying service. This is a last resort.
        let handles: Vec<Arc<McpServerHandleInner>> = {
            let active = self.active.lock();
            active.values().filter_map(|w| w.upgrade()).collect()
        };
        for handle in handles {
            if let Ok(inner) = Arc::try_unwrap(handle) {
                if let Some(service) = inner.service.into_inner().take() {
                    service.cancel().await.ok();
                }
            }
            // If try_unwrap fails, we can't force-kill without leaking
            // the service. Log and move on.
        }
    }
}
```

Phase 4's `serve()` function calls `factory.shutdown(grace)` after the axum server has stopped accepting new requests. This chains cleanly: axum drains requests → factory drains scopes → factory drains idle pool → process exits.

---

## Configuration

Add to `config.yaml`:

```yaml
mcp_pool:
  idle_timeout_seconds: 300          # how long idle servers stay warm (default: 300 for --serve, 0 for CLI/REPL)
  cleanup_interval_seconds: 30       # how often the reaper runs
  max_idle_servers: 50               # LRU cap (null = unbounded)
  health_check:
    interval_seconds: 60
    timeout_seconds: 5
    on_failure: EvictAndLog          # or Evict, LogOnly
```

Per-server overrides live in `functions/mcp.json`:

```json
{
  "github":     { "command": "...", "idle_timeout_seconds": 900 },
  "filesystem": { "command": "...", "idle_timeout_seconds": 60  },
  "jira":       { "command": "...", "idle_timeout_seconds": 300 }
}
```

The per-server override wins over the global config. The resolution is: look up the server spec, check if it has `idle_timeout_seconds`, use that if present, else use `mcp_pool.idle_timeout_seconds`, else use the mode default (0 for CLI/REPL, 300 for `--serve`).

**Mode defaults** are critical because they preserve Phase 1 Step 6.5's behavior. CLI and REPL users get `idle_timeout = 0`, which means the factory behaves exactly like the no-pool version — drop = terminate. The pool is inert for single-user scenarios. Only `--serve` mode turns it on by default. This avoids regressing REPL users who don't want MCP subprocess churn quirks.

```rust
pub fn default_idle_timeout(mode: WorkingMode) -> Duration {
    match mode {
        WorkingMode::Cmd | WorkingMode::Repl => Duration::ZERO,
        WorkingMode::Api => Duration::from_secs(300),
    }
}
```

---

## Metrics

Phase 5 is the right time to add basic observability counters. They're cheap and the factory is where the interesting operational questions live.

```rust
mod metrics {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static MCP_SPAWNED: AtomicU64 = AtomicU64::new(0);
    pub static MCP_ACQUIRE_ACTIVE_HIT: AtomicU64 = AtomicU64::new(0);
    pub static MCP_ACQUIRE_IDLE_HIT: AtomicU64 = AtomicU64::new(0);
    pub static MCP_ACQUIRE_MISS: AtomicU64 = AtomicU64::new(0);
    pub static MCP_IDLE_EVICTED: AtomicU64 = AtomicU64::new(0);
    pub static MCP_LRU_EVICTED: AtomicU64 = AtomicU64::new(0);
    pub static MCP_HEALTH_OK: AtomicU64 = AtomicU64::new(0);
    pub static MCP_HEALTH_FAILED: AtomicU64 = AtomicU64::new(0);

    pub fn mcp_acquire_hit_active() { MCP_ACQUIRE_ACTIVE_HIT.fetch_add(1, Ordering::Relaxed); }
    pub fn mcp_acquire_hit_idle(age: Duration) {
        MCP_ACQUIRE_IDLE_HIT.fetch_add(1, Ordering::Relaxed);
        // In a real metrics system, record a histogram of age for revival latency.
    }
    pub fn mcp_acquire_miss() { MCP_ACQUIRE_MISS.fetch_add(1, Ordering::Relaxed); }
    pub fn mcp_spawned() { MCP_SPAWNED.fetch_add(1, Ordering::Relaxed); }
    pub fn mcp_idle_evicted() { MCP_IDLE_EVICTED.fetch_add(1, Ordering::Relaxed); }
    pub fn mcp_lru_evicted() { MCP_LRU_EVICTED.fetch_add(1, Ordering::Relaxed); }
    pub fn mcp_health_ok() { MCP_HEALTH_OK.fetch_add(1, Ordering::Relaxed); }
    pub fn mcp_health_failed() { MCP_HEALTH_FAILED.fetch_add(1, Ordering::Relaxed); }

    pub fn snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            spawned: MCP_SPAWNED.load(Ordering::Relaxed),
            acquire_active_hit: MCP_ACQUIRE_ACTIVE_HIT.load(Ordering::Relaxed),
            acquire_idle_hit: MCP_ACQUIRE_IDLE_HIT.load(Ordering::Relaxed),
            acquire_miss: MCP_ACQUIRE_MISS.load(Ordering::Relaxed),
            idle_evicted: MCP_IDLE_EVICTED.load(Ordering::Relaxed),
            lru_evicted: MCP_LRU_EVICTED.load(Ordering::Relaxed),
            health_ok: MCP_HEALTH_OK.load(Ordering::Relaxed),
            health_failed: MCP_HEALTH_FAILED.load(Ordering::Relaxed),
        }
    }
}
```

Expose the snapshot via `GET /v1/info/mcp` in the API server (piggybacks on Phase 4's `/v1/info`). CLI/REPL users can inspect via a new `.info mcp` dot-command.

**Derived metrics worth computing:**
- Hit rate = `(active_hit + idle_hit) / (active_hit + idle_hit + miss)` — should be >0.9 for a well-tuned pool.
- Revival latency distribution — how old were idle entries when revived? Informs tuning of `idle_timeout`.
- Eviction rate — how often is the pool churning?

None of this is Prometheus-compatible yet; that integration is a follow-up. For Phase 5, plain counters are enough to diagnose issues.

---

## Migration Strategy

### Step 1: Expand `McpFactory` to support the idle pool

Add the `idle` map, `shutdown` flag, and `reaper_handle` fields. Keep the existing `active` map. Don't change any caller code yet.

Implement `acquire()` with the three-case logic (active → idle → spawn). At this point the idle pool is always empty because nothing puts anything in it, so the logic reduces to Phase 1's behavior. Tests should still pass.

**Verification:** `cargo check` + existing Phase 1 tests pass.

### Step 2: Implement `Drop` on `McpServerHandleInner` with return-to-idle

Switch `service` to `Mutex<Option<RunningService>>`. Implement `Drop` that spawns a task to call `factory.accept_returning_handle(key, service)`. The factory method inserts into `idle`.

At this point, dropped handles start populating the idle pool. The reaper isn't running yet, so idle entries accumulate without bound.

**Verification:** Manual test: acquire a handle, drop it, assert the idle map now has the entry. Then acquire the same key again and assert it comes from idle (not a fresh spawn).

### Step 3: Implement the reaper task

Add `reaper_loop` and `evict_stale_idle`. Start the reaper in `McpFactory::new()` via `tokio::spawn`, store the `JoinHandle`. Default `idle_timeout` based on working mode.

**Verification:** Unit test with a tiny timeout (e.g., 100ms) — acquire, drop, wait 200ms, assert the idle map is empty. Use a mock MCP server (or a no-op `RunningService` for tests).

### Step 4: Add configuration plumbing

Parse `mcp_pool` from `config.yaml` into `McpFactoryConfig`. Parse per-server `idle_timeout_seconds` overrides from `functions/mcp.json`. Wire everything through `AppState::init()`.

**Verification:** Config tests that verify defaults, overrides, and mode-specific behavior.

### Step 5: Implement health checks

Add `run_health_checks`, `ping_handle`, and the `HealthCheckPolicy` config. Wire into the reaper loop. Default is `None` (disabled).

**Verification:** Unit test with a mock MCP server that returns an error on `list_tools` after N calls — verify the factory evicts it and logs.

### Step 6: Implement graceful shutdown

Add `McpFactory::shutdown(grace)`. Wire into Phase 4's `serve()` shutdown sequence and into the CLI/REPL exit path (for clean subprocess termination).

**Verification:** Start the API server, send several requests to warm up the pool, send SIGTERM, verify all MCP subprocesses terminate within the grace period (use `ps` or process tree inspection).

### Step 7: Expose metrics

Add the atomic counters, the snapshot function, and the `.info mcp` dot-command. Add `GET /v1/info/mcp` handler in the API server.

**Verification:** `.info mcp` shows sensible numbers after a few REPL turns. `/v1/info/mcp` returns JSON. Hit rate climbs over time as the pool warms.

### Step 8: Load testing

Write a test harness that spins up `--serve` mode and fires 100 concurrent completion requests, each using a mix of 2–3 MCP servers, across a pool of 10 different server configurations. Assert:

- No test failures
- No orphaned subprocesses (check `ps` before and after)
- MCP spawn count stays low (hit rate >80%)
- p99 latency for the warm path is <200ms (allowing for LLM latency)

This is the practical validation that Phase 5 delivered on its performance promise.

**Verification:** Load test passes. Metrics snapshot shows expected hit rate.

### Step 9: Document tuning knobs

Update `docs/function-calling/MCP-SERVERS.md` with the new config options and tuning guidance:

- How to choose `idle_timeout` for different workloads
- When to enable health checks
- How to read the metrics
- What the `max_idle_servers` cap protects against

Add an "MCP Pool Lifecycle" section to `docs/REST-API-ARCHITECTURE.md` describing the production topology.

---

## Risks and Watch Items

| Risk | Severity | Mitigation |
|---|---|---|
| **Drop-race between `acquire` and `return_to_idle`** | Medium | The `tokio::spawn` inside Drop runs asynchronously. If an `acquire(key)` fires between Drop and the spawned task completing, it misses the idle pool and spawns fresh. Acceptable for correctness; monitor hit rate metrics, switch to the mpsc coordinator pattern if races show up in production. |
| **`Arc::try_unwrap` failing in `terminate_idle_handle`** | Medium | If something holds an extra Arc to an idle entry (shouldn't happen under normal flow), `try_unwrap` returns `Err` and we skip eviction. The entry stays in the idle map forever. Mitigation: log every such failure with a WARN. Write a test that verifies the shape never produces such extra refs. |
| **`tokio::time::interval` drift** | Low | `interval` drifts if the system is under load — a tick can be delayed. This means `cleanup_interval` is a lower bound, not a guarantee. For a 30-second interval this is irrelevant; document it. |
| **Reaper task panic** | Medium | If the reaper task panics (unreachable under normal flow, but possible under library bugs), the pool stops cleaning up. Mitigation: wrap the reaper body in `tokio::task::JoinHandle` inspection, restart on failure. Add a metric for reaper restarts. |
| **MCP server state on revival** | High | Reviving a server from idle assumes it's still in the same state it was when it went idle. Most MCP servers are stateless (they reload config on each tool call), but some might maintain in-memory state that's stale after 5 minutes of idle. Mitigation: health checks during idle provide an early warning; document that pool idle is only safe for stateless servers. |
| **Credential rotation** | High | If the user rotates their GitHub token (or any MCP-server-side credential), the idle pool entries hold the old credential baked into the subprocess env. A rotation requires restarting affected MCP servers. Mitigation: expose a `.reload mcp` REPL command and `POST /v1/mcp/reload` API that clears the idle pool, forcing fresh spawns with the new credentials on next acquire. |
| **Per-server timeout resolution** | Low | The `idle_timeout` lookup (per-server override → pool default → mode default) happens at `return_to_idle` time. Changing config at runtime won't affect already-idle entries. Document this; config reload flushes idle pool. |
| **`max_idle_servers` thrashing** | Medium | If the cap is set too low relative to the working set, every new `acquire` evicts an old idle entry, destroying the hit rate. Default to 50, document the signal: rising eviction rate + falling hit rate = raise the cap. |
| **Subprocess leak on factory drop** | High | If `AppState` (which owns `McpFactory`) drops without calling `shutdown()`, the idle pool Arc holds die, their Drops run, but the factory's Weak self-ref is already dead so nothing puts them back in idle — they just terminate via `RunningService::drop`. Verify this actually fires cleanly (not via the tokio::spawn hack). Add a test. |

---

## What Phase 5 Does NOT Do

- **No LLM response caching.** The factory pools MCP subprocesses, not LLM responses.
- **No distributed pooling.** A single factory instance owns its pool. Running multiple Loki server instances means each has its own pool; MCP processes are not shared across hosts.
- **No background server restart on crash.** If an MCP subprocess dies while idle, the reaper's health check evicts it; the next `acquire` spawns fresh. There's no "always keep N warm" preflight.
- **No OAuth token refresh for MCP.** If a server uses OAuth and its token expires during an idle period, the next `acquire` gets an expired handle. The server must handle its own refresh, or the user must rotate and `.reload mcp`.
- **No Prometheus integration.** Plain atomic counters; Prometheus support is a follow-up.
- **No adaptive tuning.** `idle_timeout` is a fixed config value, not auto-adjusted based on usage patterns.
- **No cross-process coordination.** Two Loki processes running `--serve` on the same host each have independent pools. They can't share MCP subprocesses across processes.
- **No changes to the factory's public API.** `acquire()` still takes `&McpServerKey`, still returns `McpServerHandle`. Callers don't notice Phase 5 happened.

The sole goal of Phase 5 is: **make the warm path free by keeping recently-used MCP subprocesses alive, with automatic eviction of stale ones, a background reaper, health checks, and graceful shutdown integration.**

---

## Entry Criteria (from Phase 4)

- [ ] API server runs in production-like conditions
- [ ] Concurrent request handling verified by integration tests
- [ ] `McpFactory::acquire()` is the only MCP acquisition path
- [ ] Phase 4's integration test suite passes
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean

## Exit Criteria (Phase 5 complete)

- [ ] `McpFactory` has the idle map and reaper task
- [ ] `McpServerHandleInner::Drop` returns handles to the idle pool instead of terminating
- [ ] Reaper evicts idle entries past `idle_timeout`
- [ ] `max_idle_servers` LRU cap enforced
- [ ] Optional health checks working and configurable
- [ ] Per-server `idle_timeout_seconds` overrides parsed and respected
- [ ] Mode-specific defaults (CLI/REPL = 0, API = 300) preserve pre-Phase-5 behavior
- [ ] Graceful shutdown drains the pool within the grace period
- [ ] Metrics counters exposed via `.info mcp` and `GET /v1/info/mcp`
- [ ] Load test shows hit rate >0.8 and no orphaned subprocesses
- [ ] `docs/function-calling/MCP-SERVERS.md` documents the pool config
- [ ] `docs/REST-API-ARCHITECTURE.md` "MCP Pool Lifecycle" section updated
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean
- [ ] Phase 6 (production hardening) can proceed
