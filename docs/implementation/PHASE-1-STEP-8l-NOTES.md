# Phase 1 Step 8l — Implementation Notes

## Status

Done (partial — `handle_spawn` migrated, other handlers kept on
`&GlobalConfig` signatures).

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8l: Migrate `supervisor.rs` sub-agent spawning"

## Summary

Replaced `Config::use_agent(&child_config, ...)` in `handle_spawn`
with a direct call to `Agent::init(&AppConfig, &AppState, ...)`,
inlining the MCP reinit and agent state setup that `Config::use_agent`
previously handled. The child `AppState` is constructed from the
parent `GlobalConfig`'s data.

All handler function signatures remain `&GlobalConfig` because they're
called from `eval_tool_calls` → `ToolCall::eval(config)` which still
passes `GlobalConfig`. Migrating the signatures requires migrating
the entire tool evaluation chain first.

## What was changed

### Files modified (1 file)

- **`src/function/supervisor.rs`** — `handle_spawn`:
  - Builds `AppConfig` + `AppState` from parent `GlobalConfig`
  - Calls `Agent::init(&app_config, &child_app_state, ...)` directly
  - Inlines MCP reinit (take registry → reinit → append meta functions → put back)
  - Inlines agent state setup (rag, agent, supervisor on child_config)
  - Inlines session setup (`Config::use_session_safely` or `init_agent_shared_variables`)
  - Added imports: `Agent`, `AppState`, `McpRegistry`, `Supervisor`

## Key decisions

### 1. Handler signatures unchanged

All 12 handler functions still take `&GlobalConfig`. This is required
because the call chain is: `eval_tool_calls(&GlobalConfig)` →
`ToolCall::eval(&GlobalConfig)` → `handle_supervisor_tool(&GlobalConfig)`.
Until `eval_tool_calls` is migrated (requires client module migration),
the signatures must stay.

### 2. Child still uses GlobalConfig for run_child_agent

The child's chat loop (`run_child_agent`) still uses a `GlobalConfig`
because `Input` and `eval_tool_calls` need it. The `Agent::init` call
uses `&AppConfig` + `&AppState` (the new signature), but the agent's
state is written back onto the child `GlobalConfig` for the chat loop.

### 3. MCP reinit stays on child GlobalConfig

The child agent's MCP servers are started via `McpRegistry::reinit`
on the child `GlobalConfig`. This is necessary because the child's
`eval_tool_calls` → MCP tool handlers read the MCP registry from
the `GlobalConfig`. Using `McpFactory::acquire` would require the
MCP tool handlers to read from a different source.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean
- `cargo test` — 63 passed, 0 failed

## What remains for supervisor.rs

The handler signatures (`&GlobalConfig`) can only change after:
1. `init_client` migrated to `&AppConfig` (Step 8j completion)
2. Client structs migrated from `GlobalConfig`
3. `eval_tool_calls` migrated to `&AppConfig` + `&mut RequestContext`
4. `ToolCall::eval` migrated similarly
5. All MCP tool handlers migrated to use `McpRuntime` instead of `McpRegistry`

This is the "client chain migration" — a cross-cutting change that
should be a dedicated effort.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8l
- Step 8k notes: `docs/implementation/PHASE-1-STEP-8k-NOTES.md`
- QA checklist: `docs/QA-CHECKLIST.md` — items 11, 12
