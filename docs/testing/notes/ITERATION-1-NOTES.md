# Iteration 1 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/01-config-and-appconfig.md`

## Tests created

| File | Test name | What it verifies |
|---|---|---|
| `src/config/mod.rs` | `config_defaults_match_expected` | All Config::default() fields match old code values |
| `src/config/app_config.rs` | `to_app_config_copies_serialized_fields` | to_app_config copies model_id, temperature, top_p, dry_run, stream, save, highlight, compression_threshold, rag_top_k |
| `src/config/app_config.rs` | `to_app_config_copies_clients` | clients field populated (empty by default) |
| `src/config/app_config.rs` | `to_app_config_copies_mapping_fields` | mapping_tools and mapping_mcp_servers copied correctly |
| `src/config/app_config.rs` | `editor_returns_configured_value` | editor() returns configured value |
| `src/config/app_config.rs` | `editor_falls_back_to_env` | editor() doesn't panic without config |
| `src/config/app_config.rs` | `light_theme_default_is_false` | light_theme() default |
| `src/config/app_config.rs` | `sync_models_url_has_default` | sync_models_url() has non-empty default |
| `src/config/request_context.rs` | `to_request_context_creates_clean_state` | RequestContext starts with clean state (no role/session/agent, empty tool_scope, no agent_runtime) |
| `src/config/request_context.rs` | `update_app_config_persists_changes` | Dynamic config updates via clone-mutate-replace persist |

**Total: 10 new tests (59 → 69)**

## Bugs discovered

None. The `save` default was `false` in both old and new code
(my plan file incorrectly said `true` — corrected).

## Observations for future iterations

1. The `Config::default().save` is `false`, but the plan file
   01 incorrectly listed it as `true`. Plan file should be
   updated to reflect the actual default.

2. `AppConfig::default()` doesn't exist natively (no derive).
   Tests construct it via `Config::default().to_app_config()`.
   This is fine since that's how it's created in production.

3. The `visible_tools` field computation happens during
   `Config::init` (not `to_app_config`). Testing the full
   visible_tools resolution requires integration-level testing
   with actual tool files. Deferred to plan file 16
   (functions-and-tools).

4. Testing `Config::init` directly is difficult because it reads
   from the filesystem, starts MCP servers, etc. The unit tests
   focus on the conversion paths which are the Phase 1 surface.

## Next iteration

Plan file 02: Roles — role loading, retrieve_role, use_role/exit_role,
use_prompt, extract_role, one-shot role messages, MCP context switching.
