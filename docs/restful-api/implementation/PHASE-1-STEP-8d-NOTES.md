# Phase 1 Step 8d — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8d: Scope transition rewrites — `use_role`,
  `use_session`, `use_agent`, `exit_agent`"

## Summary

Added scope transition methods to `RequestContext` that build real
`ToolScope` instances via `McpFactory::acquire()`. Added
`mcp_config` and `mcp_log_path` fields to `AppState` so scope
transitions can look up MCP server specs and acquire handles. Added
`Session::new_from_ctx` and `Session::load_from_ctx` constructors
that take `&RequestContext` + `&AppConfig` instead of `&Config`.
Migrated `edit_role` (deferred from Step 8b) since `use_role` is
now available. `use_agent` is deferred to Step 8h because
`Agent::init` takes `&GlobalConfig`.

## What was changed

### Files modified (4 files)

- **`src/config/app_state.rs`** — added 2 fields:
  - `mcp_config: Option<McpServersConfig>` — parsed MCP server
    specs from `mcp.json`, stored at init time for scope
    transitions to look up server specs by name
  - `mcp_log_path: Option<PathBuf>` — log path for MCP server
    stderr output, passed to `McpFactory::acquire`

- **`src/config/request_context.rs`** — added 6 methods in a new
  impl block:
  - `rebuild_tool_scope(&mut self, app, enabled_mcp_servers)` —
    private async helper that resolves MCP server IDs, acquires
    handles via `McpFactory::acquire()`, builds a fresh `Functions`
    instance, appends user interaction and MCP meta functions,
    assembles a `ToolScope`, and assigns it to `self.tool_scope`
  - `use_role(&mut self, app, name, abort_signal)` — retrieves
    the role, resolves its MCP server list, calls
    `rebuild_tool_scope`, then `use_role_obj`
  - `use_session(&mut self, app, session_name, abort_signal)` —
    creates or loads a session via `Session::new_from_ctx` /
    `Session::load_from_ctx`, rebuilds the tool scope, handles
    the "carry last message" prompt, calls
    `init_agent_session_variables`
  - `exit_agent(&mut self, app)` — exits the session, resets the
    tool scope to a fresh default (global functions + user
    interaction), clears agent/supervisor/rag state
  - `edit_role(&mut self, app, abort_signal)` — resolves the
    current role name, calls `upsert_role` (editor), then
    `use_role`
  - `upsert_role(&self, app, name)` — opens the role file in the
    editor (via `app.editor()`)

  Updated imports: `McpRuntime`, `TEMP_SESSION_NAME`, `AbortSignal`,
  `formatdoc`, `Confirm`, `remove_file`.

- **`src/config/session.rs`** — added 2 constructors:
  - `Session::new_from_ctx(&RequestContext, &AppConfig, name)` —
    equivalent to `Session::new(&Config, name)` but reads
    `ctx.extract_role(app)` and `app.save_session`
  - `Session::load_from_ctx(&RequestContext, &AppConfig, name, path)` —
    equivalent to `Session::load(&Config, name, path)` but calls
    `Model::retrieve_model(app, ...)` and
    `ctx.retrieve_role(app, role_name)` instead of `&Config` methods

- **`src/config/bridge.rs`** — added `mcp_config: None,
  mcp_log_path: None` to all 3 `AppState` construction sites in
  tests

### Files NOT changed

- **`src/mcp/mod.rs`** — untouched; Step 8c's extraction is used
  via `McpFactory::acquire()`
- **`src/config/mcp_factory.rs`** — untouched
- **`src/config/mod.rs`** — all `Config::use_role`,
  `Config::use_session`, `Config::use_agent`,
  `Config::exit_agent` stay intact for current callers

## Key decisions

### 1. `rebuild_tool_scope` replaces `McpRegistry::reinit`

The existing `Config::use_role` and `Config::use_session` both
follow the pattern: take `McpRegistry` → `McpRegistry::reinit` →
put registry back. The new `rebuild_tool_scope` replaces this with:
resolve server IDs → `McpFactory::acquire()` each → build
`ToolScope`. This is the core semantic change from the plan.

Key differences:
- `McpRegistry::reinit` does batch start/stop of servers (stops
  servers not in the new set, starts missing ones). The factory
  approach acquires each server independently — unused servers
  are dropped when their `Arc` refcount hits zero.
- The factory's `Weak` sharing means that switching from role A
  (github,slack) to role B (github,jira) shares the github
  handle instead of stopping and restarting it.

### 2. `ToolCallTracker` initialized with default params

`ToolCallTracker::new(4, 10)` — 4 max repeats, 10 chain length.
These match the constants used in the existing codebase (the
tracker is used for tool-call loop detection). A future step can
make these configurable via `AppConfig` if needed.

