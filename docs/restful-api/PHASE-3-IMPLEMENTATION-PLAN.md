# Phase 3 Implementation Plan: SessionStore Abstraction

## Overview

Phase 3 extracts session persistence behind a trait so that CLI, REPL, and the future API server all resolve sessions through the same interface. The file-based YAML storage that exists today remains the only implementation in Phase 3 — no database, no schema migration, no new on-disk format. What changes is that session identity becomes **UUID-primary with optional name-based aliases**, direct `std::fs::write` calls disappear from `Session::save()`, and concurrent access to the same session is properly serialized.

After Phase 3, Phase 4 (REST API) can plug in without touching any persistence code: `POST /v1/sessions` returns a UUID, subsequent requests address sessions by that UUID, and CLI/REPL users continue typing `.session my-project` without noticing the internal change.

**Estimated effort:** ~3–5 days
**Risk:** Low. Storage semantics don't change; we're re-shaping the API surface around existing YAML files.
**Depends on:** Phase 1 complete, Phase 2 complete (Engine needs to call through the new store, not raw `Session::load`).

---

## Why This Phase Exists

Today's `Session::load()` and `Session::save()` embed the file layout, the filename-is-the-identity assumption, and the absence of concurrency control directly in the type. Three things break when you try to run this in a multi-tenant server:

1. **No UUID identity.** Two API clients both start a "project" session and collide on the filename. You can't safely let clients name sessions freely.

2. **No concurrency control.** Two concurrent requests to the same session do `load → mutate → save` with no coordination. The later save clobbers the earlier one's changes.

3. **No abstraction seam.** Every callsite computes paths itself via `Config::session_file(name)` and calls `Session::load()` / `.save()` directly. There's no single place to swap in alternate storage, add caching, or instrument persistence.

Phase 3 fixes all three without breaking anything users currently do.

---

## The Architecture After Phase 3

```
┌────────┐ ┌────────┐ ┌────────┐
│  CLI   │ │  REPL  │ │  API   │  (Phase 4)
└───┬────┘ └───┬────┘ └───┬────┘
    └──────────┼──────────┘
               ▼
    ┌──────────────────────┐
    │       Engine         │
    └──────────┬───────────┘
               ▼
    ┌──────────────────────┐
    │  SessionStore trait  │
    └──────────┬───────────┘
               ▼
    ┌──────────────────────┐
    │  FileSessionStore    │   (Phase 3: the only impl)
    │  — UUID primary      │
    │  — name alias index  │
    │  — per-session mutex │
    │  — atomic writes     │
    └──────────┬───────────┘
               ▼
    ~/.config/loki/sessions/
      by-id/<uuid>/state.yaml
      by-name/<alias> → <uuid>  (text file containing the UUID)
      agents/<agent>/sessions/
        by-id/<uuid>/state.yaml
        by-name/<alias> → <uuid>
```

---

## Core Types

### `SessionId`

```rust
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl SessionId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn as_uuid(&self) -> Uuid { self.0 }
    pub fn to_string(&self) -> String { self.0.to_string() }
    pub fn parse(s: &str) -> Result<Self, SessionIdError> { /* ... */ }
}
```

UUID v4 by default. Newtype so we can't accidentally pass arbitrary strings where a session ID is expected, and so the on-disk format can evolve without breaking callers.

### `SessionAlias`

```rust
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub struct SessionAlias(String);

impl SessionAlias {
    pub fn new(s: impl Into<String>) -> Result<Self, AliasError>;
    pub fn as_str(&self) -> &str { &self.0 }
}
```

Wraps the human-readable names users type in `.session my-project`. Validation rejects path traversal (`..`), slashes, null bytes, and anything that would produce an invalid filename. This is the CLI/REPL compatibility layer — existing `sessions/my-project.yaml` files continue to work, the alias system just maps them to auto-generated UUIDs on first access.

### `SessionHandle`

