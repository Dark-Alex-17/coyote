# Phase 1 Step 8f — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8f: Entry point rewrite — `main.rs`"

## Summary

Rewrote `src/main.rs` to thread `RequestContext` instead of
`GlobalConfig` through the entire call chain. All 5 main functions
(`run`, `start_directive`, `create_input`, `shell_execute`,
`start_interactive`) now take `&mut RequestContext` (or
`&RequestContext`). The `apply_prelude_safely` wrapper was
eliminated. Three escape hatches remain where `ctx.to_global_config()`
bridges to functions that still require `&GlobalConfig`:
`Agent::init`, `Config::use_agent`, `Repl::init`.

Also added `RequestContext::bootstrap_tools` (earlier infrastructure
pass) and `#[allow(dead_code)]` to 4 `Config` methods that became
dead after `main.rs` stopped calling them.

## What was changed

### Files modified (2 files)

- **`src/main.rs`** — full rewrite of the call chain:

  - `main()` — still calls `Config::init(...)` to get the initial
    config, then constructs `AppState` + `RequestContext` from it
    via `cfg.to_app_config()` + `cfg.to_request_context(app_state)`.
    Passes `&mut ctx` to `run()`.

  - `run(&mut RequestContext, Cli, text, abort_signal)` — replaces
    `run(GlobalConfig, Cli, text, abort_signal)`. Uses
    `RequestContext` methods directly:
    - `ctx.use_prompt()`, `ctx.use_role()`, `ctx.use_session()`
    - `ctx.use_rag()`, `ctx.rebuild_rag()`
    - `ctx.set_model_on_role_like()`, `ctx.empty_session()`
    - `ctx.set_save_session_this_time()`, `ctx.list_sessions()`
    - `ctx.info()`, `ctx.apply_prelude()`
    Uses `ctx.to_global_config()` for: `Agent::init`,
    `Config::use_agent`, `macro_execute`.

  - `start_directive(&mut RequestContext, input, code_mode,
    abort_signal)` — uses `ctx.before_chat_completion()` and
    `ctx.after_chat_completion()` instead of
    `config.write().before_chat_completion()`.

  - `create_input(&RequestContext, text, file, abort_signal)` —
    uses `Input::from_str_ctx()` and
    `Input::from_files_with_spinner_ctx()`.

  - `shell_execute(&mut RequestContext, shell, input, abort_signal)` —
    uses `ctx.before_chat_completion()`,
    `ctx.after_chat_completion()`, `ctx.retrieve_role()`,
    `Input::from_str_ctx()`. Reads `app.dry_run`,
    `app.save_shell_history` from `AppConfig`.

  - `start_interactive(&RequestContext)` — uses
    `ctx.to_global_config()` to build the `GlobalConfig` needed by
    `Repl::init`.

  - **Removed:** `apply_prelude_safely` — replaced by direct call
    to `ctx.apply_prelude(app, abort_signal)`.

  - **Added:** `update_app_config(ctx, closure)` helper — clones
    `AppConfig` + `AppState` to mutate a single serialized field
    (e.g., `dry_run`, `stream`). Needed during the bridge window
    because `AppConfig` is behind `Arc` and can't be mutated
    in-place.

  - **Removed imports:** `parking_lot::RwLock`, `mem`,
    `GlobalConfig`, `macro_execute` (direct use). Added:
    `AppConfig`, `AppState`, `RequestContext`.

- **`src/config/mod.rs`** — added `#[allow(dead_code)]` to 4
  methods that became dead after `main.rs` stopped calling them:
  `info`, `set_save_session_this_time`, `apply_prelude`,
  `sync_models_url`. These will be deleted in Step 10.

### Files NOT changed

- **All other source files** — no changes. The REPL, agent, input,
  rag, and function modules still use `&GlobalConfig` internally.

## Key decisions

### 1. Agent path uses `to_global_config()` with full state sync-back

`Config::use_agent` takes `&GlobalConfig` and does extensive setup:
`Agent::init`, RAG loading, supervisor creation, session activation.
After the call, all runtime fields (model, functions, role, session,
rag, agent, supervisor, agent_variables, last_message) are synced
back from the temporary `GlobalConfig` to `ctx`.

### 2. `update_app_config` for serialized field mutations

`dry_run` and `stream` live on `AppConfig` (serialized state), not
`RequestContext` (runtime state). Since `AppConfig` is behind
`Arc<AppConfig>` inside `Arc<AppState>`, mutating it requires
cloning both layers. The `update_app_config` helper encapsulates
this clone-mutate-replace pattern. This is a bridge-window
artifact — Phase 2's mutable `AppConfig` will eliminate it.

### 3. `macro_execute` still uses `GlobalConfig`

`macro_execute` calls `run_repl_command` which takes `&GlobalConfig`.
Migrating `run_repl_command` is Step 8g scope (REPL rewrite). For
now, `macro_execute` is called via the original function with a
`ctx.to_global_config()` escape hatch.

### 4. Four `Config` methods marked dead

`Config::info`, `Config::set_save_session_this_time`,
`Config::apply_prelude`, `Config::sync_models_url` were only called
from `main.rs`. After the rewrite, `main.rs` calls the
`RequestContext`/`AppConfig` equivalents instead. The methods are
marked `#[allow(dead_code)]` rather than deleted because:
- `repl/mod.rs` may still reach some of them indirectly
- Step 10 deletes all `Config` methods

## Deviations from plan

| Deviation | Rationale |
|---|---|
| Still calls `Config::init(...)` | No `AppState::init` yet; Step 9-10 scope |
| 3 escape hatches via `to_global_config()` | Agent::init, Config::use_agent, Repl::init still need `&GlobalConfig` |
| `macro_execute` still via GlobalConfig | `run_repl_command` is Step 8g scope |

## Verification

### Compilation

- `cargo check` — clean, zero warnings, zero errors
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged)

## Handoff to next step

### What Step 8g (REPL rewrite) needs

The REPL (`src/repl/mod.rs`) currently holds `GlobalConfig` and
calls `Config` methods throughout. Step 8g should:

1. Change `Repl` struct to hold `RequestContext` (or receive it
   from `start_interactive`)
2. Rewrite all 39+ command handlers to use `RequestContext` methods
3. Eliminate `use_role_safely` / `use_session_safely` wrappers
4. Use `to_global_config()` for any remaining `&GlobalConfig` needs

### Files to re-read at the start of Step 8g

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8g section
- This notes file
- `src/repl/mod.rs` — full REPL implementation
- `src/repl/completer.rs`, `src/repl/prompt.rs` — REPL support

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 8h notes: `docs/implementation/PHASE-1-STEP-8h-NOTES.md`
- Step 8e notes: `docs/implementation/PHASE-1-STEP-8e-NOTES.md`
- Modified files:
  - `src/main.rs` (full rewrite — 586 lines, 5 function signatures
    changed, 1 function removed, 1 helper added)
  - `src/config/mod.rs` (4 methods marked `#[allow(dead_code)]`)
