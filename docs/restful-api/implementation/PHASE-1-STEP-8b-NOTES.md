# Phase 1 Step 8b — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8b: Finish Step 7's deferred mixed-method migrations"

## Summary

Migrated 7 of the 9 Step 7 deferrals to `RequestContext` / `AppConfig`
methods that take `&AppConfig` instead of `&Config`. Two methods
(`edit_role` and `update`) remain deferred because they depend on
`use_role` (Step 8d) and MCP registry manipulation (Step 8d)
respectively. Four private helper functions in `mod.rs` were bumped
to `pub(super)` to support the new `repl_complete` implementation.

## What was changed

### Files modified (3 files)

- **`src/config/request_context.rs`** — added a fifth `impl RequestContext`
  block with 7 methods:

  - `retrieve_role(&self, app: &AppConfig, name: &str) -> Result<Role>` —
    loads a role by name, resolves its model via
    `Model::retrieve_model(app, ...)`. Reads `app.temperature` and
    `app.top_p` for the no-model-id fallback branch.

  - `set_model_on_role_like(&mut self, app: &AppConfig, model_id: &str)
    -> Result<bool>` — resolves the model via `Model::retrieve_model`,
    sets it on the active role-like if present (returns `true`), or on
    `ctx.model` directly (returns `false`). The `false` case means the
    caller should also call `AppConfig::set_model_id_default` if they
    want the global default updated.

  - `reload_current_model(&mut self, app: &AppConfig, model_id: &str)
    -> Result<()>` — resolves a model by ID and assigns it to
    `ctx.model`. Used in tandem with `AppConfig::ensure_default_model_id`.

  - `use_prompt(&mut self, _app: &AppConfig, prompt: &str) -> Result<()>` —
    creates a `TEMP_ROLE_NAME` role with the prompt text, sets its model
    to `current_model()`, calls `use_role_obj`. The `_app` parameter is
    included for signature consistency; it's unused because `use_prompt`
    only reads runtime state.

  - `set_rag_reranker_model(&mut self, app: &AppConfig,
    value: Option<String>) -> Result<bool>` — validates the model ID via
    `Model::retrieve_model(app, ...)` if present, then clones-and-replaces
    the `Arc<Rag>` with the updated reranker model. Returns `true` if RAG
    was mutated, `false` if no RAG is active.

  - `set_rag_top_k(&mut self, value: usize) -> Result<bool>` — same
    clone-and-replace pattern on the active RAG. Returns `true`/`false`.

  - `repl_complete(&self, app: &AppConfig, cmd: &str, args: &[&str],
    _line: &str) -> Vec<(String, Option<String>)>` — full tab-completion
    handler. Reads `app.*` for serialized fields, `self.*` for runtime
    state, `self.app.vault` for vault completions. MCP configured-server
    completions are limited to `app.mapping_mcp_servers` keys during the
    bridge (no live `McpRegistry` on `RequestContext`; Step 8d's
    `ToolScope` will restore full MCP completions).

  Updated imports: added `TEMP_ROLE_NAME`, `list_agents`, `ModelType`,
  `list_models`, `read_to_string`, `fuzzy_filter`. Removed duplicate
  `crate::utils` import that had accumulated.

- **`src/config/app_config.rs`** — added 4 methods to the existing
  `set_*_default` impl block:

  - `set_rag_reranker_model_default(&mut self, value: Option<String>)`
  - `set_rag_top_k_default(&mut self, value: usize)`
  - `set_model_id_default(&mut self, model_id: String)`
  - `ensure_default_model_id(&mut self) -> Result<String>` — picks the
    first available chat model if `model_id` is empty, updates
    `self.model_id`, returns the resolved ID.

- **`src/config/mod.rs`** — bumped 4 private helper functions to
  `pub(super)`:

  - `parse_value` — used by `update` when it migrates (Step 8f/8g)
  - `complete_bool` — used by `repl_complete`
  - `complete_option_bool` — used by `repl_complete`
  - `map_completion_values` — used by `repl_complete`

### Files NOT changed

- **`src/client/macros.rs`**, **`src/client/model.rs`** — untouched;
  Step 8a already migrated these.
