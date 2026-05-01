# Phase 1 Step 16f — Implementation Notes

## Status

Done. Phase 1 Step 16 (Config → AppConfig migration) complete.

## Plan reference

- Parent plan: `docs/implementation/PHASE-1-STEP-16-NOTES.md`
- Predecessor: `docs/implementation/PHASE-1-STEP-16e-NOTES.md`
- Sub-phase goal: "Delete all #[allow(dead_code)] scaffolding from
  Config and bridge.rs, delete runtime fields from Config, delete
  bridge.rs entirely."

## Summary

`Config` is now a pure serde POJO. `bridge.rs` is gone. Every
runtime field, every `Config::init*` flavor, and every Config method
that was scaffolding for the old god-init has been deleted. The
project compiles clean, clippy clean, and all 122 tests pass.

## What was changed

### Deleted: `src/config/bridge.rs`

Whole file removed. `mod bridge;` declaration in `config/mod.rs`
removed. The two methods (`Config::to_app_config` and
`Config::to_request_context`) had no remaining callers after 16e.

### `src/config/mod.rs` — Config slimmed to a POJO

**Deleted runtime (`#[serde(skip)]`) fields from `Config`:**
- `vault`, `macro_flag`, `info_flag`, `agent_variables`
- `model`, `functions`, `mcp_registry`, `working_mode`,
  `last_message`
- `role`, `session`, `rag`, `agent`, `tool_call_tracker`
- `supervisor`, `parent_supervisor`, `self_agent_id`,
  `current_depth`, `inbox`, `root_escalation_queue`

**Deleted methods on `Config`:**
- `init`, `init_bare` (god-init replaced by
  `load_with_interpolation` + `AppConfig::from_config` +
  `AppState::init` + `RequestContext::bootstrap`)
- `sessions_dir`, `list_sessions` (replaced by
  `config::default_sessions_dir` / `config::list_sessions` free
  functions for use without a Config; per-context paths live on
  `RequestContext::sessions_dir` / `RequestContext::list_sessions`)
- `role_like_mut` (lives on `RequestContext` post-migration)
- `set_wrap`, `setup_document_loaders`, `setup_user_agent`,
  `load_envs` (lives on `AppConfig` post-migration)
- `set_model`, `setup_model` (model resolution now in
  `AppConfig::resolve_model`; per-scope model selection lives on
  `RequestContext`)
- `load_functions`, `load_mcp_servers` (absorbed by
  `AppState::init`)

**Default impl entries** for the deleted runtime fields removed.

**Imports cleaned up:** removed unused `ToolCallTracker`,
`McpRegistry`, `Supervisor`, `EscalationQueue`, `Inbox`, `RwLock`,
`ColorScheme`, `QueryOptions`, `color_scheme`, `Handle`. Kept
`Model`, `ModelType`, `GlobalVault` because sibling modules
(`role.rs`, `input.rs`, `agent.rs`, `session.rs`) use
`use super::*;` and depend on those re-exports.

**Removed assertions** for the deleted runtime fields from
`config_defaults_match_expected` test.

### `src/config/mod.rs` — `load_with_interpolation` no longer touches AppConfig::to_app_config

Previously called `config.to_app_config()` to build a Vault for
secret interpolation. Now constructs a minimal `AppConfig` inline
with only `vault_password_file` populated, since that's all
`Vault::init` reads. Also removed the `config.vault = Arc::new(vault)`
assignment that was the last write to the deleted runtime field.

### `src/config/mod.rs` — `vault_password_file` made `pub(super)`

Previously private. Now `pub(super)` so `AppConfig::from_config` (a
sibling module under `config/`) can read it during the field-copy.

### `src/config/app_config.rs` — `AppConfig::from_config` self-contained

Previously delegated to `Config::to_app_config()` (lived on bridge)
for the field-copy. Now inlines the field-copy directly in
`from_config`, then runs `load_envs`, `set_wrap`,
`setup_document_loaders`, `setup_user_agent`, and `resolve_model`
as before.

