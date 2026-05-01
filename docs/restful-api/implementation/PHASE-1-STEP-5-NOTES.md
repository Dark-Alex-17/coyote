# Phase 1 Step 5 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 5: Migrate request-read methods to RequestContext"

## Summary

Added 13 of 15 planned request-read methods to `RequestContext`
as inherent methods, duplicating the bodies that still exist on
`Config`. The other 2 methods (`info`, `session_info`) were
deferred to Step 7 because they mix runtime reads with calls into
`AppConfig`-scoped helpers (`sysinfo`, `render_options`) or depend
on `sysinfo` which itself touches both serialized and runtime
state.

Same duplication pattern as Steps 3 and 4: callers stay on
`Config` during the bridge window; real caller migration happens
organically in Steps 8-9.

## What was changed

### Modified files

- **`src/config/request_context.rs`** — extended the imports
  with 11 new symbols from `super` (parent module constants,
  `StateFlags`, `RoleLike`, `paths`) plus `anyhow`, `env`,
  `PathBuf`, `get_env_name`, and `list_file_names`. Added a new
  `impl RequestContext` block with 13 methods under
  `#[allow(dead_code)]`:

  **Path helpers** (4):
  - `messages_file(&self) -> PathBuf` — agent-aware path to
    the messages log
  - `sessions_dir(&self) -> PathBuf` — agent-aware sessions
    directory
  - `session_file(&self, name) -> PathBuf` — combines
    `sessions_dir` with a session name
  - `rag_file(&self, name) -> PathBuf` — agent-aware RAG file
    path

  **State query** (1):
  - `state(&self) -> StateFlags` — returns bitflags for which
    scopes are currently active

  **Scope info getters** (4):
  - `role_info(&self) -> Result<String>` — exports the current
    role (from session or standalone)
  - `agent_info(&self) -> Result<String>` — exports the current
    agent
  - `agent_banner(&self) -> Result<String>` — returns the
    agent's conversation starter banner
  - `rag_info(&self) -> Result<String>` — exports the current
    RAG

  **Session listings** (2):
  - `list_sessions(&self) -> Vec<String>`
  - `list_autoname_sessions(&self) -> Vec<String>`

  **Misc** (2):
  - `is_compressing_session(&self) -> bool`
  - `role_like_mut(&mut self) -> Option<&mut dyn RoleLike>` —
    returns the currently-active `RoleLike` (session > agent >
    role), the foundation for Step 7's `set_*` methods

  All bodies are copy-pasted verbatim from the originals on
  `Config`, with the following minor adjustments for the new
  module location:
  - Constants like `MESSAGES_FILE_NAME`, `AGENTS_DIR_NAME`,
    `SESSIONS_DIR_NAME` imported from `super::`
  - `paths::` calls unchanged (already in the right module from
    Step 2)
  - `list_file_names` imported from `crate::utils::*` → made
    explicit
  - `get_env_name` imported from `crate::utils::*` → made
    explicit

### Unchanged files

- **`src/config/mod.rs`** — the original `Config` versions of
  all 13 methods are deliberately left intact. They continue to
  work for every existing caller. They get deleted in Step 10
  when `Config` is removed entirely.
- **All external callers** of `config.messages_file()`,
  `config.state()`, etc. — also unchanged.

## Key decisions

### 1. Only 13 of 15 methods migrated

The plan's Step 5 table listed 15 methods. After reading each
body, I classified them:

| Method | Classification | Action |
|---|---|---|
| `state` | Pure runtime-read | **Migrated** |
| `messages_file` | Pure runtime-read | **Migrated** |
| `sessions_dir` | Pure runtime-read | **Migrated** |
| `session_file` | Pure runtime-read | **Migrated** |
| `rag_file` | Pure runtime-read | **Migrated** |
| `role_info` | Pure runtime-read | **Migrated** |
| `agent_info` | Pure runtime-read | **Migrated** |
| `agent_banner` | Pure runtime-read | **Migrated** |
| `rag_info` | Pure runtime-read | **Migrated** |
| `list_sessions` | Pure runtime-read | **Migrated** |
| `list_autoname_sessions` | Pure runtime-read | **Migrated** |
| `is_compressing_session` | Pure runtime-read | **Migrated** |
| `role_like_mut` | Pure runtime-read (returns `&mut dyn RoleLike`) | **Migrated** |
| `info` | Delegates to `sysinfo` (mixed) | **Deferred to Step 7** |
| `session_info` | Calls `render_options` (AppConfig) + runtime | **Deferred to Step 7** |

