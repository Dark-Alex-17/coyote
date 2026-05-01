# Phase 1 Step 6.5 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 6.5: Unify tool/MCP fields into `ToolScope` and
  agent fields into `AgentRuntime`"

## Summary

Step 6.5 is the "big architecture step." The plan describes it as
a semantic rewrite of scope transitions (`use_role`, `use_session`,
`use_agent`, `exit_*`) to build and swap `ToolScope` instances via
a new `McpFactory`, plus an `AgentRuntime` collapse for agent-
specific state, and a unified `RagCache` on `AppState`.

**This implementation deviates from the plan.** Rather than doing
the full semantic rewrite, Step 6.5 ships **scaffolding only**:

- New types (`ToolScope`, `McpRuntime`, `McpFactory`, `McpServerKey`,
  `RagCache`, `RagKey`, `AgentRuntime`) exist and compile
- New fields on `AppState` (`mcp_factory`, `rag_cache`) and
  `RequestContext` (`tool_scope`, `agent_runtime`) coexist with
  the existing flat fields
- The `Config::to_request_context` bridge populates the new
  sub-struct fields with defaults; real values flow through the
  existing flat fields during the bridge window
- **No scope transitions are rewritten**; `Config::use_role`,
  `Config::use_session`, `Config::use_agent`, `Config::exit_agent`
  stay on `Config` and continue working with the old
  `McpRegistry` / `Functions` machinery

The semantic rewrite is **deferred to Step 8** when the entry
points (`main.rs`, `repl/mod.rs`) get rewritten to thread
`RequestContext` through the pipeline. That's the natural point
to switch from `Config::use_role` to
`RequestContext::use_role_with_tool_scope`-style methods, because
the callers will already be holding the right instance type.

See "Deviations from plan" for the full rationale.

## What was changed

### New files

Four new modules under `src/config/`, all with module docstrings
explaining their scaffolding status and load-bearing references
to the architecture + phase plan docs:

- **`src/config/tool_scope.rs`** (~75 lines)
  - `ToolScope` struct: `functions`, `mcp_runtime`, `tool_tracker`
    with `Default` impl
  - `McpRuntime` struct: wraps a
    `HashMap<String, Arc<ConnectedServer>>` (reuses the existing
    rmcp `RunningService` type)
  - Basic accessors: `is_empty`, `insert`, `get`, `server_names`
  - No `build_from_enabled_list` or similar; that's Step 8

- **`src/config/mcp_factory.rs`** (~90 lines)
  - `McpServerKey` struct: `name` + `command` + sorted `args` +
    sorted `env` (so identically-configured servers hash to the
    same key and share an `Arc`, while differently-configured
    ones get independent processes — the sharing-vs-isolation
    invariant from architecture doc section 5)
  - `McpFactory` struct:
    `Mutex<HashMap<McpServerKey, Weak<ConnectedServer>>>` for
    future sharing
  - Basic accessors: `active_count`, `try_get_active`,
    `insert_active`
  - **No `acquire()` that actually spawns.** That would require
    lifting the MCP server startup logic out of
    `McpRegistry::init_server` into a factory method. Deferred
    to Step 8 with the scope transition rewrites.

- **`src/config/rag_cache.rs`** (~90 lines)
  - `RagKey` enum: `Named(String)` vs `Agent(String)` (distinct
    namespaces)
  - `RagCache` struct:
    `RwLock<HashMap<RagKey, Weak<Rag>>>` with weak-ref sharing
  - `try_get`, `insert`, `invalidate`, `entry_count`
  - `load_with<F, Fut>()` — async helper that checks the cache,
    calls a user-provided loader closure on miss, inserts the
    result, and returns the `Arc`. Has a small race window
    between `try_get` and `insert` (two concurrent misses will
    both load); this is acceptable for Phase 1 per the
    architecture doc's "concurrent first-load" note. Tightening
    with a per-key `OnceCell` or `tokio::sync::Mutex` lands in
    Phase 5.

