# Phase 1 Step 16d — Implementation Notes

## Status

Done.

## Plan reference

- Parent plan: `docs/implementation/PHASE-1-STEP-16-NOTES.md`
- Sub-phase goal: "Build `AppState::init(app_config, ...).await`
  absorbing MCP registry startup and global functions loading"

## Summary

Added `AppState::init()` async constructor that self-initializes all
process-wide shared state from an `Arc<AppConfig>` and startup
context. Two new fields on `AppState`: `mcp_registry` (holds initial
MCP server Arcs alive) and `functions` (the global base
`Functions`). Changed `McpRegistry::init` to take `&AppConfig +
&Vault` instead of `&Config`.

The constructor is dead-code-gated (`#[allow(dead_code)]`) until
Step 16e switches `main.rs` over to call it. The bridge flow
continues to populate the new fields via Config's existing
`functions` and `mcp_registry` so nothing breaks.

## What was changed

### `src/mcp/mod.rs`

**`McpRegistry::init` signature change:**
```rust
// Before
pub async fn init(
    log_path: Option<PathBuf>,
    start_mcp_servers: bool,
    enabled_mcp_servers: Option<String>,
    abort_signal: AbortSignal,
    config: &Config,
) -> Result<Self>

// After
pub async fn init(
    log_path: Option<PathBuf>,
    start_mcp_servers: bool,
    enabled_mcp_servers: Option<String>,
    abort_signal: AbortSignal,
    app_config: &AppConfig,
    vault: &Vault,
) -> Result<Self>
```

The function reads two things from its config argument:
- `config.vault` for secret interpolation → now takes `&Vault`
  directly
- `config.mcp_server_support` for the start-servers gate → a serde
  field already present on `AppConfig`

Both dependencies are now explicit. No Config reference anywhere in
the MCP module.

**Imports updated:**
- Removed `use crate::config::Config`
- Added `use crate::config::AppConfig`
- Added `use crate::vault::Vault`

### `src/config/mod.rs`

**`Config::load_mcp_servers` updated** to build a temporary AppConfig
via `self.to_app_config()` and pass it plus `&self.vault` to
`McpRegistry::init`. This is a transitional bridge — disappears in
16e when `main.rs` stops calling `Config::init` and
`AppState::init` becomes the sole entry point.

### `src/config/app_state.rs`

**New fields:**
```rust
pub mcp_registry: Option<Arc<McpRegistry>>,
pub functions: Functions,
```

**New `AppState::init()` async constructor** absorbs:
- `Vault::init(&config)` (replaces the old
  `AppState { vault: cfg.vault.clone() }` pattern)
- `McpRegistry::init(...)` (previously inside `Config::init`)
- Registers initial MCP servers with `McpFactory` via
  `insert_active(McpServerKey, &handle)` — this is NEW behavior, see
  below.
- `Functions::init(config.visible_tools)` (previously inside
  `Config::init`)
- `functions.append_mcp_meta_functions(...)` when MCP support is on
  and servers started
- Wraps `McpRegistry` in `Arc` to keep initial server handles alive
  across scope transitions (see below)

**Imports expanded:**
- `McpServerKey` from `super::mcp_factory`
- `Functions` from `crate::function`
- `McpRegistry` from `crate::mcp`
- `AbortSignal` from `crate::utils`
- `Vault` from `crate::vault`
- `anyhow::Result`

### `src/main.rs`

**AppState struct literal extended** to populate the two new fields
from `cfg.mcp_registry` and `cfg.functions`. This keeps the bridge
flow working unchanged. When 16e replaces this struct literal with
`AppState::init(...)`, these field references go away entirely.

### `src/function/supervisor.rs`

**Child AppState construction extended** to propagate the new
fields from parent: `mcp_registry: ctx.app.mcp_registry.clone()` and
`functions: ctx.app.functions.clone()`. This maintains parent-child
sharing of the MCP factory cache (which was already fixed earlier in
this work stream).

### `src/config/request_context.rs` and `src/config/session.rs`

**Test helper AppState construction extended** to include the two
new fields with safe defaults (`None`, and `cfg.functions.clone()`
respectively).

## New behavior: McpFactory pre-registration

`AppState::init` registers every initial server with `McpFactory` via
`insert_active`. This fixes a latent issue in the current bridge
flow:

- Before: initial servers were held on `Config.mcp_registry.servers`;
  when the first scope transition (e.g., `.role coder`) ran
  `rebuild_tool_scope`, it called `McpFactory::acquire(name, spec,
  log_path)` which saw an empty cache and **spawned duplicate
  servers**. The original servers died when the initial ToolScope
  was replaced.
- After (via `AppState::init`): the factory's Weak map is seeded
  with the initial server Arcs. The registry itself is wrapped in
  Arc and held on AppState so the Arcs stay alive. Scope
  transitions now hit the factory cache and reuse the same
  subprocesses.

This is a real improvement that shows up once `main.rs` switches to
`AppState::init` in 16e. During the 16d bridge window, nothing
reads the factory pre-registration yet.

