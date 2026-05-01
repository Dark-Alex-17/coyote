# Phase 1 Step 3 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 3: Migrate global-read methods to AppConfig"

## Summary

Added 7 global-read methods to `AppConfig` as inherent methods
duplicating the bodies that still exist on `Config`. The planned
approach (deprecated forwarders + caller migration) turned out to
be the wrong shape for this step because callers hold `Config`
instances, not `AppConfig` instances, and giving them an `AppConfig`
would require either a sync'd `Arc<AppConfig>` field on `Config`
(which Step 4's global-write migration would immediately break) or
cloning on every call. The clean answer is to duplicate during the
bridge window and let callers migrate naturally when Steps 8-9
switch them from `Config` to `RequestContext` + `AppState`. The
duplication is 7 methods / ~100 lines and deletes itself when
`Config` is removed in Step 10.

**Three methods from the plan's Step 3 target list were deferred
to Step 7** because they read runtime state, not just serialized
state (see "Deviations from plan").

## What was changed

### Modified files

- **`src/config/app_config.rs`** — added 6 new imports
  (`MarkdownRender`, `RenderOptions`, `IS_STDOUT_TERMINAL`,
  `decode_bin`, `anyhow`, `env`, `ThemeSet`) and a new
  `impl AppConfig` block with 7 methods under
  `#[allow(dead_code)]`:
  - `vault_password_file(&self) -> PathBuf`
  - `editor(&self) -> Result<String>`
  - `sync_models_url(&self) -> String`
  - `light_theme(&self) -> bool`
  - `render_options(&self) -> Result<RenderOptions>`
  - `print_markdown(&self, text) -> Result<()>`
  - `rag_template(&self, embeddings, sources, text) -> String`

  All bodies are copy-pasted verbatim from the originals on
  `Config`, with the following adjustments for the new module
  location:
  - `EDITOR` static → `super::EDITOR` (shared across both impls)
  - `SYNC_MODELS_URL` const → `super::SYNC_MODELS_URL`
  - `RAG_TEMPLATE` const → `super::RAG_TEMPLATE`
  - `LIGHT_THEME` / `DARK_THEME` consts → `super::LIGHT_THEME` /
    `super::DARK_THEME`
  - `paths::local_path()` continues to work unchanged (already in
    the right module from Step 2)

### Unchanged files

- **`src/config/mod.rs`** — the original `Config::vault_password_file`,
  `editor`, `sync_models_url`, `light_theme`, `render_options`,
  `print_markdown`, `rag_template` method definitions are
  deliberately left intact. They continue to work for every existing
  caller. The deletion of these happens in Step 10 when `Config` is
  removed entirely.
- **All external callers** (26 callsites across 6 files) — also
  unchanged. They continue to call `config.editor()`,
  `config.render_options()`, etc. on their `Config` instances.

## Key decisions

### 1. Duplicate method bodies instead of `#[deprecated]` forwarders

The plan prescribed the same shape as Step 2: add the new version,
add a `#[deprecated]` forwarder on the old location, migrate
callers, delete forwarders. This worked cleanly in Step 2 because
the new location was a free-standing `paths` module — callers
could switch from `Config::method()` (associated function) to
`paths::method()` (free function) without needing any instance.

Step 3 is fundamentally different: `AppConfig::method(&self)` needs
an `AppConfig` instance. Callers today hold `Config` instances.
Giving them an `AppConfig` means one of:

(a) Add an `app_config: Arc<AppConfig>` field to `Config` and have
    the forwarder do `self.app_config.method()`. **Rejected**
    because Step 4 (global-write) will mutate `Config` fields via
    `set_wrap`, `update`, etc. — keeping the `Arc<AppConfig>`
    in sync would require either rebuilding it on every write (slow
    and racy) or tracking dirty state (premature complexity).
(b) Have the forwarder do `self.to_app_config().method()`. **Rejected**
    because `to_app_config` clones all 40 serialized fields on
    every call — a >100x slowdown for simple accessors like
    `light_theme()`.
(c) Duplicate the method bodies on both `Config` and `AppConfig`,
    let each caller use whichever instance it has, delete the
    `Config` versions when `Config` itself is deleted in Step 10.
    **Chosen.**

Option (c) has a small ongoing cost (~100 lines of duplicated
logic) but is strictly additive, has zero runtime overhead, and
automatically cleans up in Step 10. It also matches how Rust's
type system prefers to handle this — parallel impls are cheaper
than synchronized state.

### 2. Caller migration is deferred to Steps 8-9

With duplication in place, the migration from `Config` to
`AppConfig` happens organically later:

- When Step 8 rewrites `main.rs` to construct an `AppState` and
  `RequestContext` instead of a `GlobalConfig`, the `main.rs`
  callers of `config.editor()` naturally become
  `ctx.app.config.editor()` — calling into `AppConfig`'s version.
- Same for every other callsite that gets migrated in Step 8+.
- By Step 10, the old `Config::editor()` etc. have zero callers
  and get deleted along with the rest of `Config`.

This means Step 3 is "additive only, no caller touches" —
deliberately smaller in scope than Step 2. That's the correct call
given the instance-type constraint.

### 3. `EDITOR` static is shared between `Config::editor` and `AppConfig::editor`

`editor()` caches the resolved editor path in a module-level
`static EDITOR: OnceLock<Option<String>>` in `src/config/mod.rs`.
Both `Config::editor(&self)` and `AppConfig::editor(&self)` read
and initialize the same static via `super::EDITOR`. This matches
the current behavior: whichever caller resolves first wins the
`OnceLock::get_or_init` race and subsequent callers see the cached
value.

There's a latent bug here (if `Config.editor` and `AppConfig.editor`
fields ever differ, the first caller wins regardless) but it's
pre-existing and preserved during the bridge window. Step 10 resolves
it by deleting `Config` entirely.

