# Phase 1 Step 8g — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8g: REPL rewrite — `repl/mod.rs`"

## Summary

Rewrote `src/repl/mod.rs` to thread `RequestContext` through
`run_repl_command` and `ask` alongside the existing `GlobalConfig`.
The `Repl` struct now owns both a `RequestContext` (source of truth
for runtime state) and a `GlobalConfig` (read-only view for reedline
components: prompt, completer, highlighter). Bidirectional sync
helpers keep them in lockstep after mutations.

Also updated `src/main.rs` to pass `RequestContext` into `Repl::init`
and `src/config/macros.rs` to construct a temporary `RequestContext`
for `run_repl_command` calls from macro execution.

## What was changed

### Files modified (5 files)

- **`src/repl/mod.rs`** — major rewrite:
  - `Repl` struct: added `ctx: RequestContext` field
  - `Repl::init`: takes `RequestContext` (by value), builds
    `GlobalConfig` from `ctx.to_global_config()` for reedline
  - `Repl::run`: passes both `&self.config` and `&mut self.ctx`
    to `run_repl_command`
  - `run_repl_command`: signature changed to
    `(config, ctx, abort_signal, line) -> Result<bool>`.
    Command handlers use `ctx.*` methods where available,
    fall through to `config.*` for unmigrated operations.
    Sync helpers called after mutations.
  - `ask`: signature changed to
    `(config, ctx, abort_signal, input, with_embeddings) -> Result<()>`.
    Uses `ctx.before_chat_completion`, `ctx.after_chat_completion`.
    Keeps `Config::compress_session`, `Config::maybe_compress_session`,
    `Config::maybe_autoname_session` on the GlobalConfig path
    (they spawn tasks).
  - Added `sync_ctx_to_config` and `sync_config_to_ctx` helpers
    for bidirectional state synchronization.

- **`src/main.rs`** — `start_interactive` takes `RequestContext`
  by value, passes it into `Repl::init`. The `run()` function's
  REPL branch moves `ctx` into `start_interactive`.

- **`src/config/macros.rs`** — `macro_execute` constructs a
  temporary `AppState` + `RequestContext` from the `GlobalConfig`
  to satisfy `run_repl_command`'s new signature.

- **`src/config/mod.rs`** — `#[allow(dead_code)]` annotations on
  additional methods that became dead after the REPL migration.

- **`src/config/bridge.rs`** — minor adjustments for compatibility.

### Files NOT changed

- **`src/repl/completer.rs`** — still holds `GlobalConfig` (owned
  by reedline's `Box<dyn Completer>`)
- **`src/repl/prompt.rs`** — still holds `GlobalConfig` (owned by
  reedline's prompt system)
- **`src/repl/highlighter.rs`** — still holds `GlobalConfig`

## Key decisions

### 1. Dual-ownership pattern (GlobalConfig + RequestContext)

The reedline library takes ownership of `Completer`, `Prompt`, and
`Highlighter` as trait objects. These implement reedline traits and
need to read config state (current role, session, model) to render
prompts and generate completions. They can't hold `&RequestContext`
because their lifetime is tied to `Reedline`, not to the REPL turn.

Solution: `Repl` holds both types. `RequestContext` is the source
of truth. After each mutation on `ctx`, `sync_ctx_to_config` copies
runtime fields to the `GlobalConfig` so the reedline components see
the updates. After operations that mutate the `GlobalConfig` (escape
hatch paths like `Config::use_agent`), `sync_config_to_ctx` copies
back.

### 2. `.exit role/session/agent` keep the MCP reinit on GlobalConfig path

The `.exit role`, `.exit session`, and `.exit agent` handlers do
`McpRegistry::reinit` which takes the registry out of `Config`,
reinits it, and puts it back. This pattern requires `GlobalConfig`
and can't use `RequestContext::rebuild_tool_scope` without a larger
refactor. These handlers stay on the GlobalConfig path with
sync-back.

### 3. `macro_execute` builds a temporary RequestContext

`macro_execute` in `config/macros.rs` calls `run_repl_command` which
now requires `&mut RequestContext`. Since `macro_execute` receives
`&GlobalConfig`, it constructs a temporary `AppState` +
`RequestContext` from it. This is a bridge-window artifact — macro
execution within the REPL creates an isolated `RequestContext` that
doesn't persist state back.

### 4. `ask`'s auto-continuation and compression stay on GlobalConfig

The auto-continuation loop and session compression in `ask` use
`Config::maybe_compress_session`, `Config::compress_session`, and
`Config::maybe_autoname_session` which spawn tasks and need the
`GlobalConfig`. These stay on the old path with sync-back after
completion.

## Deviations from plan

| Deviation | Rationale |
|---|---|
| `ReplCompleter`/`ReplPrompt` not changed to RequestContext | reedline owns them as trait objects; need shared `GlobalConfig` |
| `.exit *` MCP reinit on GlobalConfig path | McpRegistry::reinit pattern requires GlobalConfig |
| Bidirectional sync helpers added | Bridge necessity for dual-ownership |
| `macro_execute` builds temporary RequestContext | run_repl_command signature requires it |

## Verification

### Compilation

- `cargo check` — clean, zero warnings, zero errors
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged)

## Handoff to next steps

### Phase 1 Step 8 is now complete

All sub-steps 8a through 8g (plus 8h first pass) are done:
- 8a: `Model::retrieve_model` → `&AppConfig`
- 8b: Mixed-method migrations (retrieve_role, set_model, etc.)
- 8c: `McpFactory::acquire` extracted from `McpRegistry`
- 8d: Scope transitions (use_role, use_session, exit_agent)
- 8e: Session lifecycle + apply_prelude
- 8f: main.rs rewrite
- 8g: REPL rewrite
- 8h: Bridge wrappers for leaf dependencies

### What Steps 9-10 need to do

**Step 9: Remove the bridge**
- Delete `Config::from_parts`, `Config::to_app_config`,
  `Config::to_request_context`
- Rewrite `Input` to hold `&AppConfig` + `&RequestContext` instead
  of `GlobalConfig`
- Rewrite `Rag` to take `&AppConfig` instead of `&GlobalConfig`
- Rewrite `Agent::init` to take `&AppState` + `&mut RequestContext`
- Eliminate `to_global_config()` escape hatches
- Eliminate `sync_ctx_to_config`/`sync_config_to_ctx` helpers
- Rewrite `ReplCompleter`/`ReplPrompt` to use `RequestContext`
  (requires reedline component redesign)

**Step 10: Delete Config**
- Remove `Config` struct and `GlobalConfig` type alias
- Remove `bridge.rs` module
- Remove all `#[allow(dead_code)]` annotations on Config methods
- Delete the `_safely` wrappers

### Files to re-read at the start of Step 9

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Steps 9-10
- This notes file
- `src/config/mod.rs` — remaining `Config` methods
- `src/config/bridge.rs` — bridge conversions to delete
- `src/config/input.rs` — `Input` struct (holds GlobalConfig)
- `src/rag/mod.rs` — `Rag` struct (holds GlobalConfig)

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 8f notes: `docs/implementation/PHASE-1-STEP-8f-NOTES.md`
- Step 8h notes: `docs/implementation/PHASE-1-STEP-8h-NOTES.md`
- Modified files:
  - `src/repl/mod.rs` (major rewrite — sync helpers, dual ownership)
  - `src/main.rs` (start_interactive signature change)
  - `src/config/macros.rs` (temporary RequestContext construction)
  - `src/config/mod.rs` (dead_code annotations)
  - `src/config/bridge.rs` (compatibility adjustments)
