# Iteration 6 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/06-tool-evaluation.md`

## Tests created

### src/function/mod.rs (36 new tests)

| Test name | What it verifies |
|---|---|
| `toolcall_new_sets_fields` | ToolCall::new sets name, arguments, id |
| `toolcall_default_has_empty_fields` | Default ToolCall has empty/null fields |
| `toolcall_with_thought_signature` | with_thought_signature sets value |
| `toolcall_with_thought_signature_none` | with_thought_signature(None) clears |
| `dedup_removes_duplicate_ids_keeps_last` | Duplicate ids → last occurrence kept |
| `dedup_keeps_unique_ids` | Unique ids → all kept |
| `dedup_keeps_calls_without_ids` | No-id calls always kept |
| `dedup_preserves_last_occurrence_order` | Ordering based on last occurrence position |
| `dedup_empty_input_returns_empty` | Empty vec → empty result |
| `dedup_mixed_with_and_without_ids` | Mixed id/no-id dedup behavior |
| `tracker_default_values` | Default max_repeats=2, chain_len=3 |
| `tracker_no_loop_on_fresh_tracker` | Fresh tracker returns None |
| `tracker_no_loop_below_threshold` | Below max_repeats → no loop |
| `tracker_detects_loop_at_max_repeats` | At max_repeats → loop detected |
| `tracker_different_args_no_loop` | Different args break loop detection |
| `tracker_different_names_no_loop` | Different names break loop detection |
| `tracker_chain_detection` | Chain of identical calls detected |
| `tracker_record_call_respects_capacity` | Capacity bounded by chain_len * max_repeats |
| `tracker_loop_message_contains_call_history` | Loop message includes call_history JSON |
| `prefix_constants_are_correct` | All 6 prefixes: todo__, agent__, user__, mcp_invoke/search/describe |
| `functions_default_is_empty` | Default Functions has no declarations |
| `functions_append_todo_adds_declarations` | 5 todo tools: init, add, done, list, clear |
| `functions_append_supervisor_adds_declarations` | Supervisor: spawn, check, collect, list, cancel, reply |
| `functions_append_teammate_adds_declarations` | Teammate: send_message, check_inbox |
| `functions_append_user_interaction_adds_declarations` | User: ask, confirm, input, checkbox |
| `functions_append_mcp_meta_creates_three_per_server` | 3 MCP meta functions per server |
| `functions_append_mcp_meta_multiple_servers` | Multiple servers → 3 each |
| `functions_append_mcp_meta_empty_servers` | Empty servers → no declarations |
| `functions_find_returns_declaration` | find() returns matching declaration |
| `functions_find_returns_none_for_missing` | find() returns None for unknown |
| `functions_contains_true_for_existing` | contains() true for known function |
| `functions_contains_false_for_missing` | contains() false for unknown |
| `functions_mcp_invoke_declaration_has_tool_and_arguments_params` | Invoke schema: tool + arguments params |
| `functions_mcp_search_declaration_has_query_and_top_k_params` | Search schema: query + top_k params |
| `functions_mcp_describe_declaration_has_tool_param` | Describe schema: tool param |
| `functions_supervisor_includes_task_queue_tools` | Task queue: create, list, complete, fail |
| `tool_result_stores_call_and_output` | ToolResult::new stores both fields |

**Total: 36 new tests (212 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **ToolCall::dedup keeps the LAST occurrence**: The implementation
   iterates in reverse and reverses again, so when duplicate ids
   exist, the last occurrence wins. My initial tests assumed first-
   wins behavior — caught and corrected during the iteration.

2. **ToolCall::eval requires full RequestContext**: The dispatch
   routing (`agent__*`, `todo__*`, `user__*`, `mcp_*`, shell
   fallback) cannot be unit-tested because `eval()` takes
   `&mut RequestContext` which requires an initialized AppState.
   The prefix routing is verified indirectly through prefix
   constant tests and function declaration tests.

3. **Functions::init requires filesystem**: It calls
   `build_global_tool_declarations` which reads tool files from
   disk. Can't unit-test without a temp directory with actual
   tool scripts. Function filtering by `enabled_tools` is thus
   deferred.

4. **All function declaration appenders are fully testable**: The
   `append_*` methods on Functions work without I/O and produce
   the exact function declarations the LLM sees. This is the most
   important behavioral contract to test.

5. **MCP meta function schemas are critical**: The invoke, search,
   and describe meta functions each have specific parameter schemas
   (tool+arguments, query+top_k, tool). Tests verify these schemas
   exist with correct fields and required params.

6. **ToolCallTracker loop detection has two mechanisms**:
   - Consecutive repeat detection (same call N times in a row)
   - Chain detection (same call repeated across the last chain_len
     entries)
   Both are tested independently.

## Next iteration

Plan file 07: Input Construction — Input::from_str, from_files,
field capturing, function selection.
