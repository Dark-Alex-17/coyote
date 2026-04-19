# Phase 1 Step 4 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 4: Migrate global-write methods"

## Summary

Added 4 of 8 planned global-write methods to `AppConfig` as
inherent methods, duplicating the bodies that still exist on
`Config`. The other 4 methods were deferred: 2 to Step 7 (mixed
methods that call into `set_*` methods slated for Step 7), and
2 kept on `Config` because they populate runtime-only fields
(`functions`, `mcp_registry`) that don't belong on `AppConfig`.

Same duplication-no-caller-migration pattern as Step 3 — during
the bridge window both `Config` and `AppConfig` have these
methods; caller migration happens organically in Steps 8-9 when
frontends switch from `GlobalConfig` to `AppState` + `RequestContext`.

## What was changed

### Modified files

- **`src/config/app_config.rs`** — added 4 new imports (`NO_COLOR`,
  `get_env_name` via `crate::utils`, `terminal_colorsaurus`
  types) and a new `impl AppConfig` block with 4 methods under
  `#[allow(dead_code)]`:
  - `set_wrap(&mut self, value: &str) -> Result<()>` — parses and
    sets `self.wrap` for the `.set wrap` REPL command
  - `setup_document_loaders(&mut self)` — seeds default PDF/DOCX
    loaders into `self.document_loaders` if not already present
  - `setup_user_agent(&mut self)` — expands `"auto"` into
    `loki/<version>` in `self.user_agent`
  - `load_envs(&mut self)` — ~140 lines of env-var overrides that
    populate all 30+ serialized fields from `LOKI_*` environment
    variables

  All bodies are copy-pasted verbatim from the originals on
  `Config`, with references updated for the new module location:
  - `read_env_value::<T>` → `super::read_env_value::<T>`
  - `read_env_bool` → `super::read_env_bool`
  - `NO_COLOR`, `IS_STDOUT_TERMINAL`, `get_env_name`, `decode_bin`
    → imported from `crate::utils`
  - `terminal_colorsaurus` → direct import

### Unchanged files

- **`src/config/mod.rs`** — the original `Config::set_wrap`,
  `load_envs`, `setup_document_loaders`, `setup_user_agent`
  definitions are deliberately left intact. They continue to
  work for every existing caller. They get deleted in Step 10
  when `Config` is removed entirely.
- **`src/config/mod.rs`** — the `read_env_value` and
  `read_env_bool` private helpers are unchanged and accessed via
  `super::read_env_value` from `app_config.rs`.

## Key decisions

### 1. Only 4 of 8 methods migrated

The plan's Step 4 table listed 8 methods. After reading each one
carefully, I classified them:

| Method | Classification | Action |
|---|---|---|
| `set_wrap` | Pure global-write | **Migrated** |
| `load_envs` | Pure global-write | **Migrated** |
| `setup_document_loaders` | Pure global-write | **Migrated** |
| `setup_user_agent` | Pure global-write | **Migrated** |
| `setup_model` | Calls `self.set_model()` (Step 7 mixed) | **Deferred to Step 7** |
| `load_functions` | Writes runtime `self.functions` field | **Not migrated** (stays on `Config`) |
| `load_mcp_servers` | Writes runtime `self.mcp_registry` field (going away in Step 6.5) | **Not migrated** (stays on `Config`) |
| `update` | Dispatches to 10+ `set_*` methods, all Step 7 mixed | **Deferred to Step 7** |

See "Deviations from plan" for detail on each deferral.

### 2. Same duplication-no-forwarder pattern as Step 3

Step 4's target callers are all `.write()` on a `GlobalConfig` /
`Config` instance. Like Step 3, giving these callers an
`AppConfig` instance would require either (a) a sync'd
`Arc<AppConfig>` field on `Config` (breaks because Step 4
itself mutates `Config`), (b) cloning on every call (expensive
for `load_envs` which touches 30+ fields), or (c) duplicating
the method bodies.

Option (c) is the same choice Step 3 made and for the same
reasons. The duplication is 4 methods (~180 lines total dominated
by `load_envs`) that auto-delete in Step 10.

### 3. `load_envs` body copied verbatim despite being long

`load_envs` is ~140 lines of repetitive `if let Some(v) =
read_env_value(...) { self.X = v; }` blocks — one per serialized
field. I considered refactoring it to reduce repetition (e.g., a
macro or a data-driven table) but resisted that urge because:

- The refactor would be a behavior change (even if subtle) during
  a mechanical code-move step
- The verbatim copy is easy to audit for correctness (line-by-line
  diff against the original)
- It gets deleted in Step 10 anyway, so the repetition is
  temporary
- Any cleanup belongs in a dedicated tidying pass after Phase 1,
  not in the middle of a split

### 4. Methods stay in a separate `impl AppConfig` block