```rust
pub struct SessionHandle {
    id: SessionId,
    alias: Option<SessionAlias>,
    is_agent: Option<String>,
    state: Arc<tokio::sync::Mutex<Session>>,
    store: Arc<dyn SessionStore>,
    dirty: Arc<AtomicBool>,
}

impl SessionHandle {
    pub fn id(&self) -> SessionId { self.id }
    pub fn alias(&self) -> Option<&SessionAlias> { self.alias.as_ref() }
    pub async fn lock(&self) -> SessionGuard<'_>;
    pub fn mark_dirty(&self);
    pub async fn save(&self) -> Result<(), StoreError>;
    pub async fn rename(&mut self, new_alias: SessionAlias) -> Result<(), StoreError>;
}

pub struct SessionGuard<'a> {
    session: MutexGuard<'a, Session>,
    handle: &'a SessionHandle,
}

impl SessionGuard<'_> {
    pub fn get(&self) -> &Session { &self.session }
    pub fn get_mut(&mut self) -> &mut Session {
        self.handle.mark_dirty();
        &mut self.session
    }
}
```

A `SessionHandle` is what callers pass around. It wraps:
- The stable `SessionId` (never changes after creation)
- An optional `SessionAlias` (can be renamed; users see this in `.info session`)
- An optional `is_agent` marker so the store knows which directory to read/write
- A shared `Arc<Mutex<Session>>` that serializes access within the process
- A backpointer to the store so `save()`, `rename()`, etc. work without the caller knowing the storage type
- A dirty flag that auto-sets on `get_mut()` and clears after successful save

The `lock()` / `SessionGuard` pattern is important: it makes the "you must lock before touching state" rule compiler-enforced. Today's code mutates `Config.session` freely because the whole `Config` is behind an `RwLock`. After Phase 3, mutating a session requires going through `handle.lock().await.get_mut()`, which acquires the per-session mutex. Two concurrent requests to the same session serialize automatically.

### `SessionStore` trait

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Create a new session. If `alias` is provided, register it in the
    /// alias index. Fails with AliasInUse if the alias already exists.
    async fn create(
        &self,
        agent: Option<&str>,
        alias: Option<SessionAlias>,
        initial: Session,
    ) -> Result<SessionHandle, StoreError>;

    /// Open an existing session by UUID.
    async fn open(
        &self,
        agent: Option<&str>,
        id: SessionId,
    ) -> Result<SessionHandle, StoreError>;

    /// Open an existing session by alias, or create it if it doesn't exist.
    /// This is the CLI/REPL compatibility path.
    async fn open_or_create_by_alias(
        &self,
        agent: Option<&str>,
        alias: SessionAlias,
        initial_factory: impl FnOnce() -> Session + Send,
    ) -> Result<SessionHandle, StoreError>;

    /// Resolve an alias to its UUID without loading the session.
    async fn resolve_alias(
        &self,
        agent: Option<&str>,
        alias: &SessionAlias,
    ) -> Result<Option<SessionId>, StoreError>;

    /// Persist the current in-memory state of a handle back to storage.
    /// Atomically — no torn writes.
    async fn save(&self, handle: &SessionHandle) -> Result<(), StoreError>;

    /// Rename a session's alias. The UUID and session state are unchanged.
    async fn rename(
        &self,
        handle: &SessionHandle,
        new_alias: SessionAlias,
    ) -> Result<(), StoreError>;

    /// Delete a session permanently. Both the state file and any alias
    /// pointing at it are removed.
    async fn delete(
        &self,
        agent: Option<&str>,
        id: SessionId,
    ) -> Result<(), StoreError>;

    /// List all sessions in a scope (global or per-agent). Returns UUIDs
    /// paired with their aliases if any.
    async fn list(
        &self,
        agent: Option<&str>,
    ) -> Result<Vec<SessionMeta>, StoreError>;
}

pub struct SessionMeta {
    pub id: SessionId,
    pub alias: Option<SessionAlias>,
    pub last_modified: SystemTime,
    pub is_autoname: bool,
}

