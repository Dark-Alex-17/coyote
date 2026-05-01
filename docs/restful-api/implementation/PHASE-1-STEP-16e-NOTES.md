# Phase 1 Step 16e — Implementation Notes

## Status

Done.

## Plan reference

- Parent plan: `docs/implementation/PHASE-1-STEP-16-NOTES.md`
- Sub-phase goal: "Switch main.rs and all Config::init() callers
  to the new flow"

## Summary

`main.rs` and `cli/completer.rs` no longer call `Config::init` or
`Config::init_bare` — they use the new flow:
`Config::load_with_interpolation` → `AppConfig::from_config` →
`AppState::init` → `RequestContext::bootstrap`.

The bridge `Config::to_request_context` and the old `Config::init`
are now dead code, gated with `#[allow(dead_code)]` pending deletion
in 16f.

## What was changed

### New helpers

**`Config::load_with_interpolation(info_flag: bool) -> Result<Self>`** in
`src/config/mod.rs` — absorbs the two-pass YAML parse with secret
interpolation. Handles:
1. Missing config file (creates via `create_config_file` if TTY, or
   `load_dynamic` from env vars)
2. Reading the raw YAML content
3. Bootstrapping a Vault from the freshly-parsed Config
4. Interpolating secrets
5. Re-parsing Config if interpolation changed anything
6. Sets `config.vault` (legacy field — deleted in 16f)

**`config::default_sessions_dir() -> PathBuf`** and
**`config::list_sessions() -> Vec<String>`** free functions —
provide session listing without needing a Config instance. Used by
the session completer.

**`RequestContext::bootstrap(app: Arc<AppState>, working_mode,
info_flag) -> Result<Self>`** in `src/config/request_context.rs` —
the new entry point for creating the initial RequestContext. Builds:
- Resolved `Model` from `app.config.model_id`
- `ToolScope.functions` cloned from `app.functions` with
  `append_user_interaction_functions` added in REPL mode
- `ToolScope.mcp_runtime` synced from `app.mcp_registry`

### Made public in Config for new flow

- `Config::load_from_file` (was `fn`)
- `Config::load_from_str` (was `fn`)
- `Config::load_dynamic` (was `fn`)
- `config::create_config_file` (was `async fn`)

### src/main.rs

Three startup paths rewired:

```rust
// Path 1: --authenticate
let cfg = Config::load_with_interpolation(true).await?;
let app_config = AppConfig::from_config(cfg)?;
let (client_name, provider) =
    resolve_oauth_client(client_arg.as_deref(), &app_config.clients)?;
oauth::run_oauth_flow(&*provider, &client_name).await?;

// Path 2: vault flags
let cfg = Config::load_with_interpolation(true).await?;
let app_config = AppConfig::from_config(cfg)?;
let vault = Vault::init(&app_config);
return Vault::handle_vault_flags(cli, &vault);

// Path 3: main
let cfg = Config::load_with_interpolation(info_flag).await?;
let app_config: Arc<AppConfig> = Arc::new(AppConfig::from_config(cfg)?);
let app_state: Arc<AppState> = Arc::new(
    AppState::init(app_config, log_path, start_mcp_servers, abort_signal.clone()).await?
);
let ctx = RequestContext::bootstrap(app_state, working_mode, info_flag)?;
```

No more `Config::init`, `Config::to_app_config`, `cfg.mcp_registry`,
or `cfg.to_request_context` references in `main.rs`.

### src/cli/completer.rs

Three completers that needed config access updated:

- `model_completer` → uses new `load_app_config_for_completion()`
  helper (runs `Config::load_with_interpolation` synchronously from
  the completion context; async via `Handle::try_current` or a fresh
  runtime)
- `session_completer` → uses the new free function
  `list_sessions()` (no Config needed)
- `secrets_completer` → uses `Vault::init(&app_config)` directly

### #[allow(dead_code)] removed

- `AppConfig::from_config`
- `AppConfig::resolve_model`
- `AppState::init`
- `AppState.rag_cache` (was flagged dead; now wired in)

### #[allow(dead_code)] added (temporary, deleted in 16f)

- `Config::init_bare` — no longer called
- `Config::sessions_dir` — replaced by free function
- `Config::list_sessions` — replaced by free function
- `Config::to_request_context` — replaced by `RequestContext::bootstrap`

## Behavior parity

