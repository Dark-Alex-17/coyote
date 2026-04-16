# Phase 1 Step 8e — Implementation Notes

## Status

Done (partial — 3 of 8 methods migrated, 5 deferred).

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8e: RAG lifecycle + session compression +
  `apply_prelude`"

## Summary

Migrated 3 of the 8 planned Category C deferrals from Step 6.
The other 5 methods are blocked on `Input::from_str` and/or
`Rag::init`/`Rag::load`/`Rag::refresh_document_paths` still
taking `&GlobalConfig`. Those are Step 8h migration targets.

## What was changed

### Files modified (1 file)

- **`src/config/request_context.rs`** — added 3 methods in a new
  impl block:

  - `apply_prelude(&mut self, app: &AppConfig, abort_signal) ->
    Result<()>` — reads `app.repl_prelude` or `app.cmd_prelude`
    based on `self.working_mode`, parses the `type:name` format,
    calls `self.use_role(app, ...)` or `self.use_session(app, ...)`
    from Step 8d. Verbatim logic from `Config::apply_prelude`
    except it reads prelude from `app.*` instead of `self.*`.

  - `maybe_compress_session(&mut self, app: &AppConfig) -> bool` —
    checks `session.needs_compression(app.compression_threshold)`,
    sets `session.set_compressing(true)`, returns `true` if
    compression is needed. The caller is responsible for spawning
    the actual compression task and printing the status message.
    This is the semantic change from the plan: the original
    `Config::maybe_compress_session(GlobalConfig)` spawned a
    `tokio::spawn` internally; the new method returns a bool and
    leaves task spawning to the caller.

  - `maybe_autoname_session(&mut self) -> bool` — checks
    `session.need_autoname()`, sets `session.set_autonaming(true)`,
    returns `true`. Same caller-responsibility pattern as
    `maybe_compress_session`.

## Key decisions

### 1. `maybe_*` methods return bool instead of spawning tasks

The plan explicitly called for this: "the new
`RequestContext::maybe_compress_session` returns a bool; callers
that want async compression spawn the task themselves." This makes
the methods pure state transitions with no side effects beyond
setting the compressing/autonaming flags.

The callers (Step 8f's `main.rs`, Step 8g's `repl/mod.rs`) will
compose the bool with task spawning:

```rust
if ctx.maybe_compress_session(app) {
    let color = if app.light_theme() { LightGray } else { DarkGray };
    print!("\n📢 {}\n", color.italic().paint("Compressing the session."));
    tokio::spawn(async move { ... });
}
```

### 2. `maybe_autoname_session` takes no `app` parameter

Unlike `maybe_compress_session` which reads
`app.compression_threshold`, `maybe_autoname_session` only checks
`session.need_autoname()` which is a session-internal flag. No
`AppConfig` data needed.

### 3. Five methods deferred to Step 8h

| Method | Blocking dependency |
|---|---|
| `compress_session` | `Input::from_str(&GlobalConfig, ...)` |
| `autoname_session` | `Input::from_str(&GlobalConfig, ...)` + `Config::retrieve_role` |
| `use_rag` | `Rag::init(&GlobalConfig, ...)`, `Rag::load(&GlobalConfig, ...)` |
| `edit_rag_docs` | `rag.refresh_document_paths(..., &GlobalConfig, ...)` |
| `rebuild_rag` | `rag.refresh_document_paths(..., &GlobalConfig, ...)` |

All 5 are blocked on the same root cause: `Input` and `Rag` types
still take `&GlobalConfig`. These types are listed under Step 8h in
the plan's "Callsite Migration Summary" table:

- `config/input.rs` — `Input::from_str`, `from_files`,
  `from_files_with_spinner` → Step 8h
- `rag/mod.rs` — RAG init, load, search → Step 8e (lifecycle) +
  Step 8h (remaining)

The plan's Step 8e description assumed these would be migrated as
part of 8e, but the actual dependency chain makes them 8h work.
The `RagCache` scaffolding from Step 6.5 doesn't have a working
`load` method yet — it needs `Rag::load` to be migrated first.

### 4. `apply_prelude` calls Step 8d's `use_role`/`use_session`

This is the first method to call other `RequestContext` async
methods (Step 8d's scope transitions). It demonstrates that the
layering works: Step 8d methods are called by Step 8e methods,
which will be called by Step 8f/8g entry points.

## Deviations from plan

| Deviation | Rationale |
|---|---|
| 5 methods deferred to Step 8h | `Input`/`Rag` still take `&GlobalConfig` |
| `RagCache::load` not wired | `Rag::load(&GlobalConfig)` blocks it |
| No `compress_session` or `autoname_session` | Require `Input::from_str` migration |

The plan's description of Step 8e included all 8 methods. In
practice, the `Input`/`Rag` dependency chain means only the
"check + flag" methods (`maybe_*`) and the "compose existing
methods" method (`apply_prelude`) can migrate now. The actual
LLM-calling methods (`compress_session`, `autoname_session`) and
RAG lifecycle methods (`use_rag`, `edit_rag_docs`, `rebuild_rag`)
must wait for Step 8h.

## Verification

### Compilation

- `cargo check` — clean, zero warnings, zero errors
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged)

## Handoff to next step

### What Step 8f can rely on

All methods accumulated through Steps 3–8e:

- **`AppConfig`**: 21 methods
- **`RequestContext`**: 56 methods (53 from 8d + 3 from 8e)
- **`Session`**: 2 ctx-compatible constructors
- **`AppState`**: `mcp_config`, `mcp_log_path`, `mcp_factory`,
  `rag_cache`, `vault`
- **`McpFactory`**: `acquire()` working
- **`paths`**: 33 free functions
- **Step 6.5 types**: `ToolScope`, `McpRuntime`, `AgentRuntime`,
  `RagCache`, `RagKey`, `McpServerKey`

### Step 8e deferred methods that Step 8h must handle

| Method | What 8h needs to do |
|---|---|
| `compress_session` | Migrate `Input::from_str` to take `&AppConfig` + `&RequestContext`, then port `compress_session` |
| `autoname_session` | Same + uses `retrieve_role(CREATE_TITLE_ROLE)` which already exists on ctx (8b) |
| `use_rag` | Migrate `Rag::init`/`Rag::load`/`Rag::create` to take `&AppConfig`, wire `RagCache::load` |
| `edit_rag_docs` | Migrate `Rag::refresh_document_paths` to take `&AppConfig` |
| `rebuild_rag` | Same as `edit_rag_docs` |

### Files to re-read at the start of Step 8f

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8f section
- This notes file
- `src/main.rs` — full file (entry point to rewrite)
- Step 8d notes — `use_role`, `use_session` signatures

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 8d notes: `docs/implementation/PHASE-1-STEP-8d-NOTES.md`
- Step 6 notes: `docs/implementation/PHASE-1-STEP-6-NOTES.md`
  (Category C deferral list)
- Modified files:
  - `src/config/request_context.rs` (3 new methods)