pub enum StoreError {
    NotFound { id: Option<SessionId>, alias: Option<String> },
    AliasInUse(String),
    InvalidAlias(String),
    Io(std::io::Error),
    Serde(serde_yaml::Error),
    Concurrent,  // best-effort optimistic check
    Other(anyhow::Error),
}
```

### `FileSessionStore`

```rust
pub struct FileSessionStore {
    root: PathBuf,                                      // ~/.config/loki/
    agents_root: PathBuf,                               // ~/.config/loki/agents/
    handles: Mutex<HashMap<(Option<String>, SessionId), Weak<Mutex<Session>>>>,
}
```

The `handles` map is the in-process cache that enforces "one `Arc<Mutex<Session>>` per live session per process." If two callers `open()` the same session, they get two `SessionHandle`s pointing at the same underlying mutex, so their locks serialize. When the last handle drops, the weak ref fails on the next lookup and the store re-reads from disk.

---

## The On-Disk Layout

### New layout (Phase 3 target)

```
~/.config/loki/sessions/
  by-id/
    <uuid>/
      state.yaml
  by-name/
    my-project      → text file containing the UUID
    another-chat    → text file containing the UUID
```

Agent sessions mirror this inside each agent's directory:

```
~/.config/loki/agents/sisyphus/sessions/
  by-id/
    <uuid>/
      state.yaml
  by-name/
    my-project   → UUID
```

### Backward compatibility

The migration is lazy and non-destructive. On `FileSessionStore` startup, we do NOT rewrite the directory. On the first `open_or_create_by_alias("my-project")` call, the store checks:

1. **New layout hit:** is there a `by-name/my-project` alias file? Read the UUID, open `by-id/<uuid>/state.yaml`.
2. **Legacy layout hit:** is there a `sessions/my-project.yaml`? Generate a fresh UUID, create `by-id/<uuid>/state.yaml` from the legacy content (atomic copy), write `by-name/my-project` pointing to the new UUID, and leave the legacy file in place. The legacy file becomes stale but untouched.
3. **Neither:** create fresh.

This means users upgrading from pre-Phase-3 builds never lose data, and they can downgrade during the migration window (their old files are still readable by the old code because we haven't deleted them). A `loki migrate sessions` command can later do a clean sweep to remove the legacy files — but that's an operational convenience, not a requirement of Phase 3.

**Deleting a migrated session** (the `.delete` REPL command) also deletes the legacy file if it still exists, so users don't see orphan entries in `list_sessions()`.

**Autoname temp sessions** (today: `sessions/_/20231201T123456-autoname.yaml`) map cleanly to the new layout — they get UUIDs like any other session, and their alias is the generated `20231201T123456-autoname` string. The `_/` prefix from today's path becomes a flag on `SessionMeta::is_autoname: true` set by the store when it recognizes the naming pattern during migration.

### Atomic writes

Today's `Session::save()` is `std::fs::write(path, yaml)` — if the process dies mid-write, you get a truncated YAML file that can't be loaded. The new `FileSessionStore::save()` uses the standard tempfile-and-rename pattern:

```rust
async fn save(&self, handle: &SessionHandle) -> Result<(), StoreError> {
    let session = handle.state.lock().await;
    let yaml = serde_yaml::to_string(&*session)?;
    let target = self.state_path(handle.is_agent.as_deref(), handle.id);
    let tmp = target.with_extension("yaml.tmp");
    tokio::fs::write(&tmp, yaml).await?;
    tokio::fs::rename(&tmp, &target).await?;
    handle.dirty.store(false, Ordering::Release);
    Ok(())
}
```

`rename` is atomic on POSIX filesystems and on Windows NTFS (via `MoveFileEx`). Either the old content or the new content is visible to readers; never a half-written file.

---

## Concurrency Model

Three layers, each with a clear responsibility:

1. **Process-level: per-session `Arc<Mutex<Session>>`.** Two handles to the same session share one mutex. Inside one process, concurrent access to the same session is serialized automatically. This is enough for CLI (single request) and REPL (single user, but multiple async tasks like background compression).

2. **Inter-process: filesystem rename atomicity.** Two separate Loki processes (unlikely today but possible for someone running CLI and REPL simultaneously on the same state) can't corrupt files because writes go through tempfile+rename. The later writer wins cleanly; the earlier writer's changes are lost but the file is always readable.

3. **Optimistic conflict detection (optional, Phase 5+):** If we later decide to add "you edited this session somewhere else, please reload" UX, we can add an `mtime` check on load/save and surface `StoreError::Concurrent` when the on-disk mtime doesn't match the value we read at `open()` time. This is deliberately not built in Phase 3 — it's a UX improvement for later, not a correctness requirement.

For Phase 3, layers 1 and 2 together are sufficient for everything up through "many concurrent API sessions, each addressing different UUIDs." The one gap they don't cover is "multiple API requests on the same session UUID at the same time" — but the per-session mutex in layer 1 handles that by serializing them, which is the desired behavior. The second request waits its turn and sees the first request's updates.

---

## Engine and Callsite Changes

### Before Phase 3

```rust
// In REPL command handler:
Config::use_session_safely(&config, Some("my-project"), abort_signal)?;
// later:
config.write().session.as_mut().unwrap().add_message(...);
// later:
Config::save_session_safely(&config, None)?;
```

### After Phase 3

```rust
// In CoreCommand::UseSession handler inside Engine::dispatch_command:
let alias = SessionAlias::new("my-project")?;
let handle = self.app.sessions.open_or_create_by_alias(
    ctx.agent_name(),
    alias,
    || Session::new_default(ctx.model_id(), ctx.role_name()),
).await?;
ctx.session = Some(handle);

