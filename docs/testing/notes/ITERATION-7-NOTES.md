# Iteration 7 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/07-input-construction.md`

## Tests created

### src/config/input.rs (31 new tests)

| Test name | What it verifies |
|---|---|
| `resolve_role_with_explicit_role` | Explicit role returned, with_session/agent false |
| `resolve_role_without_role_no_session_no_agent` | Default role, both flags false |
| `resolve_role_without_role_with_session` | with_session true when session present |
| `resolve_role_explicit_role_overrides_session_flag` | Explicit role forces with_session=false |
| `resolve_paths_detects_last_reply_syntax` | %% sets with_last_reply=true |
| `resolve_paths_detects_url` | https:// classified as remote URL |
| `resolve_paths_detects_external_command` | Backtick-wrapped → external command |
| `resolve_paths_empty_input` | Empty vec → all empty, no last reply |
| `resolve_paths_rejects_url_with_glob_suffix` | URL** → error |
| `resolve_paths_mixed_inputs` | %% + URL + cmd all detected |
| `input_from_str_captures_text` | Text stored correctly |
| `input_from_str_with_explicit_role` | Role name captured |
| `input_from_str_captures_stream_from_config` | stream=false from config |
| `input_is_empty_with_no_text_and_no_medias` | Empty text + no medias = empty |
| `input_is_not_empty_with_text` | Text present = not empty |
| `input_set_text_changes_text` | set_text updates text |
| `input_text_returns_patched_when_set` | Patched text overrides |
| `input_clear_patch_restores_original` | clear_patch removes override |
| `input_set_continue_output_accumulates` | Multiple calls concatenate |
| `input_set_regenerate_sets_flag_and_clears_tool_calls` | Flag set, tool_calls cleared |
| `input_summary_truncates_long_text` | >80 chars → truncated with ... |
| `input_summary_preserves_short_text` | Short text unchanged |
| `input_raw_with_no_files` | Raw returns just text |
| `input_render_with_no_medias` | Render returns just text |
| `input_with_agent_false_when_no_agent` | No agent context → false |
| `input_session_returns_none_when_with_session_false` | Explicit role → no session access |
| `input_session_returns_some_when_with_session_true` | Session context → session access |
| `is_image_recognizes_image_extensions` | png/jpeg/jpg/webp/gif recognized |
| `is_image_rejects_non_image_extensions` | txt/rs/pdf rejected |
| `resolve_data_url_returns_path_for_known_hash` | Hash lookup returns path |
| `resolve_data_url_returns_original_for_non_data_url` | Non-data URL returned as-is |

### src/config/request_context.rs (7 new tests)

| Test name | What it verifies |
|---|---|
| `select_functions_returns_none_when_no_tools_enabled` | No enabled_tools → None |
| `select_functions_returns_none_when_function_calling_disabled` | function_calling_support=false → None |
| `select_functions_all_enabled_tools_returns_all_non_mcp` | "all" → all non-MCP declarations |
| `select_functions_comma_separated_filters` | Comma list → matching subset |
| `select_enabled_mcp_servers_returns_empty_when_mcp_disabled` | mcp_server_support=false → empty |
| `select_enabled_mcp_servers_all_returns_all_mcp_functions` | "all" → all MCP functions |
| `select_enabled_mcp_servers_comma_filters` | Server name → only that server's 3 functions |

**Total: 38 new tests (250 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **Input::from_files is async and I/O-heavy**: It fetches URLs,
   reads files from disk, expands globs, and runs external commands.
   Full testing requires integration tests with temp files/dirs.

2. **resolve_role with agent**: Testing requires an initialized
   Agent (which needs config files on disk). The agent path is
   tested indirectly through the existing `exit_agent` test in
   iteration 4.

3. **resolve_paths is a pure function**: No I/O, fully testable.
   It cleanly separates path classification (URL vs local vs cmd
   vs loader) from actual loading. Good design for testing.

4. **select_functions has complex filtering**: It filters non-MCP
   declarations by enabled_tools, then adds user__ functions for
   non-agent contexts, then merges agent-specific functions. The
   MCP selection mirrors this with MCP-prefixed declarations.
   Both paths fully tested.

5. **Input captures state at construction time**: All fields
   (stream_enabled, session, rag, functions) are captured from
   RequestContext at Input creation. This snapshot-at-creation
   pattern means the Input is independent of later context changes.

6. **The %% syntax for last-reply carry-over** is detected in
   resolve_paths (pure function) but the actual last_reply
   retrieval happens in from_files (async). Tested the detection
   part.

## Next iteration

Plan file 08: Request Context — RequestContext methods, scope
transitions, state management.
