# Phase 1 Step 6 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 6: Migrate request-write methods to RequestContext"

## Summary

Added 12 of 27 planned request-write methods to `RequestContext`
as inherent methods, duplicating the bodies that still exist on
`Config`. The other 15 methods were deferred: some to Step 6.5
(because they touch `self.functions` and `self.mcp_registry` —
runtime fields being restructured by the `ToolScope` / `McpFactory`
rework), some to Step 7 (because they cross the `AppConfig` /
`RequestContext` boundary or call into `set_*` mixed methods),
and some because their `GlobalConfig`-based static signatures
don't fit the `&mut RequestContext` pattern at all.

This step has the highest deferral ratio of the bridge phases
so far (12/27 ≈ 44% migrated). That's by design — Step 6 is
where the plan hits the bulk of the interesting refactoring
territory, and it's where the `ToolScope` / `AgentRuntime`
unification in Step 6.5 makes a big difference in what's
migrateable.

## What was changed

### Modified files

- **`src/config/request_context.rs`** — added 1 new import
  (`Input` from `super::`) and a new `impl RequestContext` block
  with 12 methods under `#[allow(dead_code)]`:

  **Role lifecycle (2):**
  - `use_role_obj(&mut self, role) -> Result<()>` — sets the
    role on the current session, or on `self.role` if no session
    is active; errors if an agent is active
  - `exit_role(&mut self) -> Result<()>` — clears the role from
    session or from `self.role`

  **Session lifecycle (5):**
  - `exit_session(&mut self) -> Result<()>` — saves session on
    exit and clears `self.session`
  - `save_session(&mut self, name) -> Result<()>` — persists
    the current session, optionally renaming
  - `empty_session(&mut self) -> Result<()>` — clears messages
    in the active session
  - `set_save_session_this_time(&mut self) -> Result<()>` — sets
    the session's one-shot save flag
  - `exit_agent_session(&mut self) -> Result<()>` — exits the
    agent's session without exiting the agent

  **RAG lifecycle (1):**
  - `exit_rag(&mut self) -> Result<()>` — drops `self.rag`

  **Chat lifecycle (2):**
  - `before_chat_completion(&mut self, input) -> Result<()>` —
    stores the input as `last_message` with empty output
  - `discontinuous_last_message(&mut self)` — clears the
    continuous flag on the last message

  **Agent variable init (2):**
  - `init_agent_shared_variables(&mut self) -> Result<()>` —
    prompts for agent variables on first activation
  - `init_agent_session_variables(&mut self, new_session) -> Result<()>` —
    syncs agent variables into/from session on new or resumed
    session

  All bodies are copy-pasted verbatim from `Config` with no
  modifications — every one of these methods only touches
  fields that already exist on `RequestContext` with the same
  names and types.

### Unchanged files

- **`src/config/mod.rs`** — all 27 original `Config` methods
  (including the 15 deferred ones) are deliberately left intact.
  They continue to work for every existing caller.

## Key decisions

### 1. Only 12 of 27 methods migrated

The plan's Step 6 table listed ~20 methods, but when I scanned
for `fn (use_prompt|use_role|use_role_obj|...)` I found 27
(several methods have paired variants: `compress_session` +
`maybe_compress_session`, `autoname_session` +
`maybe_autoname_session`, `use_role_safely` vs `use_role`). Of
those 27, **12 are pure runtime-writes that migrated cleanly**
and **15 are deferred** to later steps. Full breakdown below.

### 2. Same duplication pattern as Steps 3-5

Callers hold `Config`, not `RequestContext`. Duplication is
strictly additive during the bridge window and auto-deletes in
Step 10.

### 3. Identified three distinct deferral categories

The 15 deferred methods fall into three categories, each with
a different resolution step:

**Category A: Touch `self.functions` or `self.mcp_registry`**
(resolved in Step 6.5 when `ToolScope` / `McpFactory` replace
those fields):
- `use_role` (async, reinits MCP registry for role's servers)
- `use_session` (async, reinits MCP registry for session's
  servers)

**Category B: Call into Step 7 mixed methods** (resolved in
Step 7):
- `use_prompt` (calls `self.current_model()`)
- `edit_role` (calls `self.editor()` + `self.use_role()`)
- `after_chat_completion` (calls private `save_message` which
  touches `self.save`, `self.session`, `self.agent`, etc.)

**Category C: Static async methods taking `&GlobalConfig` that
don't fit the `&mut RequestContext` pattern at all** (resolved
in Step 8 or a dedicated lifecycle-refactor step):
- `maybe_compress_session` — takes owned `GlobalConfig`, spawns
  tokio task
