# Phase 1 Step 8j — Implementation Notes

## Status

Done (partial — hot-path methods migrated, `config` field kept for
client creation and embeddings).

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8j: Migrate `Input` and chat completion chain away
  from `GlobalConfig`"

## Summary

Added 3 captured fields to the `Input` struct: `stream_enabled`,
`session`, `functions`. These are populated at construction time
from the `GlobalConfig`, eliminating 5 of 7 `self.config.read()`
calls. The remaining 2 calls (`set_regenerate`, `use_embeddings`)
still need the `GlobalConfig` and are low-frequency.

The `config: GlobalConfig` field is KEPT on `Input` because:
1. `create_client()` calls `init_client(&self.config, ...)` — the
   client holds the `GlobalConfig` and passes it to `eval_tool_calls`
2. `use_embeddings()` calls `Config::search_rag(&self.config, ...)`
3. `set_regenerate()` calls `self.config.read().extract_role()`

Full elimination of `config` from `Input` requires migrating
`init_client`, every client struct, and `eval_tool_calls` — which
is a cross-cutting change across the entire client module.

## What was changed

### Files modified (1 file)

- **`src/config/input.rs`**:
  - Added fields: `stream_enabled: bool`, `session: Option<Session>`,
    `functions: Option<Vec<FunctionDeclaration>>`
  - `from_str`: captures `stream_enabled`, `session`, `functions`
    from `config.read()` at construction time
  - `from_files`: same captures
  - `stream()`: reads `self.stream_enabled` instead of
    `self.config.read().stream`
  - `prepare_completion_data()`: uses `self.functions.clone()`
    instead of `self.config.read().select_functions(...)`
  - `build_messages()`: uses `self.session(...)` with
    `&self.session` instead of `&self.config.read().session`
  - `echo_messages()`: same

### config.read() call reduction

| Method | Before | After |
|---|---|---|
| `stream()` | `self.config.read().stream` | `self.stream_enabled` |
| `prepare_completion_data()` | `self.config.read().select_functions(...)` | `self.functions.clone()` |
| `build_messages()` | `self.config.read().session` | `self.session` |
| `echo_messages()` | `self.config.read().session` | `self.session` |
| `set_regenerate()` | `self.config.read().extract_role()` | unchanged |
| `use_embeddings()` | `self.config.read().rag.clone()` | unchanged |
| `from_files()` (last_message) | `config.read().last_message` | unchanged |

**Total: 7 → 2 config.read() calls** (71% reduction).

## Key decisions

### 1. Kept `config: GlobalConfig` on Input

The `GlobalConfig` that `Input` passes to `init_client` ends up on
the `Client` struct, which passes it to `eval_tool_calls`. The
`eval_tool_calls` function reads `tool_call_tracker`,
`current_depth`, and `root_escalation_queue` from this GlobalConfig.
These are runtime fields that MUST reflect the current state.

If we replaced `config` with a temp GlobalConfig (like Rag's
`build_temp_global_config`), the tool call tracker and escalation
queue would be missing, breaking tool-call loop detection and
sub-agent escalation.

### 2. `eval_tool_calls` migration deferred

The plan listed `eval_tool_calls` migration as part of 8j. This
was deferred because `eval_tool_calls` is called from
`client/common.rs` via `client.global_config()`, and every client
struct holds `global_config: GlobalConfig`. Migrating eval_tool_calls
requires migrating init_client and every client struct — a separate
effort.

### 3. Functions pre-computed at construction time

`select_functions` involves reading `self.functions.declarations()`,
`self.mapping_tools`, `self.mapping_mcp_servers`, and the agent's
functions. Pre-computing this at Input construction time means the
function list is fixed for the duration of the chat turn. This is
correct behavior — tool availability shouldn't change mid-turn.

## Deviations from plan

| Deviation | Rationale |
|---|---|
| `eval_tool_calls` not migrated | Requires client module migration |
| `client/common.rs` not changed | Depends on eval_tool_calls migration |
| `config` field kept on Input | Client → eval_tool_calls needs real GlobalConfig |
| `_ctx` bridge constructors kept | Still useful for main.rs callers |

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean
- `cargo test` — 63 passed, 0 failed

## Handoff to next step

Step 8k (Agent::init migration) can proceed. The Input struct
changes don't affect Agent::init directly — agents create Input
internally via `Input::from_str` which still takes `&GlobalConfig`.

The full `Input` migration (eliminating the `config` field entirely)
is blocked on:
1. Migrating `init_client` to take `&AppConfig` + `&[ClientConfig]`
2. Migrating every client struct to not hold `GlobalConfig`
3. Migrating `eval_tool_calls` to take `&AppConfig` + `&mut RequestContext`

These form a single atomic change that should be its own dedicated
step (possibly Step 8n if needed, or as part of Phase 2).

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8j
- Step 8i notes: `docs/implementation/PHASE-1-STEP-8i-NOTES.md`
- QA checklist: `docs/QA-CHECKLIST.md` — items 2-6, 8, 12, 22