### 4. Three methods deferred to Step 7

See "Deviations from plan."

## Deviations from plan

### `select_functions`, `select_enabled_functions`, `select_enabled_mcp_servers` belong in Step 7

The plan's Step 3 table lists all three. Reading their bodies (in
`src/config/mod.rs` at lines 1816, 1828, 1923), they all touch
`self.functions` and `self.agent` — both of which are `#[serde(skip)]`
runtime fields that do NOT exist on `AppConfig` and will never
exist there (they're per-request state living on `RequestContext`
and `AgentRuntime`).

These are "mixed" methods in the plan's Step 7 taxonomy — they
conditionally read serialized config + runtime state depending on
whether an agent is active. Moving them to `AppConfig` now would
require `AppConfig` to hold `functions` and `agent` fields, which
directly contradicts the Step 0 / Step 6.5 design.

**Action taken:** left all three on `Config` unchanged. They get
migrated in Step 7 with the new signature
`(app: &AppConfig, ctx: &RequestContext, role: &Role) -> Vec<...>`
as described in the plan.

**Action required from Step 7:** pick up these three methods. The
call graph is:
- `Config::select_functions` is called from `src/config/input.rs:243`
  (one external caller)
- `Config::select_functions` internally calls the two private
  helpers
- The private helpers read both `self.functions` (runtime,
  per-request) and `self.agent` (runtime, per-request) — so they
  fundamentally need `RequestContext` not `AppConfig`

### Step 3 count: 7 methods, not 10

The plan's table listed 10 target methods. After excluding the
three `select_*` methods, Step 3 migrated 7. This is documented
here rather than silently completing a smaller Step 3 so Step 7's
scope is clear.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (same as Steps 1–2)