// later, during the chat loop:
{
    let mut guard = handle.lock().await;
    guard.get_mut().add_message(input, output);
}
handle.save().await?;  // fires when the turn completes
```

The `RequestContext.session: Option<Session>` field becomes `RequestContext.session: Option<SessionHandle>`. All 13 session-touching callsites from the explore get rewritten to go through the handle instead of direct access.

### The 13 callsites and their new shapes

| Current location | Current call | New call |
|---|---|---|
| `Config::use_session` | `Session::load` or `Session::new` | `store.open_or_create_by_alias(...)` |
| `Config::use_session_safely` | take/replace pattern on `config.session` | `ctx.session = Some(handle)` |
| `Config::exit_session` | `session.exit()` (maybe saves) | `if ctx.session.dirty() { handle.save().await? }; ctx.session = None` |
| `Config::empty_session` | `session.clear_messages()` | `handle.lock().await.get_mut().clear_messages()` |
| `Config::save_session` | `session.save()` with name logic | `handle.rename(alias)?; handle.save().await?` |
| `Config::compress_session` | mutates session, relies on dirty flag | `handle.lock().await.get_mut().compress(...)?; handle.save().await?` |
| `Config::maybe_autoname_session` | spawns task, mutates session | same, but via handle |
| `Config::delete` (kind="session") | `remove_file` on path | `store.delete(agent, id).await?` |
| `Config::after_chat_completion` | `session.add_message(...)` | via handle |
| `Config::apply_prelude` | may `use_session` | via store |
| `Agent::init` / `use_agent` | may load agent session | via store, with `agent=Some(name)` |
| `.session` REPL command | via `use_session_safely` | via store |
| `.delete session` REPL command | via `Config::delete` | via store |

Most of these are one-liner changes since the store's API mirrors the semantics of today's methods. The subtle ones are:

- **`exit_session`** has "save if dirty and `save_session != Some(false)`" logic plus "prompt for name if temp session" UX. The prompt lives in the REPL layer (it calls `inquire::Text`), not in the store. After the refactor, the REPL reads the dirty flag from the handle, prompts for a name if needed, calls `handle.rename()` if the user provided one, then calls `handle.save()`.

- **`compress_session`** runs asynchronously today — it spawns a task that holds a clone of `GlobalConfig` and writes back via `config.write()`. After the refactor, the task holds an `Arc<SessionHandle>` and does `handle.lock().await.get_mut().compress(...)` followed by `handle.save().await`. The per-session mutex prevents the compression task from clobbering concurrent turn writes.

- **`maybe_autoname_session`** is the same story as compression: spawn task, mutate through handle, save through store.

---

## Migration Strategy

### Step 1: Create the types without wiring

Add new files:

- `src/session/mod.rs` — module root
- `src/session/id.rs` — `SessionId`, `SessionAlias`
- `src/session/store.rs` — `SessionStore` trait, `StoreError`, `SessionMeta`
- `src/session/handle.rs` — `SessionHandle`, `SessionGuard`
- `src/session/file_store.rs` — `FileSessionStore` implementation

Move the existing `Session` struct from `src/config/session.rs` to `src/session/session.rs`. Keep the pub re-export at `src/config::Session` so no external callers break during the migration. The struct itself is unchanged — same fields, same YAML format, same methods. This is purely a module reorganization.

Register `pub mod session;` in `src/main.rs` and add `pub sessions: Arc<dyn SessionStore>` to `AppState`. Initialize it in `AppState::init()` with `FileSessionStore::new(config_dir)`.

**Verification:** `cargo check` clean, `cargo test` passes. Nothing uses the new types yet.

### Step 2: Implement `FileSessionStore` against the new layout

Build the file-based implementation:

- `state_path(agent, id) → ~/.config/loki/[agents/<agent>/]sessions/by-id/<uuid>/state.yaml`
- `alias_path(agent, alias) → ~/.config/loki/[agents/<agent>/]sessions/by-name/<alias>`
- `legacy_path(agent, alias) → ~/.config/loki/[agents/<agent>/]sessions/<alias>.yaml`

Implement `create`, `open`, `open_or_create_by_alias`, `resolve_alias`, `save`, `rename`, `delete`, `list`. The `open_or_create_by_alias` method is the most complex — it has the lazy-migration logic that checks new layout, then legacy layout, then falls through to creation.

**Unit tests for `FileSessionStore`:**
- Create + open roundtrip
- Create with alias + open_or_create_by_alias finds it
- Lazy migration from legacy `.yaml` file
- Delete removes both new and legacy paths
- Rename updates alias index without touching state file
- List returns both new-layout and legacy-layout sessions
- Atomic write: kill the process mid-write (simulated by injected failure) and verify no torn YAML

These tests use `tempfile::TempDir` so they don't touch the real config directory.

**Verification:** Unit tests pass. `cargo check` clean.

### Step 3: Add `SessionHandle` and integrate with `RequestContext`

Change `RequestContext.session` from `Option<Session>` to `Option<SessionHandle>`. This is a mass rename across the codebase — every callsite that does `ctx.session.as_ref()` needs to become `ctx.session.as_ref().map(|h| h.lock().await.get())` or similar.

The cleanest way to minimize the blast radius is to add a thin compatibility layer on `RequestContext`:

```rust
impl RequestContext {
    pub async fn session_read<F, R>(&self, f: F) -> Option<R>
    where F: FnOnce(&Session) -> R {
        let handle = self.session.as_ref()?;
        let guard = handle.lock().await;
        Some(f(guard.get()))
    }