**Removed `#[allow(dead_code)]` from `AppConfig.model_id`** — it's
read from `app.config.model_id` in `RequestContext::bootstrap` so
the lint exemption was stale.

**Test refactor:** the three `to_app_config_*` tests rewritten as
`from_config_*` tests using `AppConfig::from_config(cfg).unwrap()`.
A `ClientConfig::default()` and non-empty `model_id: "test-model"`
were added so `resolve_model()` doesn't bail with "No available
model" during the runtime initialization.

### `src/config/session.rs` — Test helper rewired

`session_new_from_ctx_captures_save_session` rewritten to build the
test `AppState` directly with `AppConfig::default()`,
`Vault::default()`, `Functions::default()` instead of going through
`cfg.to_app_config()` / `cfg.vault` / `cfg.functions`. Then uses
`RequestContext::new(app_state, WorkingMode::Cmd)` instead of the
deleted `cfg.to_request_context(app_state)`.

### `src/config/request_context.rs` — Test helpers rewired

The `app_state_from_config(&Config)` helper rewritten as
`default_app_state()` — no longer takes a Config, builds AppState
from `AppConfig::default()` + `Vault::default()` + `Functions::default()`
directly. The two callers (`create_test_ctx`,
`update_app_config_persists_changes`) updated.

The `to_request_context_creates_clean_state` test renamed to
`new_creates_clean_state` and rewritten to use `RequestContext::new`
directly.

### Doc comment refresh

Three module docstrings rewritten to reflect the post-16f world:

- `app_config.rs` — was "Phase 1 Step 0 ... not yet wired into the
  runtime." Now describes `AppConfig` as the runtime-resolved
  view of YAML, built via `AppConfig::from_config`.
- `app_state.rs` — was "Step 6.5 added mcp_factory and rag_cache
  ... neither wired in yet ... Step 8+ will connect." Now
  describes `AppState::init` as the wiring point.
- `request_context.rs` — was an extensive description of the
  bridge window with flat fields vs sub-struct fields, citing
  `Config::to_request_context`. Now describes the type's actual
  ownership/lifecycle without referring to deleted entry points.
- `tool_scope.rs` — was "Step 6.5 scope ... unused parallel
  structure ... Step 8 will rewrite." Now describes `ToolScope`
  as the live per-scope tool runtime.

(Other phase-era comments in `paths.rs`, `mcp_factory.rs`,
`rag_cache.rs` not touched. They reference Step 2 / Step 6.5 /
Step 8 but the affected types still exist and the descriptions
aren't actively misleading — those files weren't part of 16f
scope. Future cleanup if desired.)

## What Config looks like now

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub dry_run: bool,
    pub stream: bool,
    pub save: bool,
    pub keybindings: String,
    pub editor: Option<String>,
    pub wrap: Option<String>,
    pub wrap_code: bool,
    pub(super) vault_password_file: Option<PathBuf>,
    pub function_calling_support: bool,
    pub mapping_tools: IndexMap<String, String>,
    pub enabled_tools: Option<String>,
    pub visible_tools: Option<Vec<String>>,
    pub mcp_server_support: bool,
    pub mapping_mcp_servers: IndexMap<String, String>,
    pub enabled_mcp_servers: Option<String>,
    pub repl_prelude: Option<String>,
    pub cmd_prelude: Option<String>,
    pub agent_session: Option<String>,
    pub save_session: Option<bool>,
    pub compression_threshold: usize,
    pub summarization_prompt: Option<String>,
    pub summary_context_prompt: Option<String>,
    pub rag_embedding_model: Option<String>,
    pub rag_reranker_model: Option<String>,
    pub rag_top_k: usize,
    pub rag_chunk_size: Option<usize>,
    pub rag_chunk_overlap: Option<usize>,
    pub rag_template: Option<String>,
    pub document_loaders: HashMap<String, String>,
    pub highlight: bool,
    pub theme: Option<String>,
    pub left_prompt: Option<String>,
    pub right_prompt: Option<String>,
    pub user_agent: Option<String>,
    pub save_shell_history: bool,
    pub sync_models_url: Option<String>,
    pub clients: Vec<ClientConfig>,
}