Step 3 added its 7 read methods in one `impl AppConfig` block.
Step 4 adds its 4 write methods in a second `impl AppConfig`
block directly below it. Rust allows multiple `impl` blocks on
the same type, and the visual separation makes it obvious which
methods are reads vs writes during the bridge window. When Step
10 deletes `Config`, both blocks can be merged or left separate
based on the cleanup maintainer's preference.

## Deviations from plan

### `setup_model` deferred to Step 7

The plan lists `setup_model` as a Step 4 target. Reading its
body:

```rust
fn setup_model(&mut self) -> Result<()> {
    let mut model_id = self.model_id.clone();
    if model_id.is_empty() {
        let models = list_models(self, ModelType::Chat);
        // ...
    }
    self.set_model(&model_id)?;  // ← this is Step 7 "mixed"
    self.model_id = model_id;
    Ok(())
}
```

It calls `self.set_model(&model_id)`, which the plan explicitly
lists in **Step 7** ("mixed methods") because `set_model`
conditionally writes to `role_like` (runtime) or `model_id`
(serialized) depending on whether a role/session/agent is
active. Since `setup_model` can't be migrated until `set_model`
exists on `AppConfig` / `RequestContext`, it has to wait for
Step 7.

**Action:** left `Config::setup_model` intact. Step 7 picks it up.

### `update` deferred to Step 7

The plan lists `update` as a Step 4 target. Its body is a ~140
line dispatch over keys like `"temperature"`, `"top_p"`,
`"enabled_tools"`, `"enabled_mcp_servers"`, `"max_output_tokens"`,
`"save_session"`, `"compression_threshold"`,
`"rag_reranker_model"`, `"rag_top_k"`, etc. — every branch
calls into a `set_*` method on `Config` that the plan explicitly
lists in **Step 7**:

- `set_temperature` (Step 7)
- `set_top_p` (Step 7)
- `set_enabled_tools` (Step 7)
- `set_enabled_mcp_servers` (Step 7)
- `set_max_output_tokens` (Step 7)
- `set_save_session` (Step 7)
- `set_compression_threshold` (Step 7)
- `set_rag_reranker_model` (Step 7)
- `set_rag_top_k` (Step 7)

Migrating `update` before those would mean `update` calls
`Config::set_X` (old) from inside `AppConfig::update` (new) —
which crosses the type boundary awkwardly and leaves `update`'s
behavior split between the two types during the migration
window. Not worth it.

**Action:** left `Config::update` intact. Step 7 picks it up
along with the `set_*` methods it dispatches to. At that point
all 10 dependencies will be on `AppConfig`/`RequestContext` and
`update` can be moved cleanly.

### `load_functions` not migrated (stays on Config)

The plan lists `load_functions` as a Step 4 target. Its body:

```rust
fn load_functions(&mut self) -> Result<()> {
    self.functions = Functions::init(
        self.visible_tools.as_ref().unwrap_or(&Vec::new())
    )?;
    if self.working_mode.is_repl() {
        self.functions.append_user_interaction_functions();
    }
    Ok(())
}
```

It writes to `self.functions` — a `#[serde(skip)]` runtime field
that lives on `RequestContext` after Step 6 and inside `ToolScope`
after Step 6.5. It also reads `self.working_mode`, another
runtime field. This isn't a "global-write" method in the sense
Step 4 targets — it's a runtime initialization method that will
move to `RequestContext` when `functions` does.

**Action:** left `Config::load_functions` intact. It gets
handled in Step 5 or Step 6 when runtime fields start moving.
Not Step 4, not Step 7.

### `load_mcp_servers` not migrated (stays on Config)

Same story as `load_functions`. Its body writes
`self.mcp_registry` (a field slated for deletion in Step 6.5 per
the architecture plan) and `self.functions` (runtime, moving in
Step 5/6). Nothing about this method belongs on `AppConfig`.

**Action:** left `Config::load_mcp_servers` intact. It gets
handled or deleted in Step 6.5 when `McpFactory` replaces the
singleton registry entirely.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from Steps 1–3)

