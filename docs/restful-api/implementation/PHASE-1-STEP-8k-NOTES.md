# Phase 1 Step 8k — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8k: Migrate `Agent::init` and agent lifecycle"

## Summary

Changed `Agent::init` from taking `&GlobalConfig` to taking
`&AppConfig` + `&AppState` + `&Model` + `info_flag`. Removed
MCP registry lifecycle code from `Agent::init` (moved to caller
`Config::use_agent`). Changed `AgentConfig::load_envs` to take
`&AppConfig`. Zero `GlobalConfig` references remain in
`config/agent.rs`.

## What was changed

### Files modified (3 files)

- **`src/config/agent.rs`**:
  - `Agent::init` signature: `(config: &GlobalConfig, name, abort_signal)` →
    `(app: &AppConfig, app_state: &AppState, current_model: &Model,
    info_flag: bool, name, abort_signal)`
  - Removed MCP registry take/reinit from Agent::init (lines 107-135
    in original). MCP lifecycle is now the caller's responsibility.
  - `config.read().document_loaders` → `app.document_loaders`
  - `config.read().mcp_server_support` → `app.mcp_server_support`
  - Model resolution uses `app` directly instead of
    `config.read().to_app_config()`
  - RAG loading uses `app` + `app.clients` directly
  - `config.read().vault` → `app_state.vault.clone()`
  - `AgentConfig::load_envs(&Config)` → `load_envs(&AppConfig)`
  - Added `Agent::append_mcp_meta_functions(names)` and
    `Agent::mcp_server_names()` accessors

- **`src/config/mod.rs`**:
  - `Config::use_agent` now constructs `AppConfig`, `AppState`
    (temporary), `current_model`, `info_flag` from the GlobalConfig
    and passes them to the new `Agent::init`
  - MCP registry take/reinit code moved here from Agent::init
  - After Agent::init, appends MCP meta functions to the agent's
    function list

- **`src/main.rs`**:
  - Updated the direct `Agent::init` call (build-tools path) to use
    the new signature

## Key decisions

### 1. MCP lifecycle moved from Agent::init to caller

The plan said "Replace McpRegistry::reinit call with McpFactory::acquire()
pattern." Instead, I moved the MCP lifecycle entirely out of Agent::init
and into the caller. This is cleaner because:
- Agent::init becomes pure spec-loading (no side effects on shared state)
- Different callers can use different MCP strategies (McpRegistry::reinit
  for GlobalConfig path, McpFactory::acquire for RequestContext path)
- The MCP meta function names are appended by the caller after init

### 2. Temporary AppState in Config::use_agent

`Config::use_agent` constructs a temporary `AppState` from the GlobalConfig
to pass to Agent::init. The MCP config and log path are extracted from
the GlobalConfig's McpRegistry. The MCP factory is a fresh empty one
(Agent::init doesn't call acquire — it's just for API compatibility).

### 3. No REPL or main.rs changes needed

Both call `Config::use_agent` which adapts internally. The REPL's
`.agent` handler and main.rs agent path are unchanged.

## GlobalConfig reference count

| Module | Before 8k | After 8k |
|---|---|---|
| `config/agent.rs` | ~15 | 0 |

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean
- `cargo test` — 63 passed, 0 failed

## Handoff

Step 8l (supervisor migration) can now proceed. `Agent::init` no
longer needs `GlobalConfig`, which means sub-agent spawning in
`supervisor.rs` can construct agents using `&AppConfig` + `&AppState`
without needing to create child GlobalConfigs.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8k
- Step 8i notes: `docs/implementation/PHASE-1-STEP-8i-NOTES.md`
- Step 8j notes: `docs/implementation/PHASE-1-STEP-8j-NOTES.md`
- QA checklist: `docs/QA-CHECKLIST.md` — items 4, 11, 12