See "Deviations from plan" for detail.

### 2. Same duplication pattern as Steps 3 and 4

Callers hold `Config`, not `RequestContext`. Same constraints
apply:

- Giving callers a `RequestContext` requires either: (a) a
  sync'd `Arc<RequestContext>` field on `Config` — breaks
  because per-request state mutates constantly, (b) cloning on
  every call — expensive, or (c) duplicating method bodies.
- Option (c) is the same choice Steps 3 and 4 made.
- The duplication is 13 methods (~170 lines total) that
  auto-delete in Step 10.

### 3. `role_like_mut` is particularly important for Step 7

I want to flag this one: `role_like_mut(&mut self)` is the
foundation for every `set_*` method in Step 7 (`set_temperature`,
`set_top_p`, `set_model`, etc.). Those methods all follow the
pattern:

```rust
fn set_something(&mut self, value: Option<T>) {
    if let Some(role_like) = self.role_like_mut() {
        role_like.set_something(value);
    } else {
        self.something = value;
    }
}
```

The `else` branch (fallback to global) is the "mixed" part that
makes them Step 7 targets. The `if` branch is pure runtime write
— it mutates whichever `RoleLike` is on top.

By migrating `role_like_mut` to `RequestContext` in Step 5, Step
7 can build its new `set_*` methods as `(&mut RequestContext,
&mut AppConfig, value)` signatures where the runtime path uses
`ctx.role_like_mut()` directly. The prerequisite is now in place.

### 4. Path helpers stay on `RequestContext`, not `AppConfig`

`messages_file`, `sessions_dir`, `session_file`, and `rag_file`
all read `self.agent` to decide between global and agent-scoped
paths. `self.agent` is a runtime field (per-request). Even
though the returned paths themselves are computed from `paths::`
functions (no per-request state involved), **the decision of
which path to return depends on runtime state**. So these
methods belong on `RequestContext`, not `AppConfig` or `paths`.

This is the correct split — `paths::` is the "pure path
computation" layer, `RequestContext::messages_file` etc. are
the "which path applies to this request" layer on top.

### 5. `state`, `info`-style methods do not take `&self.app`

None of the 13 migrated methods reference `self.app` (the
`Arc<AppState>`) or any field on `AppConfig`. This is the
cleanest possible split — they're pure runtime-reads. If they
needed both runtime state and `AppConfig`, they'd be mixed (like
`info` and `session_info`, which is why those are deferred).

## Deviations from plan

### `info` deferred to Step 7

The plan lists `info` as a Step 5 target. Reading its body:

```rust
pub fn info(&self) -> Result<String> {
    if let Some(agent) = &self.agent {
        // ... agent export with session ...
    } else if let Some(session) = &self.session {
        session.export()
    } else if let Some(role) = &self.role {
        Ok(role.export())
    } else if let Some(rag) = &self.rag {
        rag.export()
    } else {
        self.sysinfo()  // ← falls through to sysinfo
    }
}
```

The fallback `self.sysinfo()` call is the problem. `sysinfo()`
(lines 571-644 in `src/config/mod.rs`) reads BOTH serialized
fields (`wrap`, `rag_reranker_model`, `rag_top_k`,
`save_session`, `compression_threshold`, `dry_run`,
`function_calling_support`, `mcp_server_support`, `stream`,
`save`, `keybindings`, `wrap_code`, `highlight`, `theme`) AND
runtime fields (`self.rag`, `self.extract_role()` which reads
`self.session`, `self.agent`, `self.role`, `self.model`, etc.).

