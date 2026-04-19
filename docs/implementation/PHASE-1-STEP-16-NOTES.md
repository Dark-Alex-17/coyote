# Phase 1 Step 16 — Implementation Notes

## Status

Pending. Architecture plan approved; ready for sub-phase execution.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 16: Complete Config → AppConfig Migration (Post-QA)"

## Problem

The current startup flow mutates `Config` during `Config::init()`,
then converts it to `AppConfig` via `bridge.rs::to_app_config()`. This
design was transitional — it let us build the new structs alongside
the old one without a big-bang migration.

Now that the transition is nearly done, we want `Config` to be a
genuine serde POJO: no runtime state, no init logic, nothing that
couldn't round-trip through YAML. The structs that actually represent
runtime state (`AppConfig`, `AppState`, `RequestContext`) should own
their own initialization logic.

## Target architecture

Instead of migrating mutations incrementally through the bridge, we
**pivot the initialization direction**. Each struct owns its own init.

```
YAML file
    ↓ Config::load_from_file (serde only — no init logic)
Config (pure POJO)
    ↓ AppConfig::from_config(config) → AppConfig
AppConfig (immutable app-wide settings, self-initializing)
    ↓ AppState::init(Arc<AppConfig>, ...).await → AppState
AppState (shared process state: vault, mcp_factory, rag_cache, mcp_registry, functions)
    ↓ RequestContext::new(Arc<AppState>, working_mode)
RequestContext (per-request mutable state, unchanged)
```

### Struct responsibilities (post-16)

**`Config`** — trivial serde POJO:
- Only `#[serde(...)]` fields (no `#[serde(skip)]`)
- Only method: `load_from_file(path) -> Result<(Config, String)>`
  (returns parsed Config + raw YAML content for secret interpolation)
- Can be round-tripped via YAML

