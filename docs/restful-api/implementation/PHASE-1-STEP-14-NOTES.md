# Phase 1 Step 14 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 14: Migrate `Input` constructors and REPL"

## Summary

Eliminated `GlobalConfig` from every file except `config/mod.rs`
(where the type is defined). `Input` constructors take
`&RequestContext`. REPL holds `Arc<RwLock<RequestContext>>` instead
of `GlobalConfig`. Reedline components read from shared
`RequestContext`. Sync helpers deleted. `to_global_config()` deleted.
`macro_execute` takes `&mut RequestContext`. Implemented
`RequestContext::use_agent`. Added MCP loading spinner, MCP server
tab completions, and filtered internal tools from completions.

## What was changed

### Files modified

- **`src/config/input.rs`** — constructors take `&RequestContext`
  instead of `&GlobalConfig`. `capture_input_config` and
  `resolve_role` read from `RequestContext`/`AppConfig`.

- **`src/config/request_context.rs`** — added `use_agent()` method.
  Deleted `to_global_config()` and `sync_mcp_from_registry()`.
  Added MCP loading spinner in `rebuild_tool_scope`. Added
  configured MCP servers to `.set enabled_mcp_servers` completions.
  Filtered `user__*`, `mcp_*`, `todo__*`, `agent__*` from
  `.set enabled_tools` completions.

- **`src/repl/mod.rs`** — `Repl` struct holds
  `Arc<RwLock<RequestContext>>`, no `GlobalConfig` field. `ask` and
  `run_repl_command` take `&mut RequestContext` only. Deleted
  `sync_ctx_to_config`, `sync_config_to_ctx`,
  `sync_app_config_to_ctx`, `reinit_mcp_registry`.

- **`src/repl/completer.rs`** — holds
  `Arc<RwLock<RequestContext>>` instead of `GlobalConfig`.

- **`src/repl/prompt.rs`** — holds `Arc<RwLock<RequestContext>>`
  instead of `GlobalConfig`.

- **`src/repl/highlighter.rs`** — updated if it held `GlobalConfig`.

- **`src/config/macros.rs`** — `macro_execute` takes
  `&mut RequestContext` instead of `&GlobalConfig`.

- **`src/main.rs`** — all `to_global_config()` calls eliminated.
  Agent path uses `ctx.use_agent()`. Macro path passes
  `&mut ctx` directly.

### Methods added

- `RequestContext::use_agent(app, name, session, abort_signal)` —
  calls `Agent::init`, sets up MCP via `rebuild_tool_scope`,
  sets agent/rag/supervisor, starts session.

### Methods deleted

- `RequestContext::to_global_config()`
- `RequestContext::sync_mcp_from_registry()`
- REPL: `sync_ctx_to_config`, `sync_config_to_ctx`,
  `sync_app_config_to_ctx`, `reinit_mcp_registry`

### UX improvements

- MCP loading spinner restored in `rebuild_tool_scope`
- `.set enabled_mcp_servers<TAB>` shows configured servers from
  `mcp.json` + mapping aliases
- `.set enabled_tools<TAB>` hides internal tools (`user__*`,
  `mcp_*`, `todo__*`, `agent__*`)

## GlobalConfig remaining

Only `src/config/mod.rs` (13 references): type definition, legacy
`Config::use_agent`, `Config::use_session_safely`,
`Config::use_role_safely`, `Config::update`, `Config::delete` — all
dead code. Step 15 deletes them.

## Post-implementation review (Oracle)

Oracle reviewed all REPL and CLI flows. Findings:

1. **AbortSignal not threaded through rebuild_tool_scope** —
   FIXED. `rebuild_tool_scope`, `bootstrap_tools`, `use_role`,
   `use_session`, `use_agent`, `update` now all thread the real
   `AbortSignal` through to the MCP loading spinner. Ctrl+C
   properly cancels MCP server loading.

2. **RwLock held across await in REPL** — KNOWN LIMITATION.
   `Repl::run` holds `ctx.write()` for the duration of
   `run_repl_command`. This is safe in the current design because
   reedline's prompt/completion is synchronous (runs between line
   reads, before the write lock is taken). Phase 2 should refactor
   to owned `RequestContext` + lightweight snapshot for reedline.

3. **MCP subprocess leaks** — NOT AN ISSUE. `rmcp::RunningService`
   has a `DropGuard` that cancels the tokio cancellation token on
   Drop. Servers are killed when their `Arc<ConnectedServer>`
   refcount hits zero.

4. **MCP duplication** — NOT AN ISSUE after Step 14. The
   `initial_global` sync was removed. MCP runtime is populated
   only by `rebuild_tool_scope` → `McpFactory::acquire`, which
   deduplicates via `Weak` references.

5. **Agent+session MCP override** — PRE-EXISTING behavior, not
   a regression. When an agent session has its own MCP config,
   it takes precedence. Supervisor child agents handle this
   explicitly via `populate_agent_mcp_runtime`.

6. **Stale Input in tool loop** — PRE-EXISTING design. Input
   captures state at construction time and uses `merge_tool_results`
   for continuations. Tools communicate results via tool results,
   not by mutating the session mid-turn. Not a regression.

7. **Auto-compression** — REPL does inline compression in `ask`.
   CLI directive path relies on session save which happens in
   `after_chat_completion`. Consistent with pre-migration behavior.

## Verification

- `cargo check` — 6 dead-code warnings (legacy Config methods)
- `cargo test` — 63 passed, 0 failed