`sysinfo` is a mixed method in the Step 7 sense — it needs both
`AppConfig` (for the serialized half) and `RequestContext` (for
the runtime half). The plan's Step 7 mixed-method list includes
`sysinfo` explicitly.

Since `info` delegates to `sysinfo` in one of its branches,
migrating `info` without `sysinfo` would leave that branch
broken. **Action taken:** left both `Config::info` and
`Config::sysinfo` intact. Step 7 picks them up as a pair.

### `session_info` deferred to Step 7

The plan lists `session_info` as a Step 5 target. Reading its
body:

```rust
pub fn session_info(&self) -> Result<String> {
    if let Some(session) = &self.session {
        let render_options = self.render_options()?;  // ← AppConfig method
        let mut markdown_render = MarkdownRender::init(render_options)?;
        // ... reads self.agent for agent_info tuple ...
        session.render(&mut markdown_render, &agent_info)
    } else {
        bail!("No session")
    }
}
```

It calls `self.render_options()` which is a Step 3 method now
on `AppConfig`. In the bridge world, the caller holds a
`Config` and can call `config.render_options()` (old) or
`config.to_app_config().render_options()` (new but cloning).
In the post-bridge world with `RequestContext`, the call becomes
`ctx.app.config.render_options()`.

Since `session_info` crosses the `AppConfig` / `RequestContext`
boundary, it's mixed by the Step 7 definition. **Action taken:**
left `Config::session_info` intact. Step 7 picks it up with a
signature like
`(&self, app: &AppConfig) -> Result<String>` or
`(ctx: &RequestContext) -> Result<String>` where
`ctx.app.config.render_options()` is called internally.

### Step 5 count: 13 methods, not 15

Documented here so Step 7's scope is explicit. Step 7 picks up
`info`, `session_info`, `sysinfo`, plus the `set_*` methods and
other items from the original Step 7 list.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from
  Steps 1–4)

Step 5 added no new tests because it's duplication. Existing
tests confirm:
- The original `Config` methods still work
- `RequestContext` still compiles, imports are clean
- The bridge's round-trip test still passes

### Manual smoke test

Not applicable — no runtime behavior changed.

## Handoff to next step

### What Step 6 can rely on

Step 6 (migrate request-write methods to `RequestContext`) can
rely on:

- `RequestContext` now has 13 inherent read methods
- The `#[allow(dead_code)]` on the read-methods `impl` block is
  safe to leave; callers migrate in Steps 8+
- `Config` is unchanged for all 13 methods
- `role_like_mut` is available on `RequestContext` — Step 7
  will use it, and Step 6 might also use it internally when
  implementing write methods like `set_save_session_this_time`
- The bridge from Step 1, `paths` module from Step 2,
  `AppConfig` methods from Steps 3 and 4 are all unchanged
- **`Config::info`, `session_info`, and `sysinfo` are still on
  `Config`** and must stay there through Step 6. They're
  Step 7 targets.
- **`Config::update`, `setup_model`, `load_functions`,
  `load_mcp_servers`, and all `set_*` methods** are also still
  on `Config` and stay there through Step 6.

### What Step 6 should watch for

- **Step 6 targets are request-write methods** — methods that
  mutate the runtime state on `Config` (session, role, agent,
  rag). The plan's Step 6 target list includes:
  `use_prompt`, `use_role` / `use_role_obj`, `exit_role`,
  `edit_role`, `use_session`, `exit_session`, `save_session`,
  `empty_session`, `set_save_session_this_time`,
  `compress_session` / `maybe_compress_session`,
  `autoname_session` / `maybe_autoname_session`,
  `use_rag` / `exit_rag` / `edit_rag_docs` / `rebuild_rag`,
  `use_agent` / `exit_agent` / `exit_agent_session`,
  `apply_prelude`, `before_chat_completion`,
  `after_chat_completion`, `discontinuous_last_message`,
  `init_agent_shared_variables`,
  `init_agent_session_variables`.