- `compress_session` — async, takes `&GlobalConfig`
- `maybe_autoname_session` — takes owned `GlobalConfig`, spawns
  tokio task
- `autoname_session` — async, takes `&GlobalConfig`
- `use_rag` — async, takes `&GlobalConfig`, calls `Rag::init` /
  `Rag::load` which expect `&GlobalConfig`
- `edit_rag_docs` — async, takes `&GlobalConfig`, calls into
  `Rag::refresh_document_paths` which expects `&GlobalConfig`
- `rebuild_rag` — same as `edit_rag_docs`
- `use_agent` — async, takes `&GlobalConfig`, mutates multiple
  fields under the same write lock, calls
  `Config::use_session_safely`
- `apply_prelude` — async, calls `self.use_role()` /
  `self.use_session()` which are Category A
- `exit_agent` — calls `self.load_functions()` which writes
  `self.functions` (runtime, restructured in Step 6.5)

### 4. `exit_agent_session` migrated despite calling other methods

`exit_agent_session` calls `self.exit_session()` and
`self.init_agent_shared_variables()`. Since both of those are
also being migrated in Step 6, `exit_agent_session` can
migrate cleanly and call the new `RequestContext::exit_session`
and `RequestContext::init_agent_shared_variables` on its own
struct.

### 5. `exit_session` works because Step 5 migrated `sessions_dir`

`exit_session` calls `self.sessions_dir()` which is now a
`RequestContext` method (Step 5). Similarly, `save_session`
calls `self.session_file()` (Step 5) and reads
`self.working_mode` (a `RequestContext` field). This
demonstrates how Steps 5 and 6 layer correctly — Step 5's
reads enable Step 6's writes.

### 6. Agent variable init is pure runtime

`init_agent_shared_variables` and `init_agent_session_variables`
look complex (they call `Agent::init_agent_variables` which
can prompt interactively) but they only touch `self.agent`,
`self.agent_variables`, `self.info_flag`, and `self.session` —
all runtime fields that exist on `RequestContext`.
`Agent::init_agent_variables` itself is a static associated
function on `Agent` that takes `defined_variables`,
`existing_variables`, and `info_flag` as parameters — no
`&Config` dependency. Clean migration.

## Deviations from plan

### 15 methods deferred

Summary table of every method in the Step 6 target list:

| Method | Status | Reason |
|---|---|---|
| `use_prompt` | **Step 7** | Calls `current_model()` (mixed) |
| `use_role` | **Step 6.5** | Touches `functions`, `mcp_registry` |
| `use_role_obj` | ✅ Migrated | Pure runtime-write |
| `exit_role` | ✅ Migrated | Pure runtime-write |
| `edit_role` | **Step 7** | Calls `editor()` + `use_role()` |
| `use_session` | **Step 6.5** | Touches `functions`, `mcp_registry` |
| `exit_session` | ✅ Migrated | Pure runtime-write (uses Step 5 `sessions_dir`) |
| `save_session` | ✅ Migrated | Pure runtime-write (uses Step 5 `session_file`) |
| `empty_session` | ✅ Migrated | Pure runtime-write |
| `set_save_session_this_time` | ✅ Migrated | Pure runtime-write |
| `maybe_compress_session` | **Step 7/8** | `GlobalConfig` + spawns task + `light_theme()` |
| `compress_session` | **Step 7/8** | `&GlobalConfig`, complex LLM workflow |
| `maybe_autoname_session` | **Step 7/8** | `GlobalConfig` + spawns task + `light_theme()` |
| `autoname_session` | **Step 7/8** | `&GlobalConfig`, calls `retrieve_role` + LLM |
| `use_rag` | **Step 7/8** | `&GlobalConfig`, calls `Rag::init`/`Rag::load` |
| `edit_rag_docs` | **Step 7/8** | `&GlobalConfig`, calls `editor()` + Rag refresh |
| `rebuild_rag` | **Step 7/8** | `&GlobalConfig`, Rag refresh |
| `exit_rag` | ✅ Migrated | Trivial (drops `self.rag`) |
| `use_agent` | **Step 7/8** | `&GlobalConfig`, complex multi-field mutation |
| `exit_agent` | **Step 6.5** | Calls `load_functions()` which writes `functions` |
| `exit_agent_session` | ✅ Migrated | Composes migrated methods |
| `apply_prelude` | **Step 7/8** | Calls `use_role` / `use_session` (deferred) |
| `before_chat_completion` | ✅ Migrated | Pure runtime-write |
| `after_chat_completion` | **Step 7** | Calls `save_message` (mixed) |
| `discontinuous_last_message` | ✅ Migrated | Pure runtime-write |
| `init_agent_shared_variables` | ✅ Migrated | Pure runtime-write |
| `init_agent_session_variables` | ✅ Migrated | Pure runtime-write |

