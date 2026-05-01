# Phase 1 Step 10 — Implementation Notes

## Status

Done. Client chain migrated. `GlobalConfig` reduced to runtime-only
usage (tool evaluation chain + REPL sync).

## Summary

Migrated the entire client chain away from `GlobalConfig`:
- `Client` trait: `global_config()` → `app_config()`
- Client structs: `GlobalConfig` → `Arc<AppConfig>`
- `init_client`: `&GlobalConfig` → `&Arc<AppConfig>`
- `Input` struct: removed `config: GlobalConfig` field entirely
- `Rag`: deleted `build_temp_global_config` bridge
- `render_stream`: `&GlobalConfig` → `&AppConfig`
- `Config::search_rag`: `&GlobalConfig` → `&AppConfig`
- `call_chat_completions*`: explicit `runtime: &GlobalConfig` parameter

## What was changed

### Files modified (10 files)

- **`src/client/macros.rs`** — client structs hold `Arc<AppConfig>`,
  `init` takes `&Arc<AppConfig>`, `init_client` takes
  `&Arc<AppConfig>` + `Model`. Zero GlobalConfig in file.

- **`src/client/common.rs`** — `Client` trait: `app_config() -> &AppConfig`.
  `call_chat_completions*` take explicit `runtime: &GlobalConfig`.

- **`src/config/input.rs`** — removed `config: GlobalConfig` field.
  Added `rag: Option<Arc<Rag>>` captured at construction. Changed
  `set_regenerate` to take `current_role: Role` parameter. Zero
  `self.config` references.

- **`src/config/mod.rs`** — `search_rag` takes `&AppConfig`. Deleted
  dead `rag_template` method.

- **`src/render/mod.rs`** — `render_stream` takes `&AppConfig`. Zero
  GlobalConfig in file.

- **`src/rag/mod.rs`** — deleted `build_temp_global_config`. Creates
  clients via `init_client(&self.app_config, model)`. Zero
  GlobalConfig in file.

- **`src/main.rs`** — updated `call_chat_completions*` calls with
  explicit `runtime` parameter.

- **`src/repl/mod.rs`** — updated `call_chat_completions*` calls,
  `set_regenerate` call with `current_role` parameter.

- **`src/function/supervisor.rs`** — updated `call_chat_completions`
  call in `run_child_agent`.

- **`src/config/app_config.rs`** — no changes (already had all
  needed fields).

## Remaining GlobalConfig usage (71 references)

| Category | Files | Count | Why |
|---|---|---|---|
| Definition | `config/mod.rs` | 13 | Config struct, GlobalConfig alias, methods called by REPL |
| Tool eval chain | `function/mod.rs` | 8 | `eval_tool_calls(&GlobalConfig)`, `ToolCall::eval(&GlobalConfig)` |
| Tool handlers | `function/supervisor.rs` | 17 | All handler signatures |
| Tool handlers | `function/todo.rs` | 2 | Todo handler signatures |
| Tool handlers | `function/user_interaction.rs` | 3 | User interaction handler signatures |
| Runtime param | `client/common.rs` | 3 | `call_chat_completions*(runtime: &GlobalConfig)` |
| Input construction | `config/input.rs` | 4 | Constructor params + capture_input_config |
| REPL | `repl/mod.rs` | 10 | Input construction, ask, sync helpers |
| REPL components | `repl/completer.rs` | 3 | Holds GlobalConfig for reedline |
| REPL components | `repl/prompt.rs` | 3 | Holds GlobalConfig for reedline |
| REPL components | `repl/highlighter.rs` | 2 | Holds GlobalConfig for reedline |
| Bridge | `config/request_context.rs` | 1 | `to_global_config()` |
| Bridge | `config/macros.rs` | 2 | `macro_execute` takes &GlobalConfig |

The remaining GlobalConfig usage falls into 3 categories:
1. **Tool evaluation chain** (30 refs) — `eval_tool_calls` and
   handlers read runtime state from GlobalConfig
2. **REPL** (18 refs) — sync helpers, Input construction, reedline
3. **Definition** (13 refs) — the Config struct itself

## Phase 1 final completion summary

Phase 1 is now complete. Every module that CAN be migrated HAS been
migrated. The remaining GlobalConfig usage is the tool evaluation
chain (which reads runtime state during active tool calls) and the
REPL sync layer (which bridges RequestContext to GlobalConfig for
the tool chain).

### Key achievements
- `Input` no longer holds `GlobalConfig`
- Client structs no longer hold `GlobalConfig`
- `Rag` has zero `GlobalConfig` references
- `render_stream` takes `&AppConfig`
- `Agent::init` takes `&AppConfig` + `&AppState`
- Both entry points thread `RequestContext`
- 64+ methods on `RequestContext`, 21+ on `AppConfig`
- Zero regressions: 63 tests, zero warnings, zero clippy issues

### What Phase 2 starts with
Phase 2 can build REST API endpoints using `AppState` + `RequestContext`
directly. The tool evaluation chain will need to be migrated from
`&GlobalConfig` to `&mut RequestContext` when REST API tool calls
are implemented — at that point, `Config` and `GlobalConfig` can
be fully deleted.

## Verification

- `cargo check` — zero warnings, zero errors
- `cargo clippy` — zero warnings
- `cargo test` — 63 passed, 0 failed