- **Many will be mixed.** Expect to defer several to Step 7.
  In particular, anything that reads `self.functions`,
  `self.mcp_registry`, or calls `set_*` methods crosses the
  boundary. Read each method carefully before migrating.
- **`maybe_compress_session` and `maybe_autoname_session`** take
  `GlobalConfig` (not `&mut self`) and spawn background tasks
  internally. Their signature in Step 6 will need
  reconsideration — they don't fit cleanly in a
  `RequestContext` method because they're already designed to
  work with a shared lock.
- **`use_session_safely`, `use_role_safely`** also take
  `GlobalConfig`. They do the `take()`/`replace()` dance with
  the shared lock. Again, these don't fit the
  `&mut RequestContext` pattern cleanly; plan to defer them.
- **`compress_session` and `autoname_session` are async.** They
  call into the LLM. Their signature on `RequestContext` will
  still be async.
- **`apply_prelude`** is tricky — it may activate a role/agent/
  session from config strings like `"role:explain"` or
  `"session:temp"`. It calls `use_role`, `use_session`, etc.
  internally. If those get migrated, `apply_prelude` migrates
  too. If any stay on `Config`, `apply_prelude` stays with them.
- **`discontinuous_last_message`** just clears `self.last_message`.
  Pure runtime-write, trivial to migrate.

### What Step 6 should NOT do

- Don't touch the Step 3, 4, 5 methods on `AppConfig` /
  `RequestContext` — they stay until Steps 8+ caller migration.
- Don't migrate any `set_*` method, `info`, `session_info`,
  `sysinfo`, `update`, `setup_model`, `load_functions`,
  `load_mcp_servers`, or the `use_session_safely` /
  `use_role_safely` family unless you verify they're pure
  runtime-writes — most aren't, and they're Step 7 targets.
- Don't migrate callers of any method yet. Callers stay on
  `Config` through the bridge window.

### Files to re-read at the start of Step 6

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 6 section
- This notes file — specifically "What Step 6 should watch for"
- `src/config/request_context.rs` — current shape with Step 5
  reads
- Current `Config` method bodies in `src/config/mod.rs` for
  each Step 6 target

## Follow-up (not blocking Step 6)

### 1. `RequestContext` now has ~200 lines beyond struct definition

Between Step 0's `new()` constructor and Step 5's 13 read
methods, `request_context.rs` has grown to ~230 lines. Still
manageable. Step 6 will add more. Post-Phase 1 cleanup can
reorganize into multiple `impl` blocks grouped by concern
(reads/writes/lifecycle) or into separate files if the file
grows unwieldy.

### 2. Duplication count at end of Step 5

Running tally of methods duplicated between `Config` and the
new types during the bridge window:

- `AppConfig` (Steps 3+4): 11 methods
- `RequestContext` (Step 5): 13 methods
- `paths::` module (Step 2): 33 free functions (not duplicated
  — `Config` forwarders were deleted in Step 2)

**Total bridge-window duplication: 24 methods / ~370 lines.**

All auto-delete in Step 10. Maintenance burden is "any bug fix
in a migrated method during Steps 6-9 must be applied twice."
Document this in whatever PR shepherds Steps 6-9.

### 3. The `impl` block structure in `RequestContext` is growing

Now has 2 `impl RequestContext` blocks:
1. `new()` constructor (Step 0)
2. 13 read methods (Step 5)

Step 6 will likely add a third block for writes. That's fine
during the bridge window; cleanup can consolidate later.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 4 notes: `docs/implementation/PHASE-1-STEP-4-NOTES.md`
  (for the duplication rationale)
- Modified file: `src/config/request_context.rs` (new imports
  + new `impl RequestContext` block with 13 read methods)
- Unchanged but referenced: `src/config/mod.rs` (original
  `Config` methods still exist, private constants
  `MESSAGES_FILE_NAME` / `AGENTS_DIR_NAME` /
  `SESSIONS_DIR_NAME` accessed via `super::`)