**Step 6 total: 12 migrated, 15 deferred.**

### Step 6's deferral load redistributes to later steps

Running tally of deferrals after Step 6:

- **Step 6.5 targets:** `use_role`, `use_session`, `exit_agent`
  (3 methods). These must be migrated alongside the
  `ToolScope` / `McpFactory` rework because they reinit or
  inspect the MCP registry.
- **Step 7 targets:** `use_prompt`, `edit_role`,
  `after_chat_completion`, `select_functions`,
  `select_enabled_functions`, `select_enabled_mcp_servers`
  (from Step 3), `setup_model`, `update` (from Step 4),
  `info`, `session_info`, `sysinfo` (from Step 5),
  **plus** the original Step 7 mixed-method list:
  `current_model`, `extract_role`, `set_temperature`,
  `set_top_p`, `set_enabled_tools`, `set_enabled_mcp_servers`,
  `set_save_session`, `set_compression_threshold`,
  `set_rag_reranker_model`, `set_rag_top_k`,
  `set_max_output_tokens`, `set_model`, `retrieve_role`,
  `use_role_safely`, `use_session_safely`, `save_message`,
  `render_prompt_left`, `render_prompt_right`,
  `generate_prompt_context`, `repl_complete`. This is a big
  step.
- **Step 7/8 targets (lifecycle refactor):** Session
  compression and autonaming tasks, RAG lifecycle methods,
  `use_agent`, `apply_prelude`. These may want their own
  dedicated step if the Step 7 list gets too long.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from
  Steps 1–5)

Step 6 added no new tests — duplication pattern. Existing
tests confirm nothing regressed.

### Manual smoke test

Not applicable — no runtime behavior changed. CLI and REPL
still call `Config::use_role_obj()`, `exit_session()`, etc.
as before.

## Handoff to next step

### What Step 6.5 can rely on

Step 6.5 (unify `ToolScope` / `AgentRuntime` / `McpFactory` /
`RagCache`) can rely on:

- `RequestContext` now has **25 inherent methods** across all
  impl blocks (1 constructor + 13 reads from Step 5 + 12
  writes from Step 6)
- `role_like_mut` is available (Step 5) — foundation for
  Step 7's `set_*` methods
- `exit_session`, `save_session`, `empty_session`,
  `exit_agent_session`, `init_agent_shared_variables`,
  `init_agent_session_variables` are all on `RequestContext` —
  the `use_role`, `use_session`, and `exit_agent` migrations
  in Step 6.5 can call these directly on the new context type
- `before_chat_completion`, `discontinuous_last_message`, etc.
  are also on `RequestContext` — available for the new
  `RequestContext` versions of deferred methods
- `Config::use_role`, `Config::use_session`, `Config::exit_agent`
  are **still on `Config`** and must be handled by Step 6.5's
  `ToolScope` refactoring because they touch `self.functions`
  and `self.mcp_registry`
- The bridge from Step 1, `paths` module from Step 2, Steps
  3-5 new methods, and all previous deferrals are unchanged

### What Step 6.5 should watch for

- **Step 6.5 is the big architecture step.** It replaces:
  - `Config.functions: Functions` with
    `RequestContext.tool_scope: ToolScope` (containing
    `functions`, `mcp_runtime`, `tool_tracker`)
  - `Config.mcp_registry: Option<McpRegistry>` with
    `AppState.mcp_factory: Arc<McpFactory>` (pool) +
    `ToolScope.mcp_runtime: McpRuntime` (per-scope handles)
  - Agent-scoped supervisor/inbox/todo into
    `RequestContext.agent_runtime: Option<AgentRuntime>`
  - Agent RAG into a shared `AppState.rag_cache: Arc<RagCache>`
