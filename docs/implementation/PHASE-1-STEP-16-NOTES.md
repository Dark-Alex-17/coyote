# Phase 1 Step 16 Notes: Config → AppConfig Migration Cleanup

**Date**: 2026-04-19
**Status**: PENDING (cleanup task)

## Overview

Complete the migration of mutations from Config to AppConfig/AppState so that Config becomes a pure POJO (serde serialization/deserialization only).

## Problem

The current startup flow mutates `Config` during init, then calls `to_app_config()` which **only copies serialized fields**, losing all the runtime mutations:

```
YAML → Config (mutated during init)
    ↓
cfg.to_app_config()  ← COPIES ONLY, loses mutations
```

### Mutations happening in Config::init

These methods mutate Config and should be migrated to AppConfig:

| Method | Location | What It Does |
|-------|----------|-------------|
| `load_envs()` | `mod.rs:387` | Env var overrides into Config fields |
| `set_wrap(&wrap)?` | `mod.rs:390` | Parse wrap string into Config.wrap |
| `load_functions()` | `mod.rs:393` | Load tool functions into Config.functions |
| `setup_model()` | `mod.rs:398` | Resolve model name → Model |
| `load_mcp_servers()` | `mod.rs:395` | Start MCP servers into Config.mcp_registry |
| `setup_document_loaders()` | `mod.rs:399` | Load document loaders |
| `setup_user_agent()` | `mod.rs:400` | Set user_agent to "auto" |

### Other Config Methods Still In Use

| Method | Location | Used By |
|--------|----------|---------|
| `set_model(&model_id)` | `mod.rs:457-466` | Runtime (role-like mutators) |
| `vault_password_file()` | `mod.rs:411-419` | `vault/mod.rs:40` |
| `sessions_dir()` | `mod.rs:421-429` | Session management |
| `role_like_mut()` | `mod.rs:431-441` | Role/session/agent mutation |
| `set_wrap(&str)` | `mod.rs:443-455` | Runtime wrap setting |

## Solution

**Option A (Quick)**: Move mutations after the bridge in `main.rs`:

```rust
// main.rs
let cfg = Config::init(...).await?;
let app_config = Arc::new(cfg.to_app_config());  // Step 1: copy serialized

// Step 2: apply mutations to AppConfig
app_config.load_envs();
app_config.set_wrap(&wrap)?;
app_config.setup_model()?;  // May need refactored

// Step 3: build AppState
let app_state = Arc::new(AppState {
    config: app_config,
    vault: cfg.vault.clone(),
    // ... other fields
});

// Step 4: build RequestContext (runtime only, no config logic)
let ctx = cfg.to_request_context(app_state);
```

**Option B (Proper)**: Remove Config entirely from startup flow - build AppConfig directly from YAML.

## Duplicated Code to Clean Up

After migration, these duplicated methods can be removed from AppConfig:

| Duplicated | Config Location | AppConfig Location |
|-----------|-----------------|-------------------|
| `load_envs()` | `mod.rs:582-722` | `app_config.rs:283-427` |
| `set_wrap()` | `mod.rs:443-455` | `app_config.rs:247-259` |
| `setup_document_loaders()` | `mod.rs:782-789` | `app_config.rs:262-269` |
| `setup_user_agent()` | `mod.rs:791-799` | `app_config.rs:272-280` |
| Default impls | `mod.rs:232-311` | `app_config.rs:94-149` |

## Target State

After Step 16:

- [ ] Config only has serde fields + Deserialization (no init logic)
- [ ] AppConfig receives all runtime mutations
- [ ] AppState built from AppConfig + Vault
- [ ] RequestContext built from AppState + runtime state
- [ ] Duplicated methods removed from AppConfig (or retained if needed)
- [ ] Bridge simplified to just field copying

## Files to Modify

- `src/config/mod.rs` - Remove init mutations, keep only serde
- `src/config/app_config.rs` - Enable mutations, remove duplication
- `src/main.rs` - Move bridge after mutations
- `src/config/bridge.rs` - Simplify or remove

## Notes

- This cleanup enables proper REST API behavior
- Each HTTP request should build fresh RequestContext from AppConfig
- AppConfig should reflect actual runtime configuration