**`AppConfig::from_config(config) -> Result<AppConfig>`** absorbs:
- Field-copy from `Config` (same as today's `to_app_config`)
- `load_envs()` — env var overrides
- `set_wrap()` — wrap string validation
- `setup_document_loaders()` — default pdf/docx loaders
- `setup_user_agent()` — resolve `"auto"` user agent
- `resolve_model()` — logic from `Config::setup_model` (picks default
  model if `model_id` is empty)

**`AppState::init(app_config, log_path, start_mcp_servers, abort_signal)`** absorbs:
- `Vault::init(&app_config)` (vault moves from Config to AppState)
- `McpRegistry::init(...)` (currently `Config::load_mcp_servers`)
- `Functions::init(...)` (currently `Config::load_functions`)
- Returns fully-wired `AppState`

**`install_builtins()`** — top-level free function (replaces
`Agent::install_builtin_agents()` + `Macro::install_macros()` being
called inside `Config::init`). Called once from `main.rs` before any
config loading. Config-independent — just copies embedded assets.

**`bridge.rs` — deleted.** No more `to_app_config()` /
`to_request_context()`.

## Sub-phase layout

| Sub-phase | Scope |
|-----------|-------|
| 16a | Build `AppConfig::from_config` absorbing env/wrap/docs/user-agent/model-resolution logic |
| 16b | Extract `install_builtins()` as top-level function |
| 16c | Migrate `Vault` onto `AppState` |
| 16d | Build `AppState::init` absorbing MCP-registry/functions logic |
| 16e | Update `main.rs` + audit all 15 `Config::init()` callers, switch to new flow |
| 16f | Delete Config runtime fields, bridge.rs, `Config::init`, duplicated methods |

Sub-phases 16a–16d can largely proceed in parallel (each adds new
entry points without removing the old ones). 16e switches callers.
16f is the final cleanup.

## 16a — AppConfig::from_config

**Target signature:**
```rust
impl AppConfig {
    pub fn from_config(config: Config) -> Result<Self> {
        let mut app_config = Self {
            // Copy all serde fields from config
        };
        app_config.load_envs();
        if let Some(wrap) = app_config.wrap.clone() {
            app_config.set_wrap(&wrap)?;
        }
        app_config.setup_document_loaders();
        app_config.setup_user_agent();
        app_config.resolve_model()?;
        Ok(app_config)
    }

    fn resolve_model(&mut self) -> Result<()> {
        if self.model_id.is_empty() {
            let models = crate::client::list_models(self, ModelType::Chat);
            if models.is_empty() {
                bail!("No available model");
            }
            self.model_id = models[0].id();
        }
        Ok(())
    }
}
```

**New method: `AppConfig::resolve_model()`** — moves logic from
`Config::setup_model`. Ensures `model_id` is a valid, non-empty
concrete model reference.

**Note on `Model` vs `model_id`:** `Model` (the resolved runtime
handle) stays on `RequestContext`. AppConfig owns `model_id: String`
(the config default). RequestContext.model is built by calling
`Model::retrieve_model(&app_config, &model_id, ModelType::Chat)`
during context construction. They're different types for a reason.

**Files modified (16a):**
- `src/config/app_config.rs` — add `from_config`, `resolve_model`
- Also remove `#[allow(dead_code)]` from `load_envs`, `set_wrap`,
  `setup_document_loaders`, `setup_user_agent`, `set_*_default`,
  `ensure_default_model_id` (they all become reachable)

**Bridge still exists after 16a.** `Config::init` still calls its own
mutations for now. 16a just introduces the new entry point.

## 16b — install_builtins()

**Target signature:**
```rust
// In src/config/mod.rs or a new module
pub fn install_builtins() -> Result<()> {
    Agent::install_builtin_agents()?;
    Macro::install_macros()?;
    Ok(())
}
```

**Changes:**
- Remove `Agent::install_builtin_agents()?;` and
  `Macro::install_macros()?;` calls from inside `Config::init`
- Add `install_builtins()?;` to `main.rs` as the first step before
  any config loading

Both functions are Config-independent (they just copy embedded
assets to the config directory), so this is a straightforward
extraction.

**Files modified (16b):**
- `src/config/mod.rs` — remove calls from `Config::init`, expose
  `install_builtins` as a module-level pub fn
- `src/main.rs` — call `install_builtins()?;` at startup

## 16c — Vault → AppState

Today `Config.vault: Arc<GlobalVault>` is a `#[serde(skip)]` runtime
field populated by `Vault::init(config)`. Post-16c, the vault lives
natively on `AppState`.

**Current:**
```rust
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub vault: GlobalVault,  // Already here, sourced from config.vault
    ...
}
```

Wait — `AppState.vault` already exists. The work in 16c is just:

1. Change `Vault::init(config: &Config)` → `Vault::init(config: &AppConfig)`
   - `Vault::init` only reads `config.vault_password_file()`, which
     is already a serde field on AppConfig. Rename the param.
2. Delete `Config.vault` field (no longer needed once 16e routes
   through AppState)
3. Update `main.rs` to call `Vault::init(&app_config)` instead of
   `cfg.vault.clone()`

**Files modified (16c):**
- `src/vault/mod.rs` — `Vault::init` takes `&AppConfig`
- `src/config/mod.rs` — delete `Config.vault` field (after callers
  switch)

## 16d — AppState::init

**Target signature:**
```rust
impl AppState {
    pub async fn init(
        config: Arc<AppConfig>,
        log_path: Option<PathBuf>,
        start_mcp_servers: bool,
        abort_signal: AbortSignal,
    ) -> Result<Self> {
        let vault = Vault::init(&config);
        let functions = {
            let mut fns = Functions::init(
                config.visible_tools.as_ref().unwrap_or(&Vec::new())
            )?;
            // REPL-specific fns appended by RequestContext, not here
            fns
        };
        let mcp_registry = McpRegistry::init(
            log_path.clone(),
            start_mcp_servers,
            config.enabled_mcp_servers.clone(),
            abort_signal,
            &config,  // new signature: &AppConfig
        ).await?;
        let (mcp_config, mcp_log_path) = (
            mcp_registry.mcp_config().cloned(),
            mcp_registry.log_path().cloned(),
        );
        Ok(Self {
            config,
            vault,
            mcp_factory: Default::default(),
            rag_cache: Default::default(),
            mcp_config,
            mcp_log_path,
            mcp_registry: Some(mcp_registry),  // NEW field
            functions,                          // NEW field
        })
    }
}
```

**New AppState fields:**
- `mcp_registry: Option<McpRegistry>` — the live registry of started
  MCP servers (currently on Config)
- `functions: Functions` — the global function declarations (currently
  on Config)

These become the "source of truth" that `ToolScope` copies from when
a scope transition happens.

**`McpRegistry::init` signature change:** today takes `&Config`,
needs to take `&AppConfig`. Only reads serialized fields.

**Files modified (16d):**
- `src/config/app_state.rs` — add `init`, add `mcp_registry` +
  `functions` fields
- `src/mcp/mod.rs` — `McpRegistry::init` takes `&AppConfig`

**Important:** `Functions.append_user_interaction_functions()` is
currently called inside `Config::load_functions` when in REPL mode.
That logic is working-mode-dependent and belongs on `RequestContext`
(which knows its mode), not `AppState`. The migration moves that
append step to `RequestContext::new` or similar.

## 16e — Switch main.rs and 15 callers

**New `main.rs` flow:**
```rust
install_builtins()?;
let (config, raw_yaml) = Config::load_from_file(&paths::config_file())?;

// Secret interpolation (two-pass)
let bootstrap_vault = Vault::init_from_password_file(&config.vault_password_file());
let interpolated = interpolate_secrets_or_err(&raw_yaml, &bootstrap_vault, info_flag)?;
let final_config = if interpolated != raw_yaml {
    Config::load_from_str(&interpolated)?
} else {
    config
};

let app_config = Arc::new(AppConfig::from_config(final_config)?);
let app_state = Arc::new(
    AppState::init(
        app_config.clone(),
        log_path,
        start_mcp_servers,
        abort_signal.clone(),
    ).await?
);
let ctx = RequestContext::new(app_state.clone(), working_mode);
```

**Secret interpolation complication:** Today's `Config::init` does a
two-pass YAML parse — load, init vault, interpolate secrets into raw
content, re-parse if content changed. In the new flow:
1. Load Config from YAML (also returns raw content)
2. Bootstrap Vault using Config's `vault_password_file` serde field
3. Interpolate secrets in raw content
4. If content changed, re-parse Config
5. Build AppConfig from final Config
6. Build AppState (creates the full Vault via `Vault::init(&app_config)`)

Step 2 and step 6 create the vault twice — once bootstrap (to decrypt
secrets in raw YAML), once full (for AppState). This matches current
behavior, just made explicit.

**15 callers of `Config::init()`** — audit required. Discovery
happens during 16e execution. Open questions flagged for user input
as discovered.

| File | Expected Action |
|------|-----------------|
| `main.rs` | Use new flow |
| `client/common.rs` | Probably needs AppConfig only |
| `vault/mod.rs` | Already uses `Config::vault_password_file`; switch to AppConfig |
| `config/request_context.rs` | Test helper — use `AppState::test_default()` or build directly |
| `config/session.rs` | Test helper — same |
| `rag/mod.rs` | Probably AppConfig |
| `function/supervisor.rs` | Test helper |
| `utils/request.rs` | Probably AppConfig |
| `config/role.rs` | Test helper |
| `utils/clipboard.rs` | Probably AppConfig |
| `supervisor/mod.rs` | Test helper |
| `repl/mod.rs` | Test helper |
| `parsers/common.rs` | Probably AppConfig |
| `utils/abort_signal.rs` | Probably AppConfig |
| `utils/spinner.rs` | Probably AppConfig |

**Files modified (16e):**
- `src/main.rs`
- Any of the 15 files above that aren't trivial — may need
  `test_default()` helpers added

## 16f — Final cleanup

**Delete from `Config`:**
- All `#[serde(skip)]` fields: `vault`, `macro_flag`, `info_flag`,
  `agent_variables`, `model`, `functions`, `mcp_registry`,
  `working_mode`, `last_message`, `role`, `session`, `rag`, `agent`,
  `tool_call_tracker`, `supervisor`, `parent_supervisor`,
  `self_agent_id`, `current_depth`, `inbox`,
  `root_escalation_queue`
- `Config::init` (whole function)
- `Config::load_envs`, `Config::load_functions`,
  `Config::load_mcp_servers`, `Config::setup_model`,
  `Config::set_model`, `Config::role_like_mut`,
  `Config::sessions_dir`, `Config::set_wrap`,
  `Config::setup_document_loaders`, `Config::setup_user_agent`,
  `Config::vault_password_file` (redundant with AppConfig's)

**Delete:**
- `src/config/bridge.rs` (entire file)
- `mod bridge;` declaration in `config/mod.rs`

**Resulting `Config`:**
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    // Only serde-annotated fields — the YAML shape
    pub model_id: String,
    pub temperature: Option<f64>,
    // ... all the other serde fields
}

impl Config {
    pub fn load_from_file(path: &Path) -> Result<(Config, String)> { ... }
    pub fn load_from_str(content: &str) -> Result<Config> { ... }
}
```

A genuine POJO. No runtime state. No init logic. Just shape.

## Open questions (for execution)

1. **`Vault::init_bare`** — currently used as a fallback when no
   Config exists. Does it still need to exist? The default
   `vault_password_file` location is static. Might need
   `AppConfig::default().vault_password_file()` or a free function.
2. **Secret interpolation ownership** — does `AppConfig::from_config`
   handle it internally (takes raw YAML string and interpolates), or
   does `main.rs` orchestrate the two-pass explicitly? Leaning toward
   `main.rs` orchestration (cleaner separation).
3. **REPL-only `append_user_interaction_functions`** — moves to
   `RequestContext::new`? Or stays as a post-init append called
   explicitly?
4. **`Functions::init` + MCP meta functions** — today
   `load_mcp_servers` calls `self.functions.append_mcp_meta_functions(...)`
   after starting servers. In the new flow, `AppState::init` does
   this. Verify ordering is preserved.
5. **Testing strategy** — User said don't worry unless trivial. If
   test helpers need refactoring to work with new flow, prefer
   adding `test_default()` methods gated by `#[cfg(test)]` over
   rewriting tests.

## Dependencies between sub-phases

```
16a ──┐
16b ──┤
16c ──┼──→ 16d ──→ 16e ──→ 16f
      │
16b, 16c, 16a independent and can run in any order
16d depends on 16c (vault on AppConfig)
16e depends on 16a, 16d (needs the new entry points)
16f depends on 16e (needs all callers switched)
```

## Rationale for this architecture

The original Step 16 plan migrated mutations piecewise through the
existing `to_app_config()` bridge. That works but:

- Leaves the bridge in place indefinitely
- Keeps `Config` burdened with both YAML shape AND runtime state
- Requires careful ordering to avoid breaking downstream consumers
  like `load_functions`/`load_mcp_servers`/`setup_model`
- Creates transitional states where some mutations live on Config,
  some on AppConfig

The new approach:

- Eliminates `bridge.rs` entirely
- Makes `Config` a true POJO
- Makes `AppConfig`/`AppState` self-contained (initialize from YAML
  directly)
- REST API path is trivial: `AppConfig::from_config(yaml_string)`
- Test helpers can build `AppConfig`/`AppState` without Config
- Each struct owns exactly its concerns

## Verification criteria for each sub-phase

- 16a: `cargo check` + `cargo test` clean. `AppConfig::from_config`
  produces the same state as `Config::init` + `to_app_config()` for
  the same YAML input.
- 16b: `install_builtins()` called once from `main.rs`; agents and
  macros still install on first startup.
- 16c: `Vault::init` takes `&AppConfig`; `Config.vault` field deleted.
- 16d: `AppState::init` builds a fully-wired `AppState` from
  `Arc<AppConfig>` + startup context. MCP servers start; functions
  load.
- 16e: REPL starts, all CLI flags work, all env vars honored, all
  existing tests pass.
- 16f: Grep for `Config {` / `Config::init(` / `bridge::to_` shows
  zero non-test hits. `Config` has only serde fields.