    pub async fn session_write<F, R>(&mut self, f: F) -> Option<R>
    where F: FnOnce(&mut Session) -> R {
        let handle = self.session.as_ref()?;
        let mut guard = handle.lock().await;
        Some(f(guard.get_mut()))
    }
}
```

Most callsites become `ctx.session_read(|s| s.model_id.clone()).await` or `ctx.session_write(|s| s.add_message(...)).await`. A few that need to hold the guard across await points (e.g., compression) use `handle.lock()` directly.

**Verification:** `cargo check` clean. Existing REPL functions still work because the old method names get forwarded through the compatibility helpers.

### Step 4: Rewrite the 13 session callsites to use the store

Go through each callsite in the inventory table and rewrite it:

1. `Config::use_session` → `Engine::dispatch_command` for `CoreCommand::UseSession`
2. `Config::use_session_safely` → same, with extra ctx reset logic
3. `Config::exit_session` → `Engine::dispatch_command` for `CoreCommand::ExitSession`
4. ... and so on

Where possible, move the logic INTO `Engine::dispatch_command` rather than leaving it on `Config`. This is consistent with Phase 2's direction — core logic lives in the engine, not on state containers.

For each rewrite:
- Delete the old method from `Config`
- Add the new handler in `Engine::dispatch_command`
- Update any callers that still reference the old method name
- Run `cargo check` after each file to catch issues incrementally

**Verification:** After each rewrite, `cargo check` + the relevant integration tests from Phase 2. The Phase 2 `CollectingEmitter` tests for session-touching scenarios are especially important here — they're the regression net.

### Step 5: Remove the compatibility helpers from `RequestContext`

Once all 13 callsites are rewritten, the `session_read` / `session_write` helpers are only used by the old session methods we just deleted. Remove them. Any remaining compile errors point at callsites we missed.

**Verification:** `cargo check` clean, all of Phase 2's tests still pass, plus the new `FileSessionStore` unit tests.

### Step 6: Add the integration tests for concurrent access

These are the tests that prove Phase 3 actually solved the concurrency problem:

```rust
#[tokio::test]
async fn concurrent_opens_share_one_mutex() {
    let store = FileSessionStore::new(tempdir);
    let id = SessionId::new();
    // ... create initial session ...

    let h1 = store.open(None, id).await.unwrap();
    let h2 = store.open(None, id).await.unwrap();

    // Both handles should point at the same Arc<Mutex<Session>>
    let lock1 = h1.lock().await;
    // Try to lock h2 — should block
    let try_lock = tokio::time::timeout(
        Duration::from_millis(50),
        h2.lock(),
    ).await;
    assert!(try_lock.is_err(), "h2 should block while h1 holds the lock");
    drop(lock1);
    let _lock2 = h2.lock().await;
}

