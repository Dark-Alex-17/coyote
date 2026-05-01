# Test Plan: Config Loading and AppConfig

## Feature description

Loki loads its configuration from a YAML file (`config.yaml`) into
a `Config` struct, then converts it to `AppConfig` (immutable,
shared) + `RequestContext` (mutable, per-request). The `AppConfig`
holds all serialized fields; `RequestContext` holds runtime state.

## Behaviors to test

### Config loading
- [ ] Config loads from YAML file with all supported fields
- [x] Missing optional fields get correct defaults (config_defaults_match_expected)
- [ ] `model_id` defaults to first available model if empty (requires Config::init, integration test)
- [x] `temperature`, `top_p` default to `None`
- [x] `stream` defaults to `true`
- [x] `save` defaults to `false` (CORRECTED: was listed as true)
- [x] `highlight` defaults to `true`
- [x] `dry_run` defaults to `false`
- [x] `function_calling_support` defaults to `true`
- [x] `mcp_server_support` defaults to `true`
- [x] `compression_threshold` defaults to `4000`
- [ ] `document_loaders` populated from config and defaults (requires Config::init)
- [x] `clients` parsed from config (to_app_config_copies_clients)

### AppConfig conversion
- [x] `to_app_config()` copies all serialized fields correctly
- [x] `clients` field populated on AppConfig
- [ ] `visible_tools` correctly computed from `enabled_tools` config (deferred to plan 16)
- [x] `mapping_tools` correctly parsed
- [x] `mapping_mcp_servers` correctly parsed
- [ ] `user_agent` resolved (auto → crate name/version)

### RequestContext conversion
- [x] `to_request_context()` copies all runtime fields (to_request_context_creates_clean_state)
- [ ] `model` field populated with resolved model (requires Model::retrieve_model)
- [ ] `working_mode` set correctly (Repl vs Cmd)
- [x] `tool_scope` starts with default (empty)
- [x] `agent_runtime` starts as `None`

### AppConfig field accessors
- [x] `editor()` returns configured editor or $EDITOR
- [x] `light_theme()` returns theme flag
- [ ] `render_options()` returns options for markdown rendering
- [x] `sync_models_url()` returns configured or default URL

### Dynamic config updates
- [x] `update_app_config` closure correctly clones and replaces Arc
- [x] Changes to `dry_run`, `stream`, `save` persist across calls
- [x] Changes visible to subsequent `ctx.app.config` reads

## Context switching scenarios
- [ ] AppConfig remains immutable after construction (no field mutation)
- [ ] Multiple RequestContexts can share the same AppState
- [ ] Changing AppConfig fields (via clone-mutate-replace) doesn't
      affect other references to the old Arc

## Old code reference
- `src/config/mod.rs` — `Config` struct, `Config::init`, defaults
- `src/config/bridge.rs` — `to_app_config`, `to_request_context`
- `src/config/app_config.rs` — `AppConfig` struct and methods