- `main.rs` startup now invokes:
  - `install_builtins()` (installs builtin global tools, agents,
    macros — same files get copied as before, Step 16b)
  - `Config::load_with_interpolation` (same YAML loading + secret
    interpolation as old `Config::init`)
  - `AppConfig::from_config` (same env/wrap/docs/user-agent/model
    resolution as old Config mutations)
  - `AppState::init` (same vault init + MCP registry startup +
    global Functions loading as old Config methods, now owned by
    AppState; also pre-registers initial servers with McpFactory —
    new behavior that fixes a latent cache miss bug)
  - `RequestContext::bootstrap` (same initial state as old bridge
    `to_request_context`: resolved Model, Functions with REPL
    extensions, MCP runtime from registry)

- Completer paths now use a lighter-weight config load (no MCP
  startup) which is appropriate since shell completion isn't
  supposed to start subprocesses.

## Files modified

- `src/config/mod.rs` — added `load_with_interpolation`,
  `default_sessions_dir`, `list_sessions`; made 3 methods public;
  added `#[allow(dead_code)]` to `Config::init_bare`,
  `sessions_dir`, `list_sessions`.
- `src/config/request_context.rs` — added `bootstrap`.
- `src/config/app_config.rs` — removed 2 `#[allow(dead_code)]`
  gates.
- `src/config/app_state.rs` — removed 2 `#[allow(dead_code)]`
  gates.
- `src/config/bridge.rs` — added `#[allow(dead_code)]` to
  `to_request_context`.
- `src/main.rs` — rewired three startup paths.
- `src/cli/completer.rs` — rewired three completers.

## Assumptions made

1. **Completer helper runtime handling**: The three completers run
   in a sync context (clap completion). The new
   `load_app_config_for_completion` uses
   `Handle::try_current().ok()` to detect if a tokio runtime
   exists; if so, uses `block_in_place`; otherwise creates a
   fresh runtime. This matches the old `Config::init_bare` pattern
   (which also used `block_in_place` + `block_on`).

2. **`Config::to_request_context` kept with `#[allow(dead_code)]`**:
   It's unused now but 16f deletes it cleanly. Leaving it in place
   keeps 16e a non-destructive switchover.

3. **`RequestContext::bootstrap` returns `Result<Self>` not
   `Arc<Self>`**: caller decides wrapping. main.rs doesn't wrap;
   the REPL wraps `Arc<RwLock<RequestContext>>` a few lines later.

4. **`install_builtin_global_tools` added to `install_builtins`**:
   A function added in user's 16b commit extracted builtin tool
   installation out of `Functions::init` into a standalone function.
   My Step 16b commit that extracted `install_builtins` missed
   including this function — fixed in this step.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean, zero warnings
- `cargo test` — 122 passing, zero failures
- Grep confirmation:
  - `Config::init(` — only called from `Config::init_bare` (which
    is now dead)
  - `Config::init_bare` — no external callers (test helper uses
    `#[allow(dead_code)]`)
  - `to_request_context` — zero callers outside bridge.rs
  - `cfg.mcp_registry` / `cfg.functions` / `cfg.vault` references
    in main.rs — zero

## Remaining work for Step 16

- **16f**: Delete all `#[allow(dead_code)]` scaffolding:
  - `Config::init`, `Config::init_bare`
  - `Config::sessions_dir`, `Config::list_sessions`
  - `Config::set_wrap`, `Config::setup_document_loaders`,
    `Config::setup_user_agent`, `Config::load_envs`,
    `Config::load_functions`, `Config::load_mcp_servers`,
    `Config::setup_model`, `Config::set_model`,
    `Config::role_like_mut`, `Config::vault_password_file`
  - `bridge.rs` — delete entirely
  - All `#[serde(skip)]` runtime fields on `Config`
  - `mod bridge;` declaration

After 16f, `Config` will be a pure serde POJO with only serialized
fields and `load_from_file` / `load_from_str` / `load_dynamic` /
`load_with_interpolation` methods.

## Migration direction achieved

Before 16e:
```
main.rs: Config::init → to_app_config → AppState {...} → to_request_context
```

After 16e:
```
main.rs:
  install_builtins()
  Config::load_with_interpolation → AppConfig::from_config
  AppState::init(app_config, ...).await
  RequestContext::bootstrap(app_state, working_mode, info_flag)
```

No more god-init. Each struct owns its initialization. The REST
API path is now trivial: skip `install_builtins()` if not desired,
call `AppConfig::from_config(yaml_string)`, call
`AppState::init(...)`, create per-request `RequestContext` as
needed.