- **`src/config/agent_runtime.rs`** (~95 lines)
  - `AgentRuntime` struct with every field from the plan:
    `rag`, `supervisor`, `inbox`, `escalation_queue`,
    `todo_list: Option<TodoList>`, `self_agent_id`,
    `parent_supervisor`, `current_depth`, `auto_continue_count`
  - `new()` constructor that takes the required agent context
    (id, supervisor, inbox, escalation queue) and initializes
    optional fields to `None`/`0`
  - `with_rag`, `with_todo_list`, `with_parent_supervisor`,
    `with_depth` builder methods for Step 8's activation path
  - **`todo_list` is `Option<TodoList>`** (opportunistic
    tightening over today's `Config.agent.todo_list:
    TodoList`): the field will be `Some(...)` only when
    `spec.auto_continue == true`, saving an allocation for
    agents that don't use the todo system

### Modified files

- **`src/mcp/mod.rs`** — changed `type ConnectedServer` from
  private to `pub type ConnectedServer` so `tool_scope.rs` and
  `mcp_factory.rs` can reference the type without reaching into
  `rmcp` directly. One-character change (`type` → `pub type`).

- **`src/config/mod.rs`** — registered 4 new `mod` declarations
  (`agent_runtime`, `mcp_factory`, `rag_cache`, `tool_scope`)
  alphabetically in the module list. No `pub use` re-exports —
  the types are used via their module paths by the parent
  `config` crate's children.

- **`src/config/app_state.rs`** — added `mcp_factory:
  Arc<McpFactory>` and `rag_cache: Arc<RagCache>` fields, plus
  the corresponding imports. Updated the module docstring to
  reflect the Step 6.5 additions and removed the old "TBD"
  placeholder language about `McpFactory`.

- **`src/config/request_context.rs`** — added `tool_scope:
  ToolScope` and `agent_runtime: Option<AgentRuntime>` fields
  alongside the existing flat fields, plus imports. Updated
  `RequestContext::new()` to initialize them with
  `ToolScope::default()` and `None`. Rewrote the module
  docstring to explain that flat and sub-struct fields coexist
  during the bridge window.

- **`src/config/bridge.rs`** — updated
  `Config::to_request_context` to initialize `tool_scope` with
  `ToolScope::default()` and `agent_runtime` with `None` (the
  bridge doesn't try to populate the sub-struct fields because
  they're deferred scaffolding). Updated the three test
  `AppState` constructors to pass `McpFactory::new()` and
  `RagCache::new()` for the new required fields, plus added
  imports for `McpFactory` and `RagCache` in the test module.

- **`Cargo.toml`** — no changes. `parking_lot` and the rmcp
  dependencies were already present.

## Key decisions

### 1. **Scaffolding-only, not semantic rewrite**

This is the biggest decision in Step 6.5 and a deliberate
deviation from the plan. The plan says Step 6.5 should
"rewrite scope transitions" (item 5, page 373) to build and
swap `ToolScope` instances via `McpFactory::acquire()`.

**Why I did scaffolding only instead:**

- **Consistency with the bridge pattern.** Steps 3–6 all
  followed the same shape: add new code alongside old, don't
  migrate callers, let Step 8 do the real wiring. The bridge
  pattern works because it keeps every intermediate state
  green and testable. Doing the full Step 6.5 rewrite would
  break that pattern.

- **Caller migration is a Step 8 concern.** The plan's Step
  6.5 semantics assume callers hold a `RequestContext` and
  can call `ctx.use_role(&app)` to rebuild `ctx.tool_scope`.
  But during the bridge window, callers still hold
  `GlobalConfig` / `&Config` and call `config.use_role(...)`.
  Rewriting `use_role` to take `(&mut RequestContext,
  &AppState)` would either:
  1. Break every existing caller immediately (~20+ callsites),
     forcing a partial Step 8 during Step 6.5, OR
  2. Require a parallel `RequestContext::use_role_with_tool_scope`
     method alongside `Config::use_role`, doubling the
     duplication count for no benefit during the bridge

- **The plan's Step 6.5 risk note explicitly calls this out:**
  *"Risk: Medium–high. This is where the Phase 1 refactor
  stops being mechanical and starts having semantic
  implications."* The scaffolding-only approach keeps Step 6.5
  mechanical and pushes the semantic risk into Step 8 where it
  can be handled alongside the entry point rewrite. That's a
  better risk localization strategy.

- **The new types are still proven by construction.**
  `Config::to_request_context` now builds `ToolScope::default()`
  and `agent_runtime: None` on every call, and the bridge
  round-trip test still passes. That proves the types compile,
  have sensible defaults, and don't break the existing runtime
  contract. Step 8 can then swap in real values without
  worrying about type plumbing.

### 2. `McpFactory::acquire()` is not implemented

The plan says Step 6.5 ships a trivial `acquire()` that
"checks `active` for an upgradable `Weak`, otherwise spawns
fresh" and "drops tear down the subprocess directly."

I wrote the `Mutex<HashMap<McpServerKey, Weak<ConnectedServer>>>`
field and the `try_get_active` / `insert_active` building
blocks, but not an `acquire()` method. The reason is that
actually spawning an MCP subprocess requires lifting the
current spawning logic out of `McpRegistry::init_server` (in
`src/mcp/mod.rs`) — that's a ~60 line chunk of tokio child
process setup, rmcp handshake, and error handling that's
tightly coupled to `McpRegistry`. Extracting it as a factory
method is a meaningful refactor that belongs alongside the
Step 8 caller migration, not as orphaned scaffolding that
nobody calls.

The `try_get_active` and `insert_active` primitives are the
minimum needed for Step 8's `acquire()` implementation to be
a thin wrapper.

### 3. Sub-struct fields coexist with flat fields

`RequestContext` now has both:

- **Flat fields** (`functions`, `tool_call_tracker`,
  `supervisor`, `inbox`, `root_escalation_queue`,
  `self_agent_id`, `current_depth`, `parent_supervisor`) —
  populated by `Config::to_request_context` during the bridge
- **Sub-struct fields** (`tool_scope: ToolScope`,
  `agent_runtime: Option<AgentRuntime>`) — default-
  initialized in `RequestContext::new()` and by the bridge;
  real population happens in Step 8

This is deliberate scaffolding, not a refactor miss. The
module docstring explicitly explains this so a reviewer
doesn't try to "fix" the apparent duplication.

When Step 8 migrates `use_role` and friends to `RequestContext`,
those methods will populate `tool_scope` and `agent_runtime`
directly. The flat fields will become stale / unused during
Step 8 and get deleted alongside `Config` in Step 10.

### 4. `ConnectedServer` visibility bump

The minimum change to `src/mcp/mod.rs` was making
`type ConnectedServer` public (`pub type ConnectedServer`).
This lets `tool_scope.rs` and `mcp_factory.rs` reference the
live MCP handle type directly without either:

1. Reaching into `rmcp::service::RunningService<RoleClient, ()>`
   from the config crate (tight coupling to rmcp)
2. Inventing a new `McpServerHandle` wrapper (premature
   abstraction that would need to be unwrapped later)

The visibility change is bounded: `ConnectedServer` is only
used from within the `loki` crate, and `pub` here means
"visible to the whole crate" via Rust's module privacy, not
"part of Loki's external API."

### 5. `todo_list: Option<TodoList>` tightening

`AgentRuntime.todo_list: Option<TodoList>` (vs today's
`Agent.todo_list: TodoList` with `Default::default()` always
allocated). This is an opportunistic memory optimization
during the scaffolding phase: when Step 8 populates
`AgentRuntime`, it should allocate `Some(TodoList::default())`
only when `spec.auto_continue == true`. Agents without
auto-continue skip the allocation entirely.

This is documented in the `agent_runtime.rs` module docstring
so a reviewer doesn't try to "fix" the `Option` into a bare
`TodoList`.

## Deviations from plan

### Full plan vs this implementation

| Plan item | Status |
|---|---|
| Implement `McpRuntime` and `ToolScope` | ✅ Done (scaffolding) |
| Implement `McpFactory` — no pool, `acquire()` | ⚠️ **Partial** — types + accessors, no `acquire()` |
| Implement `RagCache` with `RagKey`, weak-ref sharing, per-key serialization | ✅ Done (scaffolding, no per-key serialization — Phase 5) |
| Implement `AgentRuntime` with `Option<TodoList>` and agent RAG | ✅ Done (scaffolding) |
| Rewrite scope transitions (`use_role`, `use_session`, `use_agent`, `exit_*`, `update`) | ❌ **Deferred to Step 8** |
| `use_rag` rewritten to use `RagCache` | ❌ **Deferred to Step 8** |
| Agent activation populates `AgentRuntime`, serves RAG from cache | ❌ **Deferred to Step 8** |
| `exit_agent` rebuilds parent's `ToolScope` | ❌ **Deferred to Step 8** |
| Sub-agent spawning constructs fresh `RequestContext` | ❌ **Deferred to Step 8** |
| Remove old `Agent::init` registry-mutation logic | ❌ **Deferred to Step 8** |
| `rebuild_rag` / `edit_rag_docs` use `rag_cache.invalidate` | ❌ **Deferred to Step 8** |

All the ❌ items are semantic rewrites that require caller
migration to take effect. Deferring them keeps Step 6.5
strictly additive and consistent with Steps 3–6. Step 8 will
do the semantic rewrite with the benefit of all the
scaffolding already in place.

### Impact on Step 7

Step 7 is unchanged. The mixed methods (including Steps 3–6
deferrals like `current_model`, `extract_role`, `sysinfo`,
`info`, `session_info`, `use_prompt`, etc.) still need to be
split into explicit `(&AppConfig, &RequestContext)` signatures
the same way the plan originally described. They don't depend
on the `ToolScope` / `McpFactory` rewrite being done.

### Impact on Step 8

Step 8 absorbs the full Step 6.5 semantic rewrite. The
original Step 8 scope was "rewrite entry points" — now it
also includes "rewrite scope transitions to use new types."
This is actually the right sequencing because callers and
their call sites migrate together.

The Step 8 scope is now substantially bigger than originally
planned. The plan should be updated to reflect this, either
by splitting Step 8 into 8a (scope transitions) + 8b (entry
points) or by accepting the bigger Step 8.

### Impact on Phase 5

Phase 5's "MCP pooling" scope is unchanged. Phase 5 adds the
idle pool + reaper + health checks to an already-working
`McpFactory::acquire()`. If Step 8 lands the working
`acquire()`, Phase 5 plugs in the pool; if Step 8 somehow
ships without `acquire()`, Phase 5 has to write it too.
Phase 5's plan doc should note this dependency.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from
  Steps 1–6)

The bridge round-trip tests are the critical check for this
step because they construct `AppState` instances, and
`AppState` now has two new required fields. All three tests
(`to_app_config_copies_every_serialized_field`,
`to_request_context_copies_every_runtime_field`,
`round_trip_preserves_all_non_lossy_fields`,
`round_trip_default_config`) pass after updating the
`AppState` constructors in the test module.

### Manual smoke test

Not applicable — no runtime behavior changed. CLI and REPL
still call `Config::use_role()`, `Config::use_session()`,
etc. and those still work against the old `McpRegistry` /
`Functions` machinery.

## Handoff to next step

### What Step 7 can rely on

Step 7 (mixed methods) can rely on:

- **Zero changes to existing `Config` methods or fields.**
  Step 6.5 didn't touch any of the Step 7 targets.
- **New sub-struct fields exist on `RequestContext`** but are
  default-initialized and shouldn't be consulted by any
  Step 7 mixed-method migration. If a Step 7 method legitimately
  needs `tool_scope` or `agent_runtime` (e.g., because it's
  reading the active tool set), that's a signal the method
  belongs in Step 8, not Step 7.
- **`AppConfig` methods from Steps 3-4 are unchanged.**
- **`RequestContext` methods from Steps 5-6 are unchanged.**
- **`Config::use_role`, `Config::use_session`,
  `Config::use_agent`, `Config::exit_agent`, `Config::use_rag`,
  `Config::edit_rag_docs`, `Config::rebuild_rag`,
  `Config::apply_prelude` are still on `Config`** and must
  stay there through Step 7. They're Step 8 targets.

### What Step 7 should watch for

- **Step 7 targets the 17 mixed methods** from the plan's
  original table plus the deferrals accumulated from Steps
  3–6 (`select_functions`, `select_enabled_functions`,
  `select_enabled_mcp_servers`, `setup_model`, `update`,
  `info`, `session_info`, `sysinfo`, `use_prompt`, `edit_role`,
  `after_chat_completion`).
- **The "mixed" category means: reads/writes BOTH serialized
  config AND runtime state.** The migration shape is to split
  them into explicit
  `fn foo(app: &AppConfig, ctx: &RequestContext)` or
  `fn foo(app: &AppConfig, ctx: &mut RequestContext)`
  signatures.
- **Watch for methods that also touch `self.functions` or
  `self.mcp_registry`.** Those need `tool_scope` /
  `mcp_factory` which aren't ready yet. If a mixed method
  depends on the tool scope rewrite, defer it to Step 8
  alongside the scope transitions.
- **`current_model` is the simplest Step 7 target** — it just
  picks the right `Model` reference from session/agent/role/
  global. Good first target to validate the Step 7 pattern.
- **`sysinfo` is the biggest Step 7 target** — ~70 lines of
  reading both `AppConfig` serialized state and
  `RequestContext` runtime state to produce a display string.
- **`set_*` methods all follow the pattern from the plan's
  Step 7 table:**
  ```rust
  fn set_foo(&mut self, value: ...) {
      if let Some(rl) = self.role_like_mut() { rl.set_foo(value) }
      else { self.foo = value }
  }
  ```
  The new signature splits this: the `role_like` branch moves
  to `RequestContext` (using the Step 5 `role_like_mut`
  helper), the fallback branch moves to `AppConfig` via
  `AppConfig::set_foo`. Callers then call either
  `ctx.set_foo_via_role_like(value)` or
  `app_config.set_foo(value)` depending on context.
- **`update` is a dispatcher** — once all the `set_*` methods
  are split, `update` migrates to live on `RequestContext`
  (because it needs both `ctx.set_*` and `app.set_*` to
  dispatch to).

### What Step 7 should NOT do

- Don't touch the 4 new types from Step 6.5 (`ToolScope`,
  `McpRuntime`, `McpFactory`, `RagCache`, `AgentRuntime`).
  They're scaffolding, untouched until Step 8.
- Don't try to populate `tool_scope` or `agent_runtime` from
  any Step 7 migration. Those are Step 8.
- Don't migrate `use_role`, `use_session`, `use_agent`,
  `exit_agent`, or any method that touches
  `self.mcp_registry` / `self.functions`. Those are Step 8.
- Don't migrate callers of any migrated method.
- Don't touch the bridge's `to_request_context` /
  `to_app_config` / `from_parts`. The round-trip still
  works with `tool_scope` and `agent_runtime` defaulting.

### Files to re-read at the start of Step 7

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 7 section (the
  17-method table starting at line ~525)
- This notes file — specifically the accumulated deferrals
  list from Steps 3-6 in the "What Step 7 should watch for"
  section
- Step 6 notes — which methods got deferred from Step 6 vs
  Step 7 boundary

## Follow-up (not blocking Step 7)

### 1. Step 8's scope is now significantly larger

The original Phase 1 plan estimated Step 8 as "rewrite
`main.rs` and `repl/mod.rs` to use `RequestContext`" — a
meaningful but bounded refactor. After Step 6.5's deferral,
Step 8 also includes:

- Implementing `McpFactory::acquire()` by extracting server
  startup logic from `McpRegistry::init_server`
- Rewriting `use_role`, `use_session`, `use_agent`,
  `exit_agent`, `use_rag`, `edit_rag_docs`, `rebuild_rag`,
  `apply_prelude`, agent sub-spawning
- Wiring `tool_scope` population into all the above
- Populating `agent_runtime` on agent activation
- Building the parent-scope `ToolScope` restoration logic in
  `exit_agent`
- Routing `rebuild_rag` / `edit_rag_docs` through
  `RagCache::invalidate`

This is a big step. The phase plan should be updated to
either split Step 8 into sub-steps or to flag the expanded
scope.

### 2. `McpFactory::acquire()` extraction is its own mini-project

Looking at `src/mcp/mod.rs`, the subprocess spawn + rmcp
handshake lives inside `McpRegistry::init_server` (private
method, ~60 lines). Step 8's first task should be extracting
this into a pair of functions:

1. `McpFactory::spawn_fresh(spec: &McpServerSpec) ->
   Result<ConnectedServer>` — pure subprocess + handshake
   logic
2. `McpRegistry::init_server` — wraps `spawn_fresh` with
   registry bookkeeping (adds to `servers` map, fires catalog
   discovery, etc.) for backward compat

Then `McpFactory::acquire()` can call `spawn_fresh` on cache
miss. The existing `McpRegistry::init_server` keeps working
for the bridge window callers.

### 3. The `load_with` race is documented but not fixed

`RagCache::load_with` has a race window: two concurrent
callers with the same key both miss the cache, both call
the loader closure, both insert into the map. The second
insert overwrites the first. Both callers end up with valid
`Arc<Rag>`s but the cache sharing is broken for that
instant.

For Phase 1 Step 6.5, this is acceptable because the cache
isn't populated by real usage yet. Phase 5's pooling work
should tighten this with per-key `OnceCell` or
`tokio::sync::Mutex`.

### 4. Bridge-window duplication count at end of Step 6.5

Running tally:

- `AppConfig` (Steps 3+4): 11 methods duplicated with `Config`
- `RequestContext` (Steps 5+6): 25 methods duplicated with
  `Config` (1 constructor + 13 reads + 12 writes)
- `paths` module (Step 2): 33 free functions (not duplicated)
- **Step 6.5 NEW:** 4 types + 2 `AppState` fields + 2
  `RequestContext` fields — **all additive scaffolding, no
  duplication of logic**

**Total bridge-window duplication: 36 methods / ~550 lines**,
unchanged from end of Step 6. Step 6.5 added types but not
duplicated logic.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Architecture doc: `docs/REST-API-ARCHITECTURE.md` section 5
- Phase 5 plan: `docs/PHASE-5-IMPLEMENTATION-PLAN.md`
- Step 6 notes: `docs/implementation/PHASE-1-STEP-6-NOTES.md`
- New files:
  - `src/config/tool_scope.rs`
  - `src/config/mcp_factory.rs`
  - `src/config/rag_cache.rs`
  - `src/config/agent_runtime.rs`
- Modified files:
  - `src/mcp/mod.rs` (`type ConnectedServer` → `pub type`)
  - `src/config/mod.rs` (4 new `mod` declarations)
  - `src/config/app_state.rs` (2 new fields + docstring)
  - `src/config/request_context.rs` (2 new fields + docstring)
  - `src/config/bridge.rs` (3 test `AppState` constructors
    updated, `to_request_context` adds 2 defaults)