#[tokio::test]
async fn concurrent_writes_serialize_without_loss() {
    let store = Arc::new(FileSessionStore::new(tempdir));
    let id = create_initial_session(&store).await;

    let tasks: Vec<_> = (0..100).map(|i| {
        let store = store.clone();
        tokio::spawn(async move {
            let handle = store.open(None, id).await.unwrap();
            {
                let mut guard = handle.lock().await;
                guard.get_mut().add_message(
                    Input::from_str(format!("msg-{i}")),
                    format!("reply-{i}"),
                );
            }
            handle.save().await.unwrap();
        })
    }).collect();

    for t in tasks { t.await.unwrap(); }

    let handle = store.open(None, id).await.unwrap();
    let guard = handle.lock().await;
    assert_eq!(guard.get().messages.len(), 200);  // 100 user + 100 assistant
}
```

The second test specifically verifies that the per-session mutex serialization prevents lost updates — the flaw in today's code.

**Verification:** Both tests pass. `cargo test` green overall.

### Step 7: Legacy migration smoke test

Copy a real user's `sessions/my-project.yaml` file into a test fixture directory. Run `FileSessionStore::open_or_create_by_alias("my-project")` and assert:

- A new `by-id/<uuid>/state.yaml` exists with identical content
- A new `by-name/my-project` file exists containing the UUID
- The original `sessions/my-project.yaml` is still there, untouched
- A second `open_or_create_by_alias("my-project")` call reuses the same UUID (idempotent)

**Verification:** Test passes with real fixture data including a session that has compressed messages and agent variables.

### Step 8: Manual smoke test

Run through a full REPL session covering every session-touching command:

1. `loki` → REPL starts, `.session foo` → new session created, check `by-id/` and `by-name/foo` exist
2. Several messages → check `state.yaml` updates atomically
3. `.save session bar` → check alias renamed, UUID unchanged
4. `.empty session` → messages cleared, file still exists
5. `.exit session` → session closed
6. `loki --session bar` from command line → same UUID resumes
7. `.delete` then choose session → both new and legacy files gone
8. Agent with `.agent sisyphus my-work` → agent-scoped session in `agents/sisyphus/sessions/`
9. Auto-continuation in an agent → compression fires, concurrent writes serialize cleanly

Every interaction should behave identically to pre-Phase-3.

---

## Risks and Watch Items

| Risk | Severity | Mitigation |
|---|---|---|
| **Legacy file discovery** | Medium | The migration path must handle every legacy layout: `sessions/<name>.yaml`, `sessions/_/<timestamp>-<autoname>.yaml`, and agent-scoped `agents/<agent>/sessions/<name>.yaml`. Write a fixture test for each variant. |
| **Alias collisions during migration** | Medium | If two processes simultaneously migrate the same legacy session, they could create two different UUIDs. Mitigation: the `open_or_create_by_alias` path should acquire a file lock on the alias file itself during creation, not just rely on the store's in-memory map. |
| **`RequestContext.session` type change blast radius** | Medium | Using the compatibility helpers (`session_read` / `session_write`) in Step 3 contains the blast radius. Only remove them in Step 5 once everything compiles. |
| **Session::save deadlock via re-entry** | Medium | If `Session::compress()` or `add_message()` internally trigger anything that tries to re-lock the session's mutex, we get a deadlock. Audit every `Session` method called inside a `guard.get_mut()` scope to make sure none of them take the lock again. Document the invariant in `SessionHandle` rustdoc. |
| **Tempfile cleanup on crash** | Low | If the process dies after writing `.yaml.tmp` but before the rename, we leave a stray file. On startup, `FileSessionStore::new` should sweep `by-id/*/state.yaml.tmp` files and remove them. |
| **Alias index corruption** | Low | If `by-name/foo` contains garbage (not a valid UUID), treat it as a missing alias and log a warning. Don't crash the process. |
| **Serde compatibility with old files** | Low | The `Session` struct's serde shape doesn't change in Phase 3, so old YAML files deserialize identically. Verify with a fixture test that includes every optional field set. |
| **CLI `--session <uuid>` vs `--session <alias>` ambiguity** | Low | `SessionId::parse` recognizes UUID format; fall back to treating the argument as an alias if parsing fails. Document in `--help`. |
| **Concurrent delete while handle held** | Low | If one task is using a handle while another deletes the session, the first task's save will fail (file missing). This is acceptable behavior — log a warning and return `StoreError::NotFound`. Tests should cover this. |

---

## What Phase 3 Does NOT Do

- **No schema migration.** YAML format stays identical. `Session` struct unchanged.
- **No database.** `FileSessionStore` is the only implementation.
- **No session TTL / eviction.** Sessions live until explicitly deleted.
- **No cross-process locking.** Two Loki processes can still race, but writes are atomic so files never corrupt.
- **No session encryption.** Vault handles secrets; sessions are plain YAML.
- **No session sharing between users.** Each process has its own config directory.
- **No optimistic concurrency (mtime check).** Deferred to Phase 5+ as a UX enhancement.
- **No session versioning / rollback.** Deferred.
- **No changes to `Session::build_messages()`, compression logic, or autoname generation.** The behaviors that read/mutate `Session` stay the same — only how they're reached changes.

The sole goal of Phase 3 is: **route all session persistence through a `SessionStore` trait with UUID-primary identity, lazy migration from the legacy layout, per-session mutex serialization, and atomic writes.**

---

## Entry Criteria (from Phase 2)

- [ ] `Engine::run` is the only path to the LLM pipeline
- [ ] `CoreCommand::UseSession`, `ExitSession`, `EmptySession`, `CompressSession`, `SaveSession`, `EditSession` are all implemented and tested
- [ ] `CollectingEmitter` integration tests cover session-touching scenarios
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean
- [ ] CLI and REPL manual smoke tests match pre-Phase-2 behavior

## Exit Criteria (Phase 3 complete)

- [ ] `src/session/` module exists with `SessionStore` trait, `FileSessionStore`, `SessionId`, `SessionAlias`, `SessionHandle`, `SessionGuard`
- [ ] `AppState.sessions: Arc<dyn SessionStore>` is wired in
- [ ] `RequestContext.session: Option<SessionHandle>` (not `Option<Session>`)
- [ ] All 13 session callsites go through the store; no direct `Session::load` or `Session::save` calls remain outside `FileSessionStore`
- [ ] Legacy layout files are lazily migrated on first access
- [ ] New layout (`by-id/<uuid>/state.yaml` + `by-name/<alias>`) is the canonical on-disk format for all new sessions
- [ ] Atomic writes via tempfile+rename
- [ ] Per-session mutex serialization verified by concurrent-write integration tests
- [ ] Legacy fixture test passes (existing user data still loads)
- [ ] Full REPL smoke test covers every session command
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean
- [ ] Phase 4 (REST API) can address sessions by UUID without touching persistence code