## Behavior parity (16d window)

- `main.rs` still calls `Config::init`, still uses the bridge; all
  new fields populated from Config's own state.
- `AppState::init` is present but unused in production code paths.
- Test helpers still use struct literals; they pass `None` for
  `mcp_registry` and clone `cfg.functions` which is the same as
  what the bridge was doing.
- No observable runtime change for users.

## Files modified

- `src/mcp/mod.rs` — `McpRegistry::init` signature; imports.
- `src/config/mod.rs` — `Config::load_mcp_servers` bridges to new
  signature.
- `src/config/app_state.rs` — added 2 fields, added `init`
  constructor, expanded imports.
- `src/main.rs` — struct literal populates 2 new fields.
- `src/function/supervisor.rs` — child struct literal populates 2
  new fields.
- `src/config/request_context.rs` — test helper populates 2 new
  fields.
- `src/config/session.rs` — test helper populates 2 new fields.

## Assumptions made

1. **`McpFactory::insert_active` with Weak is sufficient to seed the
   cache.** The initial ServerArcs live on `AppState.mcp_registry`
   (wrapped in Arc to enable clone across child states). Scope
   transitions call `McpFactory::acquire` which does
   `try_get_active(key).unwrap_or_else(spawn_new)`. The Weak in
   factory upgrades because Arc is alive in `mcp_registry`. Verified
   by reading code paths; not yet verified at runtime since bridge
   still drives today's flow.

2. **`functions: Functions` is Clone-safe.** The struct contains
   `Vec<FunctionDeclaration>` and related fields; cloning is cheap
   enough at startup and child-agent spawn. Inspected the
   definition; no references to check.

3. **`mcp_server_support` gate still applies.** `Config::init` used
   to gate the MCP meta function append with both `is_empty()` and
   `mcp_server_support`; `AppState::init` preserves both checks.
   Parity confirmed.

4. **Mode-specific function additions (REPL-only
   `append_user_interaction_functions`) do NOT live on AppState.**
   They are added per-scope in `rebuild_tool_scope` and in the
   initial `RequestContext::new` path. `AppState.functions` is the
   mode-agnostic base. This matches the long-term design
   (RequestContext owns mode-aware additions).

5. **`mcp_registry: Option<Arc<McpRegistry>>` vs `McpRegistry`.**
   Went with `Option<Arc<_>>` so:
   - `None` when no MCP config exists (can skip work)
   - `Arc` so parent/child AppStates share the same registry
     (keeping initial server handles alive across the tree)

## Open questions

1. **Registry on AppState vs factory-owned lifecycle**. The factory
   holds Weak; the registry holds Arc. Keeping the registry alive
   on AppState extends server lifetime to the process lifetime.
   This differs from the current "servers die on scope transition"
   behavior. In practice this is what users expect — start the
   servers once, keep them alive. But it means long-running REPL
   sessions retain all server subprocesses even if the user switches
   away from them. Acceptable trade-off for Phase 1.

2. **Should `AppState::init` return `Arc<AppState>` directly?**
   Currently returns `Self`. Caller wraps in Arc. Symmetric with
   other init functions; caller has full flexibility. Keep as-is.

3. **Unit tests for `AppState::init`.** Didn't add any because the
   function is heavily async, touches filesystem (paths),
   subprocess startup (MCP), and the vault. A meaningful unit test
   would require mocking. Integration-level validation happens in
   16e when main.rs switches over. Deferred.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean, zero warnings
- `cargo test` — 122 passing, zero failures
- New `AppState::init` gated with `#[allow(dead_code)]` — no
  warnings for being unused

## Remaining work for Step 16

- **16e**: Switch `main.rs` to call
  `AppState::init(app_config, log_path, start_mcp_servers,
  abort_signal).await?` instead of the bridge pattern. Audit the
  15 `Config::init()` callers. Remove `#[allow(dead_code)]` from
  `AppConfig::from_config`, `AppConfig::resolve_model`, and
  `AppState::init`.
- **16f**: Delete `Config.vault`, `Config.functions`,
  `Config.mcp_registry`, all other `#[serde(skip)]` runtime fields.
  Delete `Config::init`, `Config::load_envs`, `Config::load_functions`,
  `Config::load_mcp_servers`, `Config::setup_model`,
  `Config::set_model`, etc. Delete `bridge.rs`.

## Migration direction preserved

Before 16d:
```
AppState {
    config, vault, mcp_factory, rag_cache, mcp_config, mcp_log_path
}
```
Constructed only via struct literal from Config fields via bridge.

After 16d:
```
AppState {
    config, vault, mcp_factory, rag_cache, mcp_config, mcp_log_path,
    mcp_registry, functions
}
impl AppState {
    pub async fn init(config, log_path, start_mcp_servers, abort_signal)
        -> Result<Self>
}
```
New fields present on all code paths. New self-initializing
constructor ready for 16e's switchover.
