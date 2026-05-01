# Phase 1 Step 15 — Implementation Notes

## Status

Done. Phase 1 complete.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 15: Delete `Config` struct and `GlobalConfig`"

## Summary

Deleted `GlobalConfig` type alias and all dead `Config` methods.
Deleted `Config::from_parts` and bridge tests. Moved 8 flat
runtime fields from `RequestContext` into `ToolScope` and
`AgentRuntime`. `RequestContext` is now a clean composition of
well-scoped state structs.

## What was changed

### Dead code deletion

- `GlobalConfig` type alias — deleted
- `Config::from_parts` — deleted
- All bridge.rs tests — deleted
- Dead `Config` methods — deleted (use_agent, use_session_safely,
  use_role_safely, update, delete, and associated helpers)
- Dead `McpRegistry` methods (search_tools_server, describe,
  invoke) — deleted
- Dead `Functions` methods — deleted
- Unused imports cleaned across all files

### Field migrations

**From `RequestContext` to `ToolScope`:**
- `functions: Functions` → `tool_scope.functions` (was duplicated)
- `tool_call_tracker: Option<ToolCallTracker>` → `tool_scope.tool_tracker`

**From `RequestContext` to `AgentRuntime`:**
- `supervisor: Option<Arc<RwLock<Supervisor>>>` → `agent_runtime.supervisor`
- `parent_supervisor: Option<Arc<RwLock<Supervisor>>>` → `agent_runtime.parent_supervisor`
- `self_agent_id: Option<String>` → `agent_runtime.self_agent_id`
- `current_depth: usize` → `agent_runtime.current_depth`
- `inbox: Option<Arc<Inbox>>` → `agent_runtime.inbox`
- `root_escalation_queue: Option<Arc<EscalationQueue>>` → `agent_runtime.escalation_queue`

### RequestContext accessors added

Accessor methods on `RequestContext` provide the same API:
- `current_depth()` → returns `agent_runtime.current_depth` or 0
- `supervisor()` → returns `agent_runtime.supervisor` or None
- `parent_supervisor()` → returns agent_runtime.parent_supervisor or None
- `self_agent_id()` → returns agent_runtime.self_agent_id or None
- `inbox()` → returns agent_runtime.inbox or None
- `root_escalation_queue()` → returns agent_runtime.escalation_queue or None

### AgentRuntime changes

All fields made `Option` to support agents without spawning
capability (no supervisor), root agents without inboxes, and
lazy escalation queue creation.

### Files modified

- `src/config/request_context.rs` — removed 8 flat fields, added
  accessors, updated all internal methods
- `src/config/tool_scope.rs` — removed `#![allow(dead_code)]`
- `src/config/agent_runtime.rs` — made fields Optional, removed
  `#![allow(dead_code)]`, added `Default` impl
- `src/config/bridge.rs` — deleted `from_parts`, tests; updated
  `to_request_context` to build `AgentRuntime`
- `src/config/mod.rs` — deleted `GlobalConfig`, dead methods,
  dead runtime fields
- `src/function/mod.rs` — `ctx.tool_scope.functions`,
  `ctx.tool_scope.tool_tracker`
- `src/function/supervisor.rs` — agent_runtime construction,
  accessor methods
- `src/function/user_interaction.rs` — accessor methods
- `src/function/todo.rs` — agent_runtime access
- `src/client/common.rs` — `ctx.tool_scope.tool_tracker`
- `src/config/macros.rs` — agent_runtime construction
- `src/repl/mod.rs` — tool_scope/agent_runtime access
- `src/main.rs` — agent_runtime for startup path
- `src/mcp/mod.rs` — deleted dead methods

## RequestContext final structure

```rust
pub struct RequestContext {
    // Shared immutable state
    pub app: Arc<AppState>,

    // Per-request identity
    pub macro_flag: bool,
    pub info_flag: bool,
    pub working_mode: WorkingMode,

    // Current model
    pub model: Model,

    // Active scope state
    pub role: Option<Role>,
    pub session: Option<Session>,
    pub rag: Option<Arc<Rag>>,
    pub agent: Option<Agent>,
    pub agent_variables: Option<AgentVariables>,
    pub last_message: Option<LastMessage>,

    // Tool runtime (functions + MCP + tracker)
    pub tool_scope: ToolScope,

    // Agent runtime (supervisor + inbox + escalation + depth)
    pub agent_runtime: Option<AgentRuntime>,
}
```

## Verification

- `cargo check` — zero warnings, zero errors
- `cargo test` — 59 passed, 0 failed
- `GlobalConfig` references — zero across entire codebase
- Flat runtime fields on RequestContext — zero (all moved)

## Phase 1 complete

The monolithic `Config` god-state struct has been broken apart:

| Struct | Purpose | Lifetime |
|---|---|---|
| `AppConfig` | Serialized config from YAML | Immutable, shared |
| `AppState` | Process-wide shared state (vault, MCP factory, RAG cache) | Immutable, shared via Arc |
| `RequestContext` | Per-request mutable state | Owned per request |
| `ToolScope` | Active tool declarations + MCP runtime + call tracker | Per scope transition |
| `AgentRuntime` | Agent-specific wiring (supervisor, inbox, escalation) | Per agent activation |

The codebase is ready for Phase 2: REST API endpoints that create
`RequestContext` per-request from shared `AppState`.
