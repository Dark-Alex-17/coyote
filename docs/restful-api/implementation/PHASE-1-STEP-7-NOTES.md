# Phase 1 Step 7 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 7: Tackle mixed methods (THE HARD PART)"

## Summary

Added 14 mixed-method splits to the new types, plus 6 global-
default setters on `AppConfig`. The methods that mix serialized
config reads/writes with runtime state reads/writes are now
available on `RequestContext` with `&AppConfig` as an explicit
parameter for the serialized half.

Same bridge pattern as Steps 3–6: `Config`'s originals stay
intact, new methods sit alongside, caller migration happens in
Step 8.

**Step 7 completed ~65% of its planned scope.** Nine target
methods were deferred to Step 8 because they transitively
depend on `Model::retrieve_model(&Config)` and
`list_models(&Config)` — refactoring those requires touching
the `client` module macros, which is beyond Step 7's bridge-
pattern scope. Step 8 will rewrite them alongside the entry
point migration.

## What was changed

### Modified files

- **`src/config/app_config.rs`** — added a third `impl AppConfig`
  block with 6 `set_*_default` methods for the serialized-field
  half of the mixed-method splits:
  - `set_temperature_default`
  - `set_top_p_default`
  - `set_enabled_tools_default`
  - `set_enabled_mcp_servers_default`
  - `set_save_session_default`
  - `set_compression_threshold_default`

- **`src/config/request_context.rs`** — added a fourth
  `impl RequestContext` block with 14 methods:

  **Helpers (2):**
  - `current_model(&self) -> &Model` — pure runtime traversal
    (session > agent > role > ctx.model)
  - `extract_role(&self, app: &AppConfig) -> Role` — pure
    runtime except fallback reads `app.temperature`,
    `app.top_p`, `app.enabled_tools`, `app.enabled_mcp_servers`

  **Role-like setters (7):** these all return `bool`
  indicating whether they mutated a `RoleLike` (if `false`,
  the caller should fall back to
  `app.set_<name>_default()`). This preserves the exact
  semantics of today's `Config::set_*` methods:
  - `set_temperature_on_role_like`
  - `set_top_p_on_role_like`
  - `set_enabled_tools_on_role_like`
  - `set_enabled_mcp_servers_on_role_like`
  - `set_save_session_on_session` (uses `self.session` directly,
    not `role_like_mut`)
  - `set_compression_threshold_on_session` (same)
  - `set_max_output_tokens_on_role_like`

  **Chat lifecycle (2):**
  - `save_message(&mut self, app: &AppConfig, input, output)` —
    writes to session if present, else to messages file if
    `app.save` is true
  - `after_chat_completion(&mut self, app, input, output,
    tool_results)` — updates `last_message`, calls
    `save_message` if not `app.dry_run`
  - `open_message_file(&self) -> Result<File>` — private
    helper

  **Info getters (3):**
  - `sysinfo(&self, app: &AppConfig) -> Result<String>` —
    ~70-line display output mixing serialized and runtime
    state
  - `info(&self, app: &AppConfig) -> Result<String>` —
    delegates to `sysinfo` in fallback branch
  - `session_info(&self, app: &AppConfig) -> Result<String>` —
    calls `app.render_options()`

  **Prompt rendering (3):**
  - `generate_prompt_context(&self, app) -> HashMap<&str, String>` —
    builds the template variable map
  - `render_prompt_left(&self, app) -> String`
  - `render_prompt_right(&self, app) -> String`

  **Function selection (3):**
  - `select_enabled_functions(&self, app, role) -> Vec<FunctionDeclaration>` —
    filters `ctx.functions.declarations()` by role's enabled
    tools + agent filters + user interaction functions
  - `select_enabled_mcp_servers(&self, app, role) -> Vec<...>` —
    same pattern for MCP meta-functions
  - `select_functions(&self, app, role) -> Option<Vec<...>>` —
    combines both

- **`src/config/mod.rs`** — bumped `format_option_value` from
  private to `pub(super)` so `request_context.rs` can use it
  as `super::format_option_value`.

### Unchanged files

- **`src/config/mod.rs`** — all Step 7 target methods still
  exist on `Config`. They continue to work for every current
  caller.

## Key decisions

