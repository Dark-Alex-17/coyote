# Iteration 2 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/02-roles.md`

## Tests created

### src/config/role.rs (12 new tests, 15 total)

| Test name | What it verifies |
|---|---|
| `role_new_parses_prompt` | Role::new extracts prompt text |
| `role_new_parses_metadata` | Metadata block parses model, temperature, top_p |
| `role_new_parses_enabled_tools` | enabled_tools from metadata |
| `role_new_parses_enabled_mcp_servers` | enabled_mcp_servers from metadata |
| `role_new_no_metadata_has_none_fields` | No metadata → all optional fields None |
| `role_builtin_shell_loads` | Built-in "shell" role loads |
| `role_builtin_code_loads` | Built-in "code" role loads |
| `role_builtin_nonexistent_errors` | Non-existent built-in → error |
| `role_default_has_empty_fields` | Default role has empty name/prompt |
| `role_set_model_updates_model` | set_model() changes the model |
| `role_set_temperature_works` | set_temperature() changes temperature |
| `role_export_includes_metadata` | export() includes metadata and prompt |

### src/config/request_context.rs (5 new tests, 7 total)

| Test name | What it verifies |
|---|---|
| `use_role_obj_sets_role` | use_role_obj sets role on ctx |
| `exit_role_clears_role` | exit_role clears role from ctx |
| `use_prompt_creates_temp_role` | use_prompt creates TEMP_ROLE_NAME role |
| `extract_role_returns_standalone_role` | extract_role returns active role |
| `extract_role_returns_default_when_nothing_active` | extract_role returns default role |

**Total: 17 new tests (69 → 86)**

## Bugs discovered

None. Role parsing behavior matches between old and new code.

## Observations for future iterations

1. `retrieve_role` (which calls `Model::retrieve_model`) can't be
   easily unit-tested without a real client config. It depends on
   having at least one configured client. Deferred to integration
   testing or plan 08 (RequestContext scope transitions).

2. The `use_role` async method (which calls `rebuild_tool_scope`)
   requires async test runtime and MCP infrastructure. Deferred to
   plan 05 (MCP lifecycle) and 08 (RequestContext).

3. `use_role_obj` correctly rejects when agent is active — tested
   implicitly through the error path, but creating a mock Agent
   is complex. Noted for plan 04 (agents).

4. The `extract_role` priority order (session > agent > role > default)
   is important behavioral contract. Tests verify the role and
   default cases. Session and agent cases deferred to plans 03, 04.

5. Added `create_test_ctx()` helper to request_context.rs tests.
   Future iterations should reuse this.

## Plan file updates

Updated 02-roles.md to mark completed items.

## Next iteration

Plan file 03: Sessions — session create/load/save, compression,
autoname, carry-over, exit, context switching.
