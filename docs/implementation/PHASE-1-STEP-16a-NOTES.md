# Phase 1 Step 16a — Implementation Notes

## Status

Done.

## Plan reference

- Parent plan: `docs/implementation/PHASE-1-STEP-16-NOTES.md`
- Sub-phase goal: "Build `AppConfig::from_config` absorbing
  env/wrap/docs/user-agent/model-resolution logic"

## Summary

Introduced `AppConfig::from_config(config: Config) -> Result<AppConfig>`
as the canonical constructor for a fully-initialized `AppConfig`. The
new constructor chains the four mutation methods (`load_envs`,
`set_wrap`, `setup_document_loaders`, `setup_user_agent`) plus a new
`resolve_model()` method that picks a default model when `model_id`
is empty.

The existing bridge (`Config::init` + `Config::to_app_config`) is
untouched. `from_config` is currently dead code (gated with
`#[allow(dead_code)]`) — it becomes the entry point in Step 16e when
`main.rs` switches over. The methods it calls (`load_envs`, etc.) are
no longer dead code because they're reachable via `from_config`, so
their `#[allow(dead_code)]` gates were removed.

## What was changed

### `src/config/app_config.rs`

**Added:**
- `AppConfig::from_config(config) -> Result<Self>` — canonical
  constructor that copies serde fields, applies env overrides,
  validates wrap, installs doc loaders, resolves user agent, and
  ensures a default model.
- `AppConfig::resolve_model(&mut self) -> Result<()>` — if
  `model_id` is empty, picks the first available chat model. Errors
  if no models are available. Replaces the logic from
  `Config::setup_model` that belongs on `AppConfig` (the
  `Model`-resolution half of `Config::setup_model` stays in Config
  for now — that moves in 16e).
- 8 unit tests covering field copying, doc loader insertion, user
  agent resolution, wrap validation (valid + invalid), and
  `resolve_model` error/happy paths.

**Removed `#[allow(dead_code)]` from:**
- `set_wrap`
- `setup_document_loaders`
- `setup_user_agent`
- `load_envs`

These are now reachable via `from_config`. They remain `pub` because
REPL-mode mutations (via `.set wrap <value>` or similar) will go
through them once `RequestContext` stops mutating `Config`.

**Removed entirely:**
- `AppConfig::ensure_default_model_id` — redundant with the new
  `resolve_model`. Had no callers outside itself (confirmed via
  grep).

### Behavior parity notes

1. **`from_config` is non-destructive:** it consumes `Config` by
   value (not `&Config`) since post-bridge, Config is no longer
   needed. This matches the long-term design.

2. **`from_config` vs `to_app_config` + mutations:** The methods
   called inside `from_config` are identical bodies to the ones
   currently called on `Config` inside `Config::init`. Env var
   reads, wrap validation, doc loader defaults, and user agent
   resolution all produce the same state.

3. **`resolve_model` vs `Config::setup_model`:**
   - `Config::setup_model` does TWO things:
     (a) ensure `model_id` is non-empty (pick default if empty)
     (b) resolve the `Model` struct via `Model::retrieve_model` and
         store it in `self.model`
   - `AppConfig::resolve_model` only does (a).
   - (b) happens today in `cfg.set_model(&model_id)` inside
     `Config::setup_model`. In the new architecture, the `Model`
     struct lives on `RequestContext.model`, and
     `Model::retrieve_model(&app_config, &app_config.model_id, ...)`
     will be called inside `RequestContext::new` (or equivalent)
     once the bridge is removed in 16e.

## Files modified

- `src/config/app_config.rs` — 2 new methods, 4
  `#[allow(dead_code)]` gates removed, 1 method deleted, 8 new
  tests.

## Files NOT modified

- `src/config/mod.rs` — `Config::init` still runs all mutations on
  Config; bridge still copies to AppConfig. Unchanged in 16a.
- `src/config/bridge.rs` — Untouched. Used by `from_config`
  internally (`config.to_app_config()`).
- `src/main.rs` — Still uses the bridge flow. Switch happens in 16e.

## Assumptions made

1. **`from_config` consumes `Config` by value** (not `&Config`)
   — aligns with the long-term design where `Config` is discarded
   after conversion. No current caller would benefit from keeping
   the Config around after conversion.

2. **`resolve_model` narrow scope**: only responsible for ensuring
   `model_id` is non-empty. Does NOT resolve a `Model` struct —
   that's RequestContext's job. This matches the split between
   `AppConfig` (the configuration) and `RequestContext` (the
   resolved runtime handle).

3. **`#[allow(dead_code)]` on `from_config` and `resolve_model`**:
   they're unused until 16e. The gate is explicit so grep-hunts can
   find them when 16e switches over.

4. **User agent prefix in tests**: I assumed the user agent prefix
   is not critical to test literally (it depends on the crate name).
   The test checks for a non-"auto" value containing `/` rather
   than matching `loki-ai/`. Safer against crate rename.

## Open questions (parked for later sub-phases)

1. **Should `from_config` also run secret interpolation?** Currently
   `Config::init` does a two-pass YAML parse where the raw content
   gets secrets injected from the vault, then the Config is
   re-parsed. In the new architecture this belongs in `main.rs` or
   a separate helper (the Config comes in already-interpolated).
   Not a 16a concern.

2. **Test naming convention**: Existing tests use
   `fn test_name_returns_value_when_condition`. New tests use
   `fn from_config_does_thing`. Both styles present in the file;
   kept consistent with new code.

3. **`ensure_default_model_id` deletion**: confirmed via grep that
   it had no callers outside itself. Deleted cleanly. If a future
   sub-phase needs the Option<String> return variant, it can be
   re-added.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean, zero warnings
- `cargo test` — 122 passing (114 pre-16a + 8 new), zero failures

## Remaining work for Step 16

- **16b**: Extract `install_builtins()` as top-level free function
- **16c**: Migrate `Vault::init(&Config)` → `Vault::init(&AppConfig)`
- **16d**: Build `AppState::init(app_config, ...).await`
- **16e**: Switch `main.rs` and all 15 `Config::init()` callers to
  the new flow
- **16f**: Delete Config runtime fields, bridge.rs, `Config::init`,
  duplicated methods

## Migration direction preserved

After 16a, no runtime behavior has changed. The new entry point
exists but isn't wired in. The bridge flow continues as before:

```
YAML → Config::load_from_file
    → Config::init (unchanged, does all current mutations)
        - load_envs, set_wrap, setup_document_loaders, ...
        - setup_model, load_functions, load_mcp_servers
    → cfg.to_app_config() → AppConfig (via bridge)
    → cfg.to_request_context(AppState) → RequestContext
```

New entry point ready for 16e:

```
AppConfig::from_config(config) → AppConfig
    (internally: to_app_config, load_envs, set_wrap,
     setup_document_loaders, setup_user_agent, resolve_model)
```