### 1. Same bridge pattern as Steps 3-6

Step 7 follows the same additive pattern as earlier steps: new
methods on `AppConfig` / `RequestContext`, `Config`'s originals
untouched, no caller migration. Caller migration is Step 8.

The plan's Step 7 description implied a semantic rewrite
("split into explicit parameter passing") but that phrasing
applies to the target signatures, not the migration mechanism.
The bridge pattern achieves the same end state — methods with
`(&AppConfig, &RequestContext)` signatures exist and are ready
for Step 8 to call.

### 2. `set_*` methods split into `_on_role_like` + `_default` pair

Today's `Config::set_temperature` does:
```rust
match self.role_like_mut() {
    Some(role_like) => role_like.set_temperature(value),
    None => self.temperature = value,
}
```

The Step 7 split:
```rust
// On RequestContext:
fn set_temperature_on_role_like(&mut self, value) -> bool {
    match self.role_like_mut() {
        Some(rl) => { rl.set_temperature(value); true }
        None => false,
    }
}

// On AppConfig:
fn set_temperature_default(&mut self, value) {
    self.temperature = value;
}
```

**The bool return** is the caller contract: if `_on_role_like`
returns `false`, the caller must call
`app.set_*_default(value)`. This is what Step 8 callers will
do:
```rust
if !ctx.set_temperature_on_role_like(value) {
    Arc::get_mut(&mut app.config).unwrap().set_temperature_default(value);
}
```

(Or more likely, the AppConfig mutation gets hidden behind a
helper on `AppState` since `AppConfig` is behind `Arc`.)

This split is semantically equivalent to the existing
behavior while making the "where the value goes" decision
explicit at the type level.

### 3. `save_message` and `after_chat_completion` migrated together

`after_chat_completion` reads `app.dry_run` and calls
`save_message`, which reads `app.save`. Both got deferred from
Step 6 for exactly this mixed-dependency reason. Step 7
migrates them together:

```rust
pub fn after_chat_completion(
    &mut self,
    app: &AppConfig,
    input: &Input,
    output: &str,
    tool_results: &[ToolResult],
) -> Result<()> {
    if !tool_results.is_empty() { return Ok(()); }
    self.last_message = Some(LastMessage::new(input.clone(), output.to_string()));
    if !app.dry_run {
        self.save_message(app, input, output)?;
    }
    Ok(())
}
```

The `open_message_file` helper moved along with them since
it's only called from `save_message`.

### 4. `format_option_value` visibility bump

`format_option_value` is a tiny private helper in
`src/config/mod.rs` that `sysinfo` uses. Step 7's new
`RequestContext::sysinfo` needs to call it, so I bumped its
visibility from `fn` to `pub(super)`. This is a minimal
change (one word) that lets child modules reuse the helper
without duplicating it.

### 5. `select_*` methods were Step 3 deferrals

The plan's Step 3 table originally listed `select_functions`,
`select_enabled_functions`, and `select_enabled_mcp_servers`
as global-read method targets. Step 3's notes correctly
flagged them as actually-mixed because they read `self.functions`
and `self.agent` (runtime, not serialized).

Step 7 is the right home for them. They take
`(&self, app: &AppConfig, role: &Role)` and read:
- `ctx.functions.declarations()` (runtime — existing flat
  field, will collapse into `tool_scope.functions` in Step 8+)
- `ctx.agent` (runtime)
- `app.function_calling_support`, `app.mcp_server_support`,
  `app.mapping_tools`, `app.mapping_mcp_servers` (serialized)

The implementations are long (~80 lines each) but are
verbatim copies of the `Config` originals with `self.X`
replaced by `app.X` for serialized fields and `self.X`
preserved for runtime fields.

### 6. `session_info` keeps using `crate::render::MarkdownRender`

I didn't add a top-level `use crate::render::MarkdownRender`
because it's only called from `session_info`. Inline
`crate::render::MarkdownRender::init(...)` is clearer than
adding another global import for a single use site.

### 7. Imports grew substantially

`request_context.rs` now imports from 7 new sources compared
to the end of Step 6:
- `super::AppConfig` (for the mixed-method params)
- `super::MessageContentToolCalls` (for `save_message`)
- `super::LEFT_PROMPT`, `super::RIGHT_PROMPT` (for prompt
  rendering)
