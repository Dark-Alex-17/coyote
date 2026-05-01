# Iteration 10 — Test Implementation Notes

## Plan files addressed

- `docs/testing/plans/09-repl-commands.md` (completed in same session)
- `docs/testing/plans/10-cli-flags.md`

## Tests created

### src/config/mod.rs (8 new tests — iteration 9)

AssertState::assert tests for all 4 variants + pass/bare.

### src/repl/mod.rs (31 new tests — iteration 9)

REPL_COMMANDS array validation, command state assertions for 13
specific commands, parse_command edge cases, split_first_arg,
ReplCommand::is_valid, multiline regex.

### src/cli/mod.rs (31 new tests — iteration 10)

| Test name | What it verifies |
|---|---|
| `parse_no_args_defaults` | All flags default unset |
| `parse_model_flag` | --model value |
| `parse_model_short_flag` | -m value |
| `parse_role_flag` | --role value |
| `parse_session_with_name` | --session value |
| `parse_agent_flag` | --agent value |
| `parse_agent_short_flag` | -a value |
| `parse_execute_flag` | -e flag |
| `parse_code_flag` | -c flag |
| `parse_no_stream_flag` | -S flag |
| `parse_dry_run_flag` | --dry-run flag |
| `parse_info_flag` | --info flag |
| `parse_list_flags` | All 6 --list-* flags |
| `parse_file_flag_single` | Single -f |
| `parse_file_flag_multiple` | Multiple -f accumulate |
| `parse_trailing_text` | Trailing args as text vec |
| `parse_prompt_flag` | --prompt value |
| `parse_empty_session_flag` | --empty-session flag |
| `parse_save_session_flag` | --save-session flag |
| `parse_build_tools_flag` | --build-tools flag |
| `parse_sync_models_flag` | --sync-models flag |
| `parse_model_with_role` | -m + -r combined |
| `parse_agent_with_file_and_text` | -a + -f + text combined |
| `parse_role_with_session` | -r + -s combined |
| `cli_text_returns_none_when_no_text_no_stdin` | No input → None |
| `cli_text_joins_trailing_args` | Args joined with spaces |
| `parse_add_secret_flag` | --add-secret value |
| `parse_get_secret_flag` | --get-secret value |
| `parse_list_secrets_flag` | --list-secrets flag |
| `parse_rag_flag` | --rag value |
| `parse_macro_flag` | --macro value |

**Total: 70 new tests across iterations 9+10 (342 total in suite)**

## Bugs discovered

None.

## Observations for future iterations

1. **Clap parsing is fully testable**: Using `try_parse_from` with
   synthetic arg arrays, all flag parsing and combinations can be
   verified without running the actual binary.

2. **Cli::text() has stdin dependency**: When stdin is not a
   terminal, it reads from stdin. This branch can't be easily
   unit-tested. The terminal-detection branch (no stdin) is tested.

3. **Prelude is async + filesystem**: apply_prelude needs real role
   and session files. Deferred to integration tests.

4. **Mode selection is runtime behavior**: The actual mode branching
   (REPL vs CMD) happens in main.rs based on parsed flags. Testing
   the flag parsing verifies the inputs to that branching logic.

5. **Exclusive flags**: Vault flags (--add-secret, --get-secret,
   etc.) are marked `exclusive = true` in clap, meaning they
   can't be combined with other args. This is enforced by clap.

## Next iteration

Plan file 11: Sub-Agent Spawning — supervisor, child agents,
escalation, messaging.