Step 4 added no new tests because it's duplication. The existing
test suite confirms:
- The original `Config` methods still work (they weren't touched)
- `AppConfig` still compiles, its `Default` impl is intact
- The bridge's round-trip test still passes:
  - `config::bridge::tests::round_trip_default_config`
  - `config::bridge::tests::round_trip_preserves_all_non_lossy_fields`
  - `config::bridge::tests::to_app_config_copies_every_serialized_field`
  - `config::bridge::tests::to_request_context_copies_every_runtime_field`

### Manual smoke test

Not applicable — no runtime behavior changed. CLI and REPL still
call `Config::set_wrap()`, `Config::update()`, `Config::load_envs()`,
etc. unchanged.

## Handoff to next step

### What Step 5 can rely on

Step 5 (migrate request-read methods to `RequestContext`) can
rely on:

- `AppConfig` now has **11 methods total**: 7 reads from Step 3,
  4 writes from Step 4
- `#[allow(dead_code)]` on both `impl AppConfig` blocks — safe
  to leave as-is, goes away when callers migrate in Steps 8+
- `Config` is unchanged for all 11 methods — originals still
  work for all current callers
- The bridge from Step 1, the paths module from Step 2, the
  read methods from Step 3 are all unchanged and still working
- **`setup_model`, `update`, `load_functions`, `load_mcp_servers`
  are still on `Config`** and must stay there:
  - `setup_model` → migrates in Step 7 with the `set_*` methods
  - `update` → migrates in Step 7 with the `set_*` methods
  - `load_functions` → migrates to `RequestContext` in Step 5 or
    Step 6 (whichever handles `Functions`)
  - `load_mcp_servers` → deleted/transformed in Step 6.5

### What Step 5 should watch for

- **Step 5 targets are `&self` request-read methods** that read
  runtime fields like `self.session`, `self.role`, `self.agent`,
  `self.rag`, etc. The plan's Step 5 table lists:
  `state`, `messages_file`, `sessions_dir`, `session_file`,
  `rag_file`, `info`, `role_info`, `session_info`, `agent_info`,
  `agent_banner`, `rag_info`, `list_sessions`,
  `list_autoname_sessions`, `is_compressing_session`,
  `role_like_mut`.
- **These migrate to `RequestContext`**, not `AppConfig`, because
  they read per-request state.
- **Same duplication pattern applies.** Add methods to
  `RequestContext`, leave originals on `Config`, no caller
  migration.
- **`sessions_dir` and `messages_file` already use `paths::`
  functions internally** (from Step 2's migration). They read
  `self.agent` to decide between the global and agent-scoped
  path. Those paths come from the `paths` module.
- **`role_like_mut`** is interesting — it's the helper that
  returns a mutable reference to whichever of role/session/agent
  is on top. It's the foundation for every `set_*` method in
  Step 7. Migrate it to `RequestContext` in Step 5 so Step 7
  has it ready.
- **`list_sessions` and `list_autoname_sessions`** wrap
  `paths::list_file_names` with some filtering. They take
  `&self` to know the current agent context for path resolution.

### What Step 5 should NOT do

- Don't touch the Step 3/4 methods on `AppConfig` — they stay
  until Steps 8+ caller migration.
- Don't try to migrate `update`, `setup_model`, `load_functions`,
  or `load_mcp_servers` — each has a specific later-step home.
- Don't touch the `bridge.rs` conversions — still needed.
- Don't touch `paths.rs` — still complete.
- Don't migrate any caller of any method yet — callers stay on
  `Config` through the bridge window.

### Files to re-read at the start of Step 5

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 5 section has
  the full request-read method table
- This notes file — specifically "Deviations from plan" and
  "What Step 5 should watch for"
- `src/config/request_context.rs` — to see the current shape
  that Step 5 will extend
- Current `Config` method bodies in `src/config/mod.rs` for
  each Step 5 target (search for `pub fn state`, `pub fn
  messages_file`, etc.)

## Follow-up (not blocking Step 5)

### 1. `load_envs` is the biggest duplication so far

At ~140 lines, `load_envs` is the largest single duplication in
the bridge. It's acceptable because it's self-contained and
auto-deletes in Step 10, but it's worth flagging that if Phase 1
stalls anywhere between now and Step 10, this method's duplication
becomes a maintenance burden. Env var changes would need to be
made twice.

**Mitigation during the bridge window:** if someone adds a new
env var during Steps 5-9, they MUST add it to both
`Config::load_envs` and `AppConfig::load_envs`. Document this in
the Step 5 notes if any env var changes ship during that
interval.

### 2. `AppConfig` now has 11 methods across 2 `impl` blocks

Fine during Phase 1. Post-Phase 1 cleanup can consider whether to
merge them or keep the read/write split. Not a blocker.

### 3. The `read_env_value` / `read_env_bool` helpers are accessed via `super::`

These are private module helpers in `src/config/mod.rs`. Step 4's
migration means `app_config.rs` now calls them via `super::`,
which works because `app_config.rs` is a sibling module. If
Phase 2+ work moves these helpers anywhere else, the `super::`
references in `app_config.rs` will need updating.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 3 notes: `docs/implementation/PHASE-1-STEP-3-NOTES.md`
  (for the duplication rationale)
- Modified file: `src/config/app_config.rs` (new imports + new
  `impl AppConfig` block with 4 write methods)
- Unchanged but referenced: `src/config/mod.rs` (original
  `Config` methods still exist, private helpers
  `read_env_value` / `read_env_bool` accessed via `super::`)
