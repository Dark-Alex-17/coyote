# Iteration 9 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/09-repl-commands.md`

## Tests created

### src/config/mod.rs (8 new tests)

| Test name | What it verifies |
|---|---|
| `assert_state_pass_always_true` | pass() true for all flag combos |
| `assert_state_bare_only_empty` | bare() only matches empty |
| `assert_state_true_requires_flag_present` | True requires any match |
| `assert_state_true_with_multiple_flags_any_match` | OR semantics for True flags |
| `assert_state_false_requires_flag_absent` | False requires all absent |
| `assert_state_false_with_multiple_flags` | Multiple False flags all checked |
| `assert_state_truefalse_requires_true_present_and_false_absent` | Both conditions |
| `assert_state_equal_exact_match` | Exact flag equality |

### src/repl/mod.rs (31 new tests, 33 total in file)

| Test name | What it verifies |
|---|---|
| `repl_commands_has_39_entries` | Array size |
| `repl_commands_all_start_with_dot` | All commands dotted |
| `repl_commands_no_empty_descriptions` | All have descriptions |
| `repl_commands_help_is_always_available` | .help → pass |
| `repl_commands_exit_is_always_available` | .exit → pass |
| `repl_commands_info_role_requires_role` | .info role → True(ROLE) |
| `repl_commands_session_blocked_when_already_in_session` | .session → False(SESSION) |
| `repl_commands_exit_session_requires_session` | .exit session → True(SESSION) |
| `repl_commands_exit_agent_requires_agent` | .exit agent → True(AGENT) |
| `repl_commands_agent_only_when_bare` | .agent → Equal(empty) |
| `repl_commands_role_blocked_in_session_or_agent` | .role → False(SESSION\|AGENT) |
| `repl_commands_prompt_blocked_in_session_or_agent` | .prompt → False(SESSION\|AGENT) |
| `repl_commands_rag_blocked_in_agent` | .rag → False(AGENT) |
| `repl_commands_starter_requires_agent` | .starter → True(AGENT) |
| `repl_commands_clear_todo_requires_agent` | .clear todo → True(AGENT) |
| `repl_commands_edit_role_requires_role_not_session` | .edit role → TrueFalse |
| `repl_commands_exit_rag_requires_rag_not_agent` | .exit rag → TrueFalse |
| `parse_command_plain_text_returns_none` | Plain text → None |
| `parse_command_empty_returns_none` | Empty → None |
| `parse_command_whitespace_only_returns_none` | Whitespace → None |
| `parse_command_dot_only` | Single dot → (".", None) |
| `split_first_arg_none_input` | None → None |
| `split_first_arg_single_word` | "role" → ("role", None) |
| `split_first_arg_two_words` | "role x" → ("role", Some("x")) |
| `split_first_arg_with_extra_spaces` | Extra spaces trimmed |
| `repl_command_is_valid_pass_always_true` | pass → always valid |
| `repl_command_is_valid_respects_true` | True → enforced |
| `repl_command_is_valid_respects_false` | False → enforced |
| `multiline_regex_captures_content_between_markers` | :::content::: captured |
| `multiline_regex_does_not_match_single_marker` | Unclosed → no match |
| `multiline_regex_does_not_match_plain_text` | Plain text → no match |

**Total: 39 new tests (311 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **AssertState has 4 variants with distinct semantics**:
   - True: any of the required flags must be present (OR)
   - False: all of the forbidden flags must be absent (AND)
   - TrueFalse: True AND False simultaneously
   - Equal: exact flag match
   This is a critical invariant for REPL command availability.

2. **The .agent command uses AssertState::bare()** (Equal(empty)),
   meaning it's only available when NO other scope is active. This
   is stricter than False — it requires exactly empty state.

3. **All 39 REPL commands** have correct dot prefixes and non-empty
   descriptions. Verified as structural invariants.

4. **The multiline ::: syntax** is handled by a regex that requires
   both opening and closing markers. The ReplValidator marks
   single-marker input as Incomplete for the line editor.

5. **Command handler tests** (the actual .role, .session, .agent
   implementations) require full async RequestContext with
   filesystem access. These are integration tests and are deferred.

## Next iteration

Check the TEST-IMPLEMENTATION-PLAN.md for what plan file comes next.
