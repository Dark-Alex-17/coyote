# Iteration 5 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/05-mcp-lifecycle.md`

## Tests created

### src/config/mcp_factory.rs (12 new tests)

| Test name | What it verifies |
|---|---|
| `key_from_stdio_spec_captures_command_args_env` | McpServerKey extracts command, args, env from stdio spec |
| `key_from_stdio_spec_sorts_args_and_env` | Args and env are sorted for deterministic key hashing |
| `key_from_stdio_spec_defaults_empty_when_none` | None args/env default to empty vecs |
| `key_from_remote_http_spec` | Http transport key captures url and transport type |
| `key_from_remote_sse_spec_with_sorted_headers` | SSE headers sorted for deterministic keys |
| `key_equality_same_spec_produces_equal_keys` | Same spec → equal keys (sharing contract) |
| `key_inequality_different_names` | Different server names → different keys |
| `key_inequality_different_commands` | Different commands → different keys (isolation contract) |
| `key_env_bool_and_int_coerce_to_string` | JsonField::Bool/Int coerced to String in key |
| `factory_try_get_active_returns_none_when_empty` | Empty factory returns None |
| `factory_try_get_active_returns_none_for_unknown_key` | Unknown key returns None |
| `factory_default_has_empty_active_map` | Default factory has empty internal map |

### src/config/tool_scope.rs (6 new tests)

| Test name | What it verifies |
|---|---|
| `mcp_runtime_new_is_empty` | New McpRuntime has no servers |
| `mcp_runtime_default_is_empty` | Default McpRuntime is empty |
| `mcp_runtime_get_returns_none_for_missing_server` | get() on nonexistent server returns None |
| `tool_scope_default_has_empty_mcp_runtime` | Default ToolScope has empty MCP runtime |
| `tool_scope_default_has_empty_functions` | Default ToolScope has no functions |
| `tool_scope_default_tracker_has_no_loops` | Default ToolScope tracker detects no loops |

### src/mcp/mod.rs (30 new tests)

| Test name | What it verifies |
|---|---|
| `validate_stdio_with_command_succeeds` | Valid stdio spec passes |
| `validate_stdio_missing_command_fails` | Stdio without command is rejected |
| `validate_stdio_with_url_fails` | Stdio with url (remote field) is rejected |
| `validate_stdio_with_headers_fails` | Stdio with headers (remote field) is rejected |
| `validate_http_with_url_succeeds` | Valid http spec passes |
| `validate_http_missing_url_fails` | Http without url is rejected |
| `validate_http_with_command_fails` | Http with command (stdio field) is rejected |
| `validate_http_with_args_fails` | Http with args (stdio field) is rejected |
| `validate_http_with_cwd_fails` | Http with cwd (stdio field) is rejected |
| `validate_sse_with_url_succeeds` | Valid SSE spec passes |
| `validate_sse_missing_url_fails` | SSE without url is rejected |
| `is_remote_true_for_http_and_sse` | Http and SSE are remote transports |
| `is_remote_false_for_stdio` | Stdio is not remote |
| `deserialize_stdio_server_from_json` | Full stdio spec from JSON |
| `deserialize_http_server_from_json` | Http spec with headers from JSON |
| `deserialize_env_with_mixed_types` | Env with String, Bool, Int values |
| `deserialize_multiple_servers` | Multiple server entries parsed |
| `deserialize_empty_servers_map` | Empty mcpServers map parsed |
| `deserialize_server_with_cwd` | cwd field parsed correctly |
| `resolve_all_returns_all_configured_servers` | "all" resolves to all config keys |
| `resolve_comma_separated_returns_matching_servers` | Comma-separated list filters correctly |
| `resolve_single_server_name` | Single name resolved |
| `resolve_none_returns_empty` | None enabled → empty list |
| `resolve_no_config_returns_empty` | No config → empty list |
| `resolve_nonexistent_server_filtered_out` | Unknown names silently filtered |
| `resolve_all_nonexistent_returns_empty` | All unknown → empty list |
| `resolve_trims_whitespace` | Whitespace in comma list trimmed |
| `registry_default_is_empty` | Default registry: empty, no config, no log |
| `registry_with_config_reports_config` | Config accessor works |
| `meta_function_prefixes_are_correct` | mcp_invoke/search/describe prefixes |

### src/config/request_context.rs (6 new tests)

| Test name | What it verifies |
|---|---|
| `rebuild_tool_scope_mcp_disabled_skips_servers` | mcp_server_support=false → empty runtime |
| `rebuild_tool_scope_no_enabled_servers_yields_empty_runtime` | None enabled → empty runtime |
| `rebuild_tool_scope_no_mcp_config_yields_empty_runtime` | No mcp_config → empty runtime |
| `rebuild_tool_scope_preserves_tool_tracker` | Tracker survives rebuild |
| `rebuild_tool_scope_repl_mode_appends_user_interaction_functions` | REPL adds user__ functions |
| `rebuild_tool_scope_cmd_mode_no_user_interaction_functions` | CMD skips user__ functions |

**Total: 54 new tests (176 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **ConnectedServer untestable without subprocess**: `ConnectedServer`
   (= `RunningService<RoleClient, ()>`) cannot be constructed without
   a real MCP server subprocess. This blocks unit testing for:
   - McpFactory.acquire() full flow (spawn + insert + Weak sharing)
   - McpRuntime.insert/get with real handles
   - McpRuntime.search/describe/invoke (need live tool catalog)
   - All scope transition tests (role/session/agent MCP start/stop)

   These require integration tests with a mock MCP server binary
   (e.g., a simple echo server). Recommended for a dedicated
   integration test iteration.

2. **McpServerKey sorting guarantees sharing correctness**: The
   sorting of args, env, and headers in McpServerKey::from_spec
   is critical — without it, HashMap key equality would be
   non-deterministic. Tests verify this explicitly.

3. **rebuild_tool_scope has 3 guard clauses that prevent server
   acquisition**: mcp_server_support=false, mcp_config=None,
   enabled_mcp_servers=None. All three paths tested.

4. **REPL vs CMD mode differs in user interaction functions**: The
   `rebuild_tool_scope` method conditionally appends `user__*`
   functions only in REPL mode. Tested both paths.

5. **McpServer::validate enforces strict transport/field separation**:
   Stdio servers cannot have url/headers, remote servers cannot have
   command/args/cwd. This prevents misconfiguration. All cross-field
   conflict cases tested.

6. **McpRegistry.resolve_server_ids is private** but tested via
   `#[cfg(test)]` in the same module. It's the core of server ID
   resolution for "all", comma-separated, and empty cases.

## Next iteration

Plan file 06: Tool Evaluation — eval_tool_calls, ToolCall dispatch,
tool handlers, MCP tool invocation chain (mcp__search, mcp__describe,
mcp__invoke).