- `super::ensure_parent_exists` (for `open_message_file`)
- `crate::function::FunctionDeclaration`,
  `crate::function::user_interaction::USER_FUNCTION_PREFIX`
- `crate::mcp::MCP_*_META_FUNCTION_NAME_PREFIX` (3 constants)
- `std::collections::{HashMap, HashSet}`,
  `std::fs::{File, OpenOptions}`, `std::io::Write`,
  `std::path::Path`, `crate::utils::{now, render_prompt}`

This is expected — Step 7's methods are the most
dependency-heavy in Phase 1. Post–Phase 1 cleanup can
reorganize into separate files if the module becomes
unwieldy.

## Deviations from plan

### 9 methods deferred to Step 8

| Method | Why deferred |
|---|---|
| `retrieve_role` | Calls `Model::retrieve_model(&Config)` transitively, needs client module refactor |
| `set_model` | Calls `Model::retrieve_model(&Config)` transitively |
| `set_rag_reranker_model` | Takes `&GlobalConfig`, uses `update_rag` helper with Arc<RwLock> take/replace pattern |
| `set_rag_top_k` | Same as above |
| `update` | Dispatcher over all `set_*` methods including the 2 above, plus takes `&GlobalConfig` and touches `mcp_registry` |
| `repl_complete` | Calls `list_models(&Config)` + reads `self.mcp_registry` (going away in Step 6.5/8), + reads `self.functions` |
| `use_role_safely` | Takes `&GlobalConfig`, does `take()`/`replace()` on Arc<RwLock> |
| `use_session_safely` | Same as above |
| `setup_model` | Calls `self.set_model()` which is deferred |
| `use_prompt` (Step 6 deferral) | Calls `current_model()` (migratable) and `use_role_obj` (migrated in Step 6), but the whole method is 4 lines and not independently useful without its callers |
| `edit_role` (Step 6 deferral) | Calls `self.upsert_role()` and `self.use_role()` which are Step 8 |

**Root cause of most deferrals:** the `client` module's
`list_all_models` macro and `Model::retrieve_model` take
`&Config`. Refactoring them to take `&AppConfig` is a
meaningful cross-module change that belongs in Step 8
alongside the caller migration.

### 14 methods migrated

| Method | New signature |
|---|---|
| `current_model` | `&self -> &Model` (pure RequestContext) |
| `extract_role` | `(&self, &AppConfig) -> Role` |
| `set_temperature_on_role_like` | `(&mut self, Option<f64>) -> bool` |
| `set_top_p_on_role_like` | `(&mut self, Option<f64>) -> bool` |
| `set_enabled_tools_on_role_like` | `(&mut self, Option<String>) -> bool` |
| `set_enabled_mcp_servers_on_role_like` | `(&mut self, Option<String>) -> bool` |
| `set_save_session_on_session` | `(&mut self, Option<bool>) -> bool` |
| `set_compression_threshold_on_session` | `(&mut self, Option<usize>) -> bool` |
| `set_max_output_tokens_on_role_like` | `(&mut self, Option<isize>) -> bool` |
| `save_message` | `(&mut self, &AppConfig, &Input, &str) -> Result<()>` |
| `after_chat_completion` | `(&mut self, &AppConfig, &Input, &str, &[ToolResult]) -> Result<()>` |
| `sysinfo` | `(&self, &AppConfig) -> Result<String>` |
| `info` | `(&self, &AppConfig) -> Result<String>` |
| `session_info` | `(&self, &AppConfig) -> Result<String>` |
| `generate_prompt_context` | `(&self, &AppConfig) -> HashMap<&str, String>` |
| `render_prompt_left` | `(&self, &AppConfig) -> String` |
| `render_prompt_right` | `(&self, &AppConfig) -> String` |
| `select_functions` | `(&self, &AppConfig, &Role) -> Option<Vec<...>>` |
| `select_enabled_functions` | `(&self, &AppConfig, &Role) -> Vec<...>` |
| `select_enabled_mcp_servers` | `(&self, &AppConfig, &Role) -> Vec<...>` |

