# Iteration 13 — Test Implementation Notes

## Plan files addressed

- `docs/testing/plans/12-rag.md` (completed in same session)
- `docs/testing/plans/13-completions-and-prompt.md`
- `docs/testing/plans/14-macros.md`
- `docs/testing/plans/15-vault.md`
- `docs/testing/plans/16-functions-and-tools.md`

## Tests created

### src/rag/mod.rs (22 new tests — iteration 12)

DocumentId round-trip/equality/ordering/debug, RagDocument new/default,
RagData new/get/del/add/build_bm25, RAG_TEMPLATE placeholders,
get_separators language mapping.

### src/config/macros.rs (21 new tests — iteration 13)

| Test name | What it verifies |
|---|---|
| `resolve_no_variables` | Empty vars → empty output |
| `resolve_required_variable_provided` | Arg maps to variable |
| `resolve_required_variable_missing_errors` | Missing required → error |
| `resolve_default_variable_uses_default` | Default used when no arg |
| `resolve_default_variable_overridden` | Arg overrides default |
| `resolve_rest_variable_captures_all_remaining` | Rest joins remaining args |
| `resolve_rest_variable_with_default` | Rest default used |
| `resolve_multiple_variables` | Mixed required + default |
| `usage_no_variables` | Just macro name |
| `usage_required_variable` | <name> format |
| `usage_optional_variable` | [name] format |
| `usage_rest_variable` | <name>... format |
| `usage_rest_with_default` | [name]... format |
| `usage_mixed_variables` | Mixed format |
| `interpolate_replaces_variables` | {{name}} → value |
| `interpolate_multiple_variables` | Multiple replacements |
| `interpolate_no_variables_passthrough` | No vars → unchanged |
| `interpolate_variable_not_found_left_as_is` | Missing var → {{name}} kept |
| `deserialize_macro_from_yaml` | Full YAML with steps + variables |
| `deserialize_macro_with_defaults` | Variables with defaults + rest |
| `deserialize_macro_no_variables` | Steps only, empty vars default |

### src/vault/mod.rs (6 new tests)

| Test name | What it verifies |
|---|---|
| `secret_re_matches_double_braces` | {{MY_SECRET}} captured |
| `secret_re_matches_with_surrounding_text` | Captures in context |
| `secret_re_no_match_single_braces` | {NOT} not matched |
| `secret_re_no_match_plain_text` | No match for plain text |
| `secret_re_matches_with_spaces` | {{ SPACED }} captured |
| `vault_default_creates_instance` | Default has no password file |

### src/parsers/common.rs (8 new tests)

| Test name | What it verifies |
|---|---|
| `underscore_simple` | No-op for simple names |
| `underscore_dashes_to_underscores` | my-func → my_func |
| `underscore_spaces_to_underscores` | my func → my_func |
| `underscore_special_chars_removed` | @! → _ |
| `underscore_consecutive_specials_collapsed` | --- → single _ |
| `underscore_leading_trailing_stripped` | -name- → name |
| `underscore_uppercase_lowered` | MyFunc → myfunc |
| `underscore_mixed` | Get-User Info → get_user_info |

**Total: 57 new tests across iterations 12+13 (475 total in suite)**

## Bugs discovered

None.

## Observations

1. **Macro::resolve_variables has 3 variable modes**: required
   (no default), optional (with default), and rest (captures
   remaining args). All three modes tested with multiple
   combinations.

2. **Macro::interpolate_command is a simple string replacement**:
   {{key}} → value. Missing keys are left as-is (no error),
   which is the correct behavior for gradual interpolation.

3. **SECRET_RE uses fancy_regex**: The `{{(.+)}}` pattern requires
   double braces. Single braces don't match, which prevents false
   positives on JSON-like content.

4. **Vault operations all require terminal interaction or password
   file**: add_secret and update_secret prompt for passwords via
   inquire. get_secret/delete_secret/list_secrets need a tokio
   runtime + password file. These are integration-test territory.

5. **parsers::common::underscore is more than s/-/_/**: It lowercases,
   replaces all non-alphanumeric chars with _, collapses consecutive
   underscores, and strips leading/trailing underscores. Thorough
   edge cases tested.

6. **Python and TypeScript parsers have excellent existing test
   suites**: ~400 lines of tests each covering declaration parsing,
   type inference, docstring extraction. No additional tests needed.

## Final summary

All 16 plan files have been addressed across iterations 1-13.
475 total tests, all passing, 0 errors.