impl Config {
    pub async fn load_with_interpolation(info_flag: bool) -> Result<Self> { ... }
    pub fn load_from_file(config_path: &Path) -> Result<(Self, String)> { ... }
    pub fn load_from_str(content: &str) -> Result<Self> { ... }
    pub fn load_dynamic(model_id: &str) -> Result<Self> { ... }
}
```

Just shape + four loaders. The three associated functions that
used to live here (`search_rag`, `load_macro`, `sync_models`)
were relocated in the 16f cleanup pass below — none of them
touched Config state, they were squatters from the god-object
era.

## Assumptions made

1. **Doc cleanup scope**: The user asked to "delete the dead-code
   scaffolding from Config and bridge.rs." Doc comments in
   `paths.rs`, `mcp_factory.rs`, `rag_cache.rs` still reference
   "Phase 1 Step 6.5 / Step 8" but the types they describe are
   still real and the descriptions aren't actively wrong (just
   historically dated). Left them alone. Updated only the docs in
   `app_config.rs`, `app_state.rs`, `request_context.rs`, and
   `tool_scope.rs` because those were either pointing at deleted
   types (`Config::to_request_context`) or making explicitly
   false claims ("not wired into the runtime yet").

2. **`set_*_default` helpers on AppConfig**: Lines 485–528 of
   `app_config.rs` define nine `#[allow(dead_code)]`
   `set_*_default` methods. These were added in earlier sub-phases
   as planned setters for runtime overrides. They're still unused.
   The 16-NOTES plan flagged them ("set_*_default ... become
   reachable") but reachability never happened. Since the user's
   directive was specifically "Config and bridge.rs scaffolding,"
   I left these untouched. Removing them is independent cleanup
   that doesn't block 16f.

3. **`reload_current_model` on RequestContext**: Same situation —
   one `#[allow(dead_code)]` left on a RequestContext method.
   Belongs to a different cleanup task; not Config or bridge
   scaffolding.

4. **`vault_password_file` visibility**: `Config.vault_password_file`
   was a private field. Made it `pub(super)` so
   `AppConfig::from_config` (sibling under `config/`) can read it
   for the field-copy. This is the minimum viable visibility —
   no code outside `config/` can touch it, matching the previous
   intent.

5. **Bootstrap vault construction in `load_with_interpolation`**:
   Used `AppConfig { vault_password_file: ..., ..AppConfig::default() }`
   instead of e.g. a dedicated helper. The vault only reads
   `vault_password_file` so this is sufficient. A comment explains
   the dual-vault pattern (bootstrap for secret interpolation vs
   canonical from `AppState::init`).

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy --all-targets` — clean, zero warnings
- `cargo test` — 122 passing, zero failures (same count as 16e)
- Grep confirmation:
  - `to_app_config` — zero hits in `src/`
  - `to_request_context` — zero hits in `src/`
  - `Config::init` / `Config::init_bare` — zero hits in `src/`
  - `bridge::` / `config::bridge` / `mod bridge` — zero hits in `src/`
  - `src/config/bridge.rs` — file deleted
- Config now contains only serde fields and load/helper
  functions; no runtime state.

## Phase 1 Step 16 — overall outcome

The full migration is complete:

| Sub-phase | Outcome |
|-----------|---------|
| 16a | `AppConfig::from_config` built |
| 16b | `install_builtins()` extracted |
| 16c | Vault on AppState (already-existing field, `Vault::init` rewired to `&AppConfig`) |
| 16d | `AppState::init` built |
| 16e | `main.rs` + completers + `RequestContext::bootstrap` switched to new flow |
| 16f | Bridge + Config runtime fields + dead methods deleted |

`Config` is a serde POJO. `AppConfig` is the runtime-resolved
process-wide settings. `AppState` owns process-wide services
(vault, MCP registry, base functions, MCP factory, RAG cache).
`RequestContext` owns per-request mutable state. Each struct
owns its initialization. The REST API surface is now trivial:
parse YAML → `AppConfig::from_config` → `AppState::init` →
per-request `RequestContext`.

## Files modified (16f)

- `src/config/mod.rs` — runtime fields/methods/Default entries
  deleted, imports cleaned up, `vault_password_file` made
  `pub(super)`, `load_with_interpolation` decoupled from
  `to_app_config`, default-test simplified
- `src/config/app_config.rs` — `from_config` inlines field-copy,
  `#[allow(dead_code)]` on `model_id` removed, three tests
  rewritten, module docstring refreshed
- `src/config/session.rs` — test helper rewired, imports updated
- `src/config/request_context.rs` — test helpers rewired,
  imports updated, module docstring refreshed
- `src/config/app_state.rs` — module docstring refreshed
- `src/config/tool_scope.rs` — module docstring refreshed

## Files deleted (16f)

- `src/config/bridge.rs`

## 16f cleanup pass — Config straggler relocation

After the main 16f deletions landed, three associated functions
remained on `impl Config` that took no `&self` and didn't touch
any Config field — they were holdovers from the god-object era,
attached to `Config` only because Config used to be the
namespace for everything. Relocated each to its rightful owner:

| Method | New home | Why |
|--------|----------|-----|
| `Config::load_macro(name)` | `Macro::load(name)` in `src/config/macros.rs` | Sibling of `Macro::install_macros` already there. The function loads a macro from disk and parses it into a `Macro` — pure macro concern. |
| `Config::search_rag(app, rag, text, signal)` | `Rag::search_with_template(&self, app, text, signal)` in `src/rag/mod.rs` | Operates on a `Rag` instance and one field of `AppConfig`. Pulled `RAG_TEMPLATE` constant along with it. |
| `Config::sync_models(url, signal)` | Free function `config::sync_models(url, signal)` in `src/config/mod.rs` | Fetches a URL, parses YAML, writes to `paths::models_override_file()`. No Config state involved. Sibling pattern to `install_builtins`, `default_sessions_dir`, `list_sessions`. |

### Caller updates

- `src/config/macros.rs:23` — `Config::load_macro(name)` → `Macro::load(name)`
- `src/config/input.rs:214` — `Config::search_rag(&self.app_config, rag, &self.text, abort_signal)` → `rag.search_with_template(&self.app_config, &self.text, abort_signal)`
- `src/main.rs:149` — `Config::sync_models(&url, abort_signal.clone())` → `sync_models(&url, abort_signal.clone())` (added `sync_models` to the `crate::config::{...}` import list)

### Constants relocated

- `RAG_TEMPLATE` moved from `src/config/mod.rs` to `src/rag/mod.rs` alongside the new `search_with_template` method that uses it.

### Final shape of `impl Config`

```rust
impl Config {
    pub async fn load_with_interpolation(info_flag: bool) -> Result<Self> { ... }
    pub fn load_from_file(config_path: &Path) -> Result<(Self, String)> { ... }
    pub fn load_from_str(content: &str) -> Result<Self> { ... }
    pub fn load_dynamic(model_id: &str) -> Result<Self> { ... }
}
```

Four loaders, all returning `Self` or `(Self, String)`. Nothing
else. The `Config` type is now genuinely what its docstring
claims: a serde POJO with constructors. No squatters.

### Verification (cleanup pass)

- `cargo check` — clean
- `cargo clippy --all-targets` — clean
- `cargo test` — 122 passing, zero failures
- `Config::sync_models` / `Config::load_macro` / `Config::search_rag` — zero hits in `src/`

### Files modified (cleanup pass)

- `src/config/mod.rs` — deleted `Config::load_macro`, `Config::search_rag`, `Config::sync_models`, and `RAG_TEMPLATE` const; added free `sync_models` function
- `src/config/macros.rs` — added `Macro::load`, updated import (added `Context`, `read_to_string`; removed `Config`)
- `src/rag/mod.rs` — added `RAG_TEMPLATE` const and `Rag::search_with_template` method
- `src/config/input.rs` — updated caller to `rag.search_with_template`
- `src/main.rs` — added `sync_models` to import list, updated caller