Actually that's 20 methods across the two types (6 on
`AppConfig`, 14 on `RequestContext`). "14 migrated" refers to
the 14 behavior methods on `RequestContext`; the 6 on
`AppConfig` are the paired defaults for the 7 role-like
setters (4 `set_*_default` + 2 session-specific — the
`set_max_output_tokens` split doesn't need a default
because `ctx.model.set_max_tokens()` works without a
fallback).

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from
  Steps 1–6.5)

The bridge's round-trip test still passes, confirming the new
methods don't interfere with struct layout or the
`Config → AppConfig + RequestContext → Config` invariant.

### Manual smoke test

Not applicable — no runtime behavior changed. CLI and REPL
still call `Config::set_temperature`, `Config::sysinfo`,
`Config::save_message`, etc. as before.

## Handoff to next step

### What Step 8 can rely on

Step 8 (entry point rewrite) can rely on:

- **`AppConfig` now has 17 methods** (Steps 3+4+7): 7 reads
  + 4 writes + 6 setter-defaults
- **`RequestContext` now has 39 inherent methods** across 5
  impl blocks: 1 constructor + 13 reads + 12 writes + 14
  mixed
- **All of `AppConfig`'s and `RequestContext`'s new methods
  are under `#[allow(dead_code)]`** — that's safe to leave
  alone; callers wire them up in Step 8 and the allows
  become inert
- **`format_option_value` is `pub(super)`** — accessible
  from any `config` child module
- **The bridge (`Config::to_app_config`, `to_request_context`,
  `from_parts`) still works** and all round-trip tests pass
- **The `paths` module, Step 3/4 `AppConfig` methods, Step
  5/6 `RequestContext` methods, Step 6.5 scaffolding types
  are all unchanged**