- **All other source files** — no changes. All existing `Config` methods
  stay intact.

## Key decisions

### 1. Same bridge pattern as Steps 3-8a

New methods sit alongside originals. No caller migration. `Config`'s
`retrieve_role`, `set_model`, `setup_model`, `use_prompt`,
`set_rag_reranker_model`, `set_rag_top_k`, `repl_complete` all stay
on `Config` and continue working for every current caller.

### 2. `set_model_on_role_like` returns `Result<bool>` (not just `bool`)

Unlike the Step 7 `set_temperature_on_role_like` pattern that returns
a plain `bool`, `set_model_on_role_like` returns `Result<bool>` because
`Model::retrieve_model` can fail. The `bool` still signals whether a
role-like was mutated. When `false`, the model was assigned to
`ctx.model` directly (so the caller doesn't need to fall through to
`AppConfig` — the "no role-like" case is handled in-method by assigning
to `ctx.model`). This differs from the Step 7 pattern where `false`
means "caller must call the `_default`."

### 3. `setup_model` split into two independent methods

`Config::setup_model` does three things:
1. Picks a default model ID if empty (`ensure_default_model_id`)
2. Calls `set_model` to resolve and assign the model
3. Writes back `model_id` to config

The split:
- `AppConfig::ensure_default_model_id()` handles #1 and #3
- `RequestContext::reload_current_model()` handles #2

Step 8f will compose them: first call `ensure_default_model_id` on
the app config, then call `reload_current_model` on the context
with the returned ID.

### 4. `repl_complete` MCP completions are reduced during bridge

`Config::repl_complete` reads `self.mcp_registry.list_configured_servers()`
for the `enabled_mcp_servers` completion values. `RequestContext` has no
`mcp_registry` field. During the bridge window, the new `repl_complete`
offers only `mapping_mcp_servers` keys (from `AppConfig`) as MCP
completions. Step 8d's `ToolScope` will provide full MCP server
completions.

This is acceptable because:
- The new method isn't called by anyone yet (bridge pattern)
- When Step 8d wires it up, `ToolScope` will be available

### 5. `edit_role` deferred to Step 8d

`Config::edit_role` calls `self.use_role()` as its last line.
`use_role` is a scope-transition method that Step 8d will rewrite
to use `McpFactory::acquire()`. Migrating `edit_role` without
`use_role` would require either a stub or leaving it half-broken.
Deferring it keeps the bridge clean.

### 6. `update` dispatcher deferred to Step 8f/8g

`Config::update` takes `&GlobalConfig` and has two branches that
do heavy MCP registry manipulation (`enabled_mcp_servers` and
`mcp_server_support`). These branches require Step 8d's
`McpFactory`/`ToolScope` infrastructure. The remaining branches
could be migrated individually, but splitting the dispatcher
partially creates a confusing dual-path situation. Deferring the
entire dispatcher keeps things clean.

### 7. RAG mutation uses clone-and-replace on `Arc<Rag>`

`Config::set_rag_reranker_model` uses the `update_rag` helper which
takes `&GlobalConfig`, clones the `Arc<Rag>`, mutates the clone,
and writes it back via `config.write().rag = Some(Arc::new(rag))`.

The new `RequestContext` methods do the same thing but without the
`GlobalConfig` indirection: clone `Arc<Rag>` contents, mutate,
wrap in a new `Arc`, assign to `self.rag`. Semantically identical.

## Deviations from plan

### 2 methods deferred (not in plan's "done" scope for 8b)

| Method | Why deferred |
|---|---|
| `edit_role` | Calls `use_role` which is Step 8d |
| `update` | MCP registry branches require Step 8d's `McpFactory`/`ToolScope` |

The plan's 8b description listed both as potential deferrals:
- `edit_role`: "calls editor + upsert_role + use_role — use_role is
  still Step 8d, so edit_role may stay deferred"
- `update`: "Once all the individual set_* methods exist on both types"
  — the MCP-touching set_* methods don't exist yet

### `set_model_on_role_like` handles the no-role-like case internally

The plan said the split should be:
- `RequestContext::set_model_on_role_like` → returns `bool`
- `AppConfig::set_model_default` → sets global