### 3. `use_agent` deferred to Step 8h

`Config::use_agent` is a static method that takes `&GlobalConfig`
and calls `Agent::init(config, agent_name, abort_signal)`.
`Agent::init` compiles agent tools, loads RAG, resolves the model,
and does ~100 lines of setup, all against `&Config`. Migrating
`Agent::init` is a significant cross-module change that belongs
in Step 8h alongside the other agent lifecycle methods.

The plan listed `use_agent` as a target for 8d, but the
dependency on `Agent::init(&Config)` makes a clean bridge
impossible without duplicating `Agent::init`.

### 4. `abort_signal` is unused in the new methods

The existing `Config::use_role` doesn't pass `abort_signal` to
individual server starts — it's used by `abortable_run_with_spinner`
wrapping the batch `McpRegistry::reinit`. The new methods use
`McpFactory::acquire()` which doesn't take an abort signal (see
Step 8c notes). The `_abort_signal` parameter is kept in the
signature for API compatibility; Step 8f can wire it into the
factory if per-server cancellation is needed.

### 5. Session constructors parallel existing ones

`Session::new_from_ctx` and `Session::load_from_ctx` are verbatim
copies of `Session::new` and `Session::load` with `config: &Config`
replaced by `ctx: &RequestContext` + `app: &AppConfig`. The copies
are under `#[allow(dead_code)]` and will replace the originals
when callers migrate in Steps 8f-8g.

### 6. `exit_agent` rebuilds tool scope inline

`Config::exit_agent` calls `self.load_functions()` to reset the
global function declarations after exiting an agent. The new
`exit_agent` does the equivalent inline: creates a fresh
`ToolScope` with `Functions::init()` + user interaction functions.
It does NOT call `rebuild_tool_scope` because there's no MCP
server set to resolve — we're returning to the global scope.

## Deviations from plan

| Deviation | Rationale |
|---|---|
| `use_agent` deferred to Step 8h | Depends on `Agent::init(&Config)` migration |
| No `abort_signal` propagation to `McpFactory::acquire` | Step 8c decided against it; behavior matches existing code |
| No parent scope restoration test | Testing requires spawning real MCP servers; documented as Phase 5 test target |

## Verification

### Compilation

- `cargo check` — clean, zero warnings, zero errors
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged)

## Handoff to next step

### What Step 8e can rely on

- **`RequestContext::use_role(app, name, abort_signal)`** — full
  scope transition with ToolScope rebuild via McpFactory
- **`RequestContext::use_session(app, session_name, abort_signal)`** —
  full scope transition with Session creation/loading
- **`RequestContext::exit_agent(app)`** — cleans up agent state
  and rebuilds global ToolScope
- **`RequestContext::edit_role(app, abort_signal)`** — editor +
  use_role
- **`RequestContext::upsert_role(app, name)`** — editor only
- **`Session::new_from_ctx` / `Session::load_from_ctx`** — ctx-
  compatible session constructors
- **`AppState.mcp_config` / `AppState.mcp_log_path`** — MCP server
  specs and log path available for scope transitions

### Method count at end of Step 8d

- `AppConfig`: 21 methods (unchanged from 8b)
- `RequestContext`: 53 methods (46 from 8b + 6 from 8d + 1 private
  `rebuild_tool_scope`)
- `Session`: 2 new constructors (`new_from_ctx`, `load_from_ctx`)
- `AppState`: 2 new fields (`mcp_config`, `mcp_log_path`)

### What Step 8e should do

Migrate the Category C deferrals from Step 6:
- `compress_session`, `maybe_compress_session`
- `autoname_session`, `maybe_autoname_session`
- `use_rag`, `edit_rag_docs`, `rebuild_rag`
- `apply_prelude`

### Files to re-read at the start of Step 8e

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8e section
- This notes file
- Step 6 notes — Category C deferral inventory
- `src/config/rag_cache.rs` — RagCache scaffolding from Step 6.5
- `src/config/mod.rs` — `compress_session`, `maybe_compress_session`,
  `autoname_session`, `maybe_autoname_session`, `use_rag`,
  `edit_rag_docs`, `rebuild_rag`, `apply_prelude` method bodies

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 8b notes: `docs/implementation/PHASE-1-STEP-8b-NOTES.md`
- Step 8c notes: `docs/implementation/PHASE-1-STEP-8c-NOTES.md`
- Step 6.5 notes: `docs/implementation/PHASE-1-STEP-6.5-NOTES.md`
- Modified files:
  - `src/config/request_context.rs` (6 new methods)
  - `src/config/app_state.rs` (2 new fields)
  - `src/config/session.rs` (2 new constructors)
  - `src/config/bridge.rs` (test updates for new AppState fields)