- **These `Config` methods are still on `Config`** and must
  stay there through Step 8 (they're Step 8 targets):
  - `retrieve_role`, `set_model`, `set_rag_reranker_model`,
    `set_rag_top_k`, `update`, `repl_complete`,
    `use_role_safely`, `use_session_safely`, `setup_model`,
    `use_prompt`, `edit_role`
  - Plus the Step 6 Category A deferrals: `use_role`,
    `use_session`, `use_agent`, `exit_agent`
  - Plus the Step 6 Category C deferrals: `compress_session`,
    `maybe_compress_session`, `autoname_session`,
    `maybe_autoname_session`, `use_rag`, `edit_rag_docs`,
    `rebuild_rag`, `apply_prelude`

### What Step 8 should watch for

**Step 8 is the biggest remaining step** after Step 6.5
deferred its scope-transition rewrites. Step 8 now absorbs:

1. **Entry point rewrite** (original Step 8 scope):
   - `main.rs::run()` constructs `AppState` + `RequestContext`
     instead of `GlobalConfig`
   - `main.rs::start_directive()` takes
     `&mut RequestContext` instead of `&GlobalConfig`
   - `main.rs::create_input()` takes `&RequestContext`
   - `repl/mod.rs::Repl` holds a long-lived `RequestContext`
     instead of `GlobalConfig`
   - All 91 callsites in the original migration table

2. **`Model::retrieve_model` refactor** (Step 7 deferrals):
   - `Model::retrieve_model(config: &Config, ...)` →
     `Model::retrieve_model(config: &AppConfig, ...)`
   - `list_all_models!(config: &Config)` macro →
     `list_all_models!(config: &AppConfig)`
   - `list_models(config: &Config, ...)` →
     `list_models(config: &AppConfig, ...)`
   - Then migrate `retrieve_role`, `set_model`,
     `repl_complete`, `setup_model`

3. **RAG lifecycle migration** (Step 7 deferrals +
   Step 6 Category C):
   - `use_rag`, `edit_rag_docs`, `rebuild_rag` →
     `RequestContext` methods using `RagCache`
   - `set_rag_reranker_model`, `set_rag_top_k` → split
     similarly to Step 7 setters

4. **Scope transition rewrites** (Step 6.5 deferrals):
   - `use_role`, `use_session`, `use_agent`, `exit_agent`
     rewritten to build `ToolScope` via `McpFactory`
   - `McpFactory::acquire()` extracted from
     `McpRegistry::init_server`
   - `use_role_safely`, `use_session_safely` eliminated
     (not needed once callers hold `&mut RequestContext`)

5. **Session lifecycle migration** (Step 6 Category C):
   - `compress_session`, `maybe_compress_session`,
     `autoname_session`, `maybe_autoname_session` → methods
     that take `&mut RequestContext` instead of spawning
     tasks with `GlobalConfig`
   - `apply_prelude` → uses migrated `use_role` /
     `use_session`

6. **`update` dispatcher** (Step 7 deferral):
   - Once all `set_*` are available on `RequestContext` and
     `AppConfig`, `update` becomes a dispatcher over the
     new split pair

This is a **huge** step. Consider splitting into 8a-8f
sub-steps or staging across multiple PRs.

### What Step 8 should NOT do

- Don't re-migrate any Step 3-7 method
- Don't touch the new types from Step 6.5 unless actually
  implementing `McpFactory::acquire()` or
  `RagCache::load_with` usage
- Don't leave intermediate states broken — each sub-step
  should keep the build green, even if it means keeping
  temporary dual code paths

### Files to re-read at the start of Step 8

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8 section
- This notes file — specifically the deferrals table and
  Step 8 watch items
- Step 6.5 notes — scope transition rewrite details
- Step 6 notes — Category C deferral inventory
- `src/config/mod.rs` — still has ~25 methods that need
  migrating

## Follow-up (not blocking Step 8)

### 1. Bridge-window duplication count at end of Step 7

Running tally:

- `AppConfig` (Steps 3+4+7): 17 methods (11 reads/writes +
  6 setter-defaults)
- `RequestContext` (Steps 5+6+7): 39 methods (1 constructor +
  13 reads + 12 writes + 14 mixed)
- `paths` module (Step 2): 33 free functions
- Step 6.5 types: 4 new types on scaffolding

**Total bridge-window duplication: 56 methods / ~1200 lines**
(up from 36 / ~550 at end of Step 6).

All auto-delete in Step 10.

### 2. `request_context.rs` is now ~900 lines

Getting close to the point where splitting into multiple
files would help readability. Candidate layout:
- `request_context/mod.rs` — struct definition + constructor
- `request_context/reads.rs` — Step 5 methods
- `request_context/writes.rs` — Step 6 methods
- `request_context/mixed.rs` — Step 7 methods

Not blocking anything; consider during Phase 1 cleanup.

### 3. The `set_*_on_role_like` / `set_*_default` split
    has an unusual caller contract

Callers of the split have to remember: "call `_on_role_like`
first, check the bool, call `_default` if false." That's
more verbose than today's `Config::set_temperature` which
hides the dispatch.

Step 8 should add convenience helpers on `RequestContext`
that wrap both halves:

```rust
pub fn set_temperature(&mut self, value: Option<f64>, app: &mut AppConfig) {
    if !self.set_temperature_on_role_like(value) {
        app.set_temperature_default(value);
    }
}
```

But that requires `&mut AppConfig`, which requires unwrapping
the `Arc` on `AppState.config`. The cleanest shape is probably
to move the mutation into a helper on `AppState`:

```rust
impl AppState {
    pub fn config_mut(&self) -> Option<&mut AppConfig> {
        Arc::get_mut(...)
    }
}
```

Or accept that the `.set` REPL command needs an owned
`AppState` (not `Arc<AppState>`) and handle the mutation at
the entry point. Step 8 can decide.

### 4. `select_*` methods are long but verbatim

The 3 `select_*` methods are ~180 lines combined and are
verbatim copies of the `Config` originals. I resisted the
urge to refactor (extract helpers, simplify the
`enabled_tools == "all"` branches, etc.) because:

- Step 7 is about splitting signatures, not style
- The copies get deleted in Step 10 anyway
- Any refactor could introduce subtle behavior differences
  that are hard to catch without a functional test for these
  specific methods

Post–Phase 1 cleanup can factor these if desired.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 6 notes: `docs/implementation/PHASE-1-STEP-6-NOTES.md`
- Step 6.5 notes: `docs/implementation/PHASE-1-STEP-6.5-NOTES.md`
- Modified files:
  - `src/config/app_config.rs` (6 new `set_*_default` methods)
  - `src/config/request_context.rs` (14 new mixed methods,
    7 new imports)
  - `src/config/mod.rs` (`format_option_value` → `pub(super)`)