Step 3 added no new tests because it's duplication — there's
nothing new to verify. The existing test suite confirms:
(a) the original `Config` methods still work (they weren't touched)
(b) `AppConfig` still compiles and its `Default` impl is intact
    (needed for Step 1's bridge test which uses
    `build_populated_config()` → `to_app_config()`)

Running `cargo test bridge` specifically:

```
test config::bridge::tests::round_trip_default_config ... ok
test config::bridge::tests::to_app_config_copies_every_serialized_field ... ok
test config::bridge::tests::to_request_context_copies_every_runtime_field ... ok
test config::bridge::tests::round_trip_preserves_all_non_lossy_fields ... ok
test result: ok. 4 passed
```

The bridge's round-trip test still works, which proves the new
methods on `AppConfig` don't interfere with the struct layout or
deserialization. They're purely additive impl-level methods.

### Manual smoke test

Not applicable — no runtime behavior changed. CLI and REPL still
call `Config::editor()` etc. as before.

## Handoff to next step

### What Step 4 can rely on

Step 4 (migrate global-write methods) can rely on:

- `AppConfig` now has 7 inherent read methods that mirror the
  corresponding `Config` methods exactly
- `#[allow(dead_code)]` on the `impl AppConfig` block in
  `app_config.rs` — safe to leave as-is, it'll go away when the
  first caller is migrated in Step 8+
- `Config` is unchanged for all 7 methods and continues to work
  for every current caller
- The bridge (`Config::to_app_config`, `to_request_context`,
  `from_parts`) from Step 1 still works
- The `paths` module from Step 2 is unchanged
- `Config::select_functions`, `select_enabled_functions`,
  `select_enabled_mcp_servers` are **still on `Config`** and must
  stay there through Step 6. They get migrated in Step 7.

### What Step 4 should watch for

- **The Step 4 target list** (from `PHASE-1-IMPLEMENTATION-PLAN.md`):
  `set_wrap`, `update`, `load_envs`, `load_functions`,
  `load_mcp_servers`, `setup_model`, `setup_document_loaders`,
  `setup_user_agent`. These are global-write methods that
  initialize or mutate serialized fields.
- **Tension with Step 3's duplication decision:** Step 4 methods
  mutate `Config` fields. If we also duplicate them on `AppConfig`,
  then mutations through one path don't affect the other — but no
  caller ever mutates both, so this is fine in practice during
  the bridge window.
- **`load_functions` and `load_mcp_servers`** are initialization-
  only (called once in `Config::init`). They're arguably not
  "global-write" in the same sense — they populate runtime-only
  fields (`functions`, `mcp_registry`). Step 4 should carefully
  classify each: fields that belong to `AppConfig` vs fields that
  belong to `RequestContext` vs fields that go away in Step 6.5
  (`mcp_registry`).
- **Strategy for Step 4:** because writes are typically one-shot
  (`update` is called from `.set` REPL command; `load_envs` is
  called once at startup), you can be more lenient about
  duplication vs consolidation. Consider: the write methods might
  not need to exist on `AppConfig` at all if they're only used
  during `Config::init` and never during request handling. Step 4
  should evaluate each one individually.

### What Step 4 should NOT do

- Don't add an `app_config: Arc<AppConfig>` field to `Config`
  (see Key Decision #1 for why).
- Don't touch the 7 methods added to `AppConfig` in Step 3 — they
  stay until Step 8+ caller migration, and Step 10 deletion.
- Don't migrate `select_*` methods — those are Step 7.
- Don't try to migrate callers of the Step 3 methods to go
  through `AppConfig` yet. The call sites still hold `Config`,
  and forcing a conversion would require either a clone or a
  sync'd field.

### Files to re-read at the start of Step 4

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 4 section
- This notes file — specifically the "Deviations from plan" and
  "What Step 4 should watch for" sections
- `src/config/mod.rs` — the current `Config::set_wrap`, `update`,
  `load_*`, `setup_*` method bodies (search for `pub fn set_wrap`,
  `pub fn update`, `pub fn load_envs`, etc.)
- `src/config/app_config.rs` — the current shape with 7 new
  methods

## Follow-up (not blocking Step 4)

### 1. The `EDITOR` static sharing is pre-existing fragility

Both `Config::editor` and `AppConfig::editor` now share the same
`static EDITOR: OnceLock<Option<String>>`. If two Configs with
different `editor` fields exist (unlikely in practice but possible
during tests), the first caller wins. This isn't new — the single
`Config` version had the same property. Step 10's `Config`
deletion will leave only `AppConfig::editor` which eliminates the
theoretical bug. Worth noting so nobody introduces a test that
assumes per-instance editor caching.

### 2. `impl AppConfig` block grows across Steps 3-7

By the end of Step 7, `AppConfig` will have accumulated: 7 methods
from Step 3, potentially some from Step 4, more from Step 7's
mixed-method splits. The `#[allow(dead_code)]` currently covers
the whole block. As callers migrate in Step 8+, the warning
suppression can be removed. Don't narrow it prematurely during
Steps 4-7.

### 3. Imports added to `app_config.rs`

Step 3 added `MarkdownRender`, `RenderOptions`, `IS_STDOUT_TERMINAL`,
`decode_bin`, `anyhow::{Context, Result, anyhow}`, `env`,
`ThemeSet`. Future steps may add more. The import list is small
enough to stay clean; no reorganization needed.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 2 notes: `docs/implementation/PHASE-1-STEP-2-NOTES.md`
- Modified file: `src/config/app_config.rs` (imports + new
  `impl AppConfig` block)
- Unchanged but relevant: `src/config/mod.rs` (original `Config`
  methods still exist for now), `src/config/bridge.rs` (still
  passes round-trip tests)