- **Once `ToolScope` exists**, Step 6.5 can migrate `use_role`
  and `use_session` by replacing the `self.functions.clear_*` /
  `McpRegistry::reinit` dance with
  `self.tool_scope = app.mcp_factory.build_tool_scope(...)`.
- **`exit_agent` calls `self.load_functions()`** which reloads
  the global tools. In the new design, exiting an agent should
  rebuild the `tool_scope` for the now-topmost `RoleLike`. The
  plan's Step 6.5 describes this exact transition.
- **Phase 5 adds the idle pool to `McpFactory`.** Step 6.5
  ships the no-pool version: `acquire()` always spawns fresh,
  `Drop` always tears down. Correct but not optimized.
- **`RagCache` serves both standalone and agent RAGs.** Step
  6.5 needs to route `use_rag` (deferred) and agent activation
  through the cache. Since `use_rag` is a Category C deferral
  (takes `&GlobalConfig`), Step 6.5 may not touch it — it may
  need to wait for Step 8.

### What Step 6.5 should NOT do

- Don't touch the 25 methods already on `RequestContext` — they
  stay until Steps 8+ caller migration.
- Don't touch the `AppConfig` methods from Steps 3-4.
- Don't migrate the Step 7 targets unless they become
  unblocked by the `ToolScope` / `AgentRuntime` refactor.
- Don't try to build the `McpFactory` idle pool — that's
  Phase 5.

### Files to re-read at the start of Step 6.5

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 6.5 section
  (the biggest single section, ~90 lines)
- `docs/REST-API-ARCHITECTURE.md` — section 5 (Tool Scope
  Isolation) has the full design for `ToolScope`, `McpRuntime`,
  `McpFactory`, `RagCache`, `AgentRuntime`
- This notes file — specifically "Category A" deferrals
  (`use_role`, `use_session`, `exit_agent`)
- `src/config/mod.rs` — current `Config::use_role`,
  `Config::use_session`, `Config::exit_agent` bodies to see
  the MCP/functions handling that needs replacing

## Follow-up (not blocking Step 6.5)

### 1. `save_message` is private and heavy

`after_chat_completion` was deferred because it calls the
private `save_message` method, which is ~50 lines of logic
touching `self.save` (serialized), `self.session` (runtime),
`self.agent` (runtime), and the messages file (via
`self.messages_file()` which is on `RequestContext`). Step 7
should migrate `save_message` first, then
`after_chat_completion` can follow.

### 2. `Config::use_session_safely` and `use_role_safely` are a pattern to replace

Both methods do `take(&mut *guard)` on the `GlobalConfig` then
call the instance method on the taken `Config`, then put it
back. This pattern exists because `use_role` and `use_session`
are `&mut self` methods that need to await across the call,
and the `RwLock` can't be held across `.await`.

When `use_role` and `use_session` move to `RequestContext` in
Step 6.5, the `_safely` wrappers can be eliminated entirely —
the caller just takes `&mut RequestContext` directly. Flag
this as a cleanup opportunity for Step 8.

### 3. `RequestContext` is now ~400 lines

Counting imports, struct definition, and 3 `impl` blocks:

```
use statements:         ~20 lines
struct definition:      ~30 lines
impl 1 (new):           ~25 lines
impl 2 (reads, Step 5): ~155 lines
impl 3 (writes, Step 6): ~160 lines
Total:                  ~390 lines
```

Still manageable. Step 6.5 will add `tool_scope` and
`agent_runtime` fields plus their methods, pushing toward
~500 lines. Post-Phase 1 cleanup should probably split into
separate files (`reads.rs`, `writes.rs`, `tool_scope.rs`,
`agent_runtime.rs`) but that's optional.

### 4. Bridge-window duplication count at end of Step 6

Running tally:

- `AppConfig` (Steps 3+4): 11 methods
- `RequestContext` (Steps 5+6): 25 methods
- `paths` module (Step 2): 33 free functions (not duplicated)

**Total bridge-window duplication: 36 methods / ~550 lines.**

All auto-delete in Step 10.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Architecture doc: `docs/REST-API-ARCHITECTURE.md`
- Step 5 notes: `docs/implementation/PHASE-1-STEP-5-NOTES.md`
- Modified file: `src/config/request_context.rs` (new
  `impl RequestContext` block with 12 write methods, plus
  `Input` import)
- Unchanged but referenced: `src/config/mod.rs` (original
  `Config` methods still exist for all 27 targets)
