# Phase 1 Step 8m — Implementation Notes

## Status

Done (partial — reduced GlobalConfig usage by 33%, cannot fully
eliminate due to Input/eval_tool_calls/client chain dependency).

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8m: REPL cleanup — eliminate `GlobalConfig` from REPL"

## Summary

Migrated 49 `config` references in `src/repl/mod.rs` to use
`RequestContext` or `AppConfig` equivalents. The REPL's `config`
reference count dropped from 148 to 99. Key changes: vault
operations via `ctx.app.vault`, `.exit role/session/agent` via
`ctx.*` methods + `ctx.bootstrap_tools`, session/agent info via
`ctx.*`, authentication via `ctx.app.config.*`, and various
`config.read()` → `ctx.*` replacements.

Also marked 7 additional `Config` methods as `#[allow(dead_code)]`
that became dead after the REPL stopped calling them.

## What was changed

### Files modified (2 files)

- **`src/repl/mod.rs`** — bulk migration of command handlers:
  - Vault: `config.read().vault.*` → `ctx.app.vault.*` (5 operations)
  - `.exit role`: MCP registry reinit → `ctx.exit_role()` + `ctx.bootstrap_tools()`
  - `.exit session` (standalone and within agent): → `ctx.exit_session()`
  - `.exit agent`: MCP registry reinit → `ctx.exit_agent(&app)` + `ctx.bootstrap_tools()`
  - `.info session`: `config.read().session_info()` → `ctx.session_info()`
  - `.info agent` / `.starter` / `.edit agent-config`: `config.read().agent_*` → `ctx.*`
  - `.authenticate`: `config.read().current_model()` → `ctx.current_model()`
  - `.edit role`: via `ctx.edit_role()`
  - `.edit macro` guard: `config.read().macro_flag` → `ctx.macro_flag`
  - Compression checks: `config.read().is_compressing_session()` → `ctx.is_compressing_session()`
  - Light theme: `config.read().light_theme()` → `ctx.app.config.light_theme()`
  - Various sync call reductions

- **`src/config/mod.rs`** — 7 methods marked `#[allow(dead_code)]`:
  `exit_role`, `session_info`, `exit_session`, `is_compressing_session`,
  `agent_banner`, `exit_agent`, `exit_agent_session`

## Remaining GlobalConfig usage in REPL (99 references)

These CANNOT be migrated until the client chain is migrated:

| Category | Count (approx) | Why |
|---|---|---|
| `Input::from_str(config, ...)` | ~10 | Input holds GlobalConfig for create_client |
| `ask(config, ctx, ...)` | ~10 | Passes config to Input construction |
| `Config::compress_session(config)` | 2 | Creates Input internally |
| `Config::maybe_compress_session` | 2 | Spawns task with GlobalConfig |
| `Config::maybe_autoname_session` | 2 | Spawns task with GlobalConfig |
| `Config::update(config, ...)` | 1 | Complex dispatcher, reads/writes config |
| `Config::delete(config, ...)` | 1 | Reads/writes config |
| `macro_execute(config, ...)` | 1 | Calls run_repl_command |
| `init_client(config, ...)` | 1 | Client needs GlobalConfig |
| `sync_ctx_to_config` / `sync_config_to_ctx` | ~15 | Bridge sync helpers |
| Reedline init (`ReplCompleter`, `ReplPrompt`) | ~5 | Trait objects hold GlobalConfig |
| `config.write().save_role/new_role/new_macro` | ~5 | Config file mutations |
| `config.write().edit_session/edit_config` | ~3 | Editor operations |
| Struct field + constructor | ~5 | `Repl { config }` |

## Key decisions

### 1. `.exit *` handlers use ctx methods + bootstrap_tools

Instead of the MCP registry take/reinit pattern, the exit handlers
now call `ctx.exit_role()` / `ctx.exit_session()` / `ctx.exit_agent(&app)`
followed by `ctx.bootstrap_tools(&app, true).await?` to rebuild the
tool scope with the global MCP server set. Then `sync_ctx_to_config`
updates the GlobalConfig for reedline/Input.

### 2. Cannot remove Repl's config field

The `config: GlobalConfig` field stays because `ask`, `Input::from_str`,
`init_client`, `Config::compress_session`, `Config::maybe_*`, and
reedline components all need it. Full removal requires migrating the
client chain.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean
- `cargo test` — 63 passed, 0 failed

## Phase 1 completion assessment

With Step 8m done, Phase 1's Step 8 sub-steps (8a through 8m) are
all complete. The GlobalConfig is significantly reduced but not
eliminated. The remaining dependency is the **client chain**:

```
Input.config: GlobalConfig
  → create_client() → init_client(&GlobalConfig)
    → Client.global_config: GlobalConfig
      → eval_tool_calls(&GlobalConfig)
        → ToolCall::eval(&GlobalConfig)
          → all tool handlers take &GlobalConfig
```

Eliminating this chain requires:
1. Migrating `init_client` to `&AppConfig` + `&[ClientConfig]`
2. Changing every client struct from `GlobalConfig` to `AppConfig`
3. Migrating `eval_tool_calls` to `&AppConfig` + `&mut RequestContext`
4. Migrating all tool handlers similarly

This is a Phase 2 concern or a dedicated "client chain migration"
effort.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8m
- Step 8l notes: `docs/implementation/PHASE-1-STEP-8l-NOTES.md`
- QA checklist: `docs/QA-CHECKLIST.md`