But `set_model` doesn't just set `model_id` when no role-like is
active — it also assigns the resolved `Model` struct to `self.model`
(runtime). Since the `Model` struct lives on `RequestContext`, the
no-role-like branch must also live on `RequestContext`. So
`set_model_on_role_like` handles both cases (role-like mutation and
`ctx.model` assignment) and returns `false` to signal that `model_id`
on `AppConfig` may also need updating. `AppConfig::set_model_id_default`
is the simpler companion.

## Verification

### Compilation

- `cargo check` — clean, zero warnings, zero errors
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from Steps 1–8a)

No new tests added — this is a bridge-pattern step that adds methods
alongside existing ones. The existing test suite confirms no regressions.

## Handoff to next step

### What Step 8c can rely on

Step 8c (extract `McpFactory::acquire()` from `McpRegistry::init_server`)
can rely on:

- **All Step 8a guarantees still hold** — `Model::retrieve_model`,
  `list_models`, `list_all_models`, `list_client_names` all take
  `&AppConfig`
- **`RequestContext` now has 46 inherent methods** across 5 impl blocks:
  1 constructor + 13 reads + 12 writes + 14 mixed (Step 7) + 7 mixed
  (Step 8b) = 47 total (46 public + 1 private `open_message_file`)
- **`AppConfig` now has 21 methods**: 7 reads + 4 writes + 10
  setter-defaults (6 from Step 7 + 4 from Step 8b)

### What Step 8c should watch for

Step 8c is **independent of Step 8b**. It extracts the MCP subprocess
spawn logic from `McpRegistry::init_server` into a standalone function
and implements `McpFactory::acquire()`. Step 8b provides no input to
8c.

### What Step 8d should know about Step 8b's output

Step 8d (scope transitions) depends on both 8b and 8c. From 8b it
gets:

- `RequestContext::retrieve_role(app, name)` — needed by `use_role`
- `RequestContext::set_model_on_role_like(app, model_id)` — may be
  useful inside scope transitions

### What Step 8f/8g should know about Step 8b deferrals

- **`edit_role`** — needs `use_role` from Step 8d. Once 8d ships,
  `edit_role` on `RequestContext` becomes: call `app.editor()`, call
  `upsert_role(name)`, call `self.use_role(app, name, abort_signal)`.
  The `upsert_role` method is still on `Config` and needs migrating
  (it calls `self.editor()` which is on `AppConfig`, and
  `ensure_parent_exists` which is a free function — straightforward).

- **`update` dispatcher** — needs all `set_*` branches migrated. The
  non-MCP branches are ready now. The MCP branches need Step 8d's
  `McpFactory`/`ToolScope`.

- **`use_role_safely` / `use_session_safely`** — still on `Config`.
  These wrappers exist only because `Config::use_role` is `&mut self`
  and the REPL holds `Arc<RwLock<Config>>`. Step 8g eliminates them
  when the REPL switches to holding `RequestContext` directly.

### Bridge-window duplication count at end of Step 8b

Running tally:

- `AppConfig` (Steps 3+4+7+8b): 21 methods
- `RequestContext` (Steps 5+6+7+8b): 46 methods
- `paths` module (Step 2): 33 free functions
- Step 6.5 types: 4 new types on scaffolding
- `mod.rs` visibility bumps: 4 helpers → `pub(super)`

**Total: 67 methods + 33 paths + 4 types / ~1500 lines of parallel logic**

All auto-delete in Step 10.

### Files to re-read at the start of Step 8c

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8c section
- `src/mcp/mod.rs` — `McpRegistry::init_server` method body (the
  spawn logic to extract)
- `src/config/mcp_factory.rs` — current scaffolding from Step 6.5

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 8a notes: `docs/implementation/PHASE-1-STEP-8a-NOTES.md`
- Step 7 notes: `docs/implementation/PHASE-1-STEP-7-NOTES.md`
- Modified files:
  - `src/config/request_context.rs` (7 new methods, import updates)
  - `src/config/app_config.rs` (4 new `set_*_default` / `ensure_*`
    methods)
  - `src/config/mod.rs` (4 helper functions bumped to `pub(super)`)
