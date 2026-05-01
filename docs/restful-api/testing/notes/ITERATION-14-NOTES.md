# Iteration 14 — Integration Test Implementation Notes

## Focus

Filesystem-based integration tests (Tier 1 + Tier 2) for behaviors
that were previously untestable without real config directories.

## Infrastructure changes

1. **Added `serial_test` dev-dependency** — Env-var-based config dir
   isolation (`TestConfigDirGuard`) requires serialization to prevent
   parallel test races. All 25 tests using `TestConfigDirGuard` now
   use `#[serial]`.

2. **Added `src/test_helpers.rs`** — Shared test utilities module
   (`#[cfg(test)]`) with `TestConfigDirGuard`, `default_app_state`,
   `create_test_ctx`, and `run_async` helpers, available to all
   modules. Not yet used by all modules (existing module-local
   helpers kept for backward compatibility).

## Tests created

### src/config/request_context.rs (17 new integration tests)

| Test name | What it verifies |
|---|---|
| `retrieve_role_from_markdown_file` | Writes .md file, retrieves role with correct name/prompt |
| `retrieve_role_builtin_exists` | Built-in roles retrievable |
| `retrieve_role_nonexistent_errors` | Unknown role → error |
| `retrieve_role_no_model_id_inherits_current_model` | No model_id → uses current model |
| `list_roles_finds_markdown_files` | .md files listed, .txt ignored |
| `list_roles_empty_dir` | Empty roles dir → empty list |
| `session_new_from_ctx_captures_state` | Name captured, starts empty |
| `session_save_creates_file` | Save creates YAML file on disk |
| `use_session_errors_when_already_in_session` | Double session → error |
| `use_session_creates_temp_session` | None → temp session |
| `use_session_creates_named_session` | Name → named session |
| `exit_session_roundtrip` | use_session → exit_session → None |
| `use_role_obj_and_exit_role_full_cycle` | Set role → exit → None |
| `use_role_obj_twice_replaces_role` | Second role replaces first |
| `list_macros_finds_yaml_files` | .yaml macro files listed |
| `list_rags_finds_yaml_files` | .yaml RAG files listed |
| `list_rags_empty_dir` | Empty RAGs dir → empty list |

### src/config/input.rs (5 new integration tests)

| Test name | What it verifies |
|---|---|
| `from_files_loads_single_text_file` | File content + text combined |
| `from_files_loads_multiple_files` | Multiple files all loaded |
| `from_files_with_no_paths_just_text` | No files → just text |
| `from_files_with_external_command` | Backtick command executed |
| `from_files_nonexistent_file_errors` | Missing file → error |

### Serialization fixes (6 existing tests)

Added `#[serial]` to all `rebuild_tool_scope_*` tests to prevent
env-var race conditions with filesystem integration tests.

**Total: 22 new tests (497 total in suite)**

## Bugs discovered

1. **Test parallelism race condition with env vars**: The
   `TestConfigDirGuard` sets a process-global env var. When tests
   run in parallel, two guards stomp each other's values. Fixed
   by adding `serial_test` crate and `#[serial]` attribute to all
   filesystem-dependent tests.

## Observations

1. **Session loading from disk requires Model::retrieve_model**:
   `Session::load_from_ctx` calls `Model::retrieve_model` to
   resolve the session's model_id. Without a valid model provider
   config, this fails. Session loading tests are limited to
   `new_from_ctx` (creation) and `save` (serialization).

2. **use_session with empty session prompts user**: The Confirm
   dialog for "incorporate last Q&A?" requires terminal interaction.
   Tests avoid this by: (a) having no last_message, or (b) using
   named sessions that already exist on disk.

3. **Input::from_files with external commands works**: The backtick
   syntax (`\`echo hello\``) actually runs the command and captures
   output. This is a real integration test — it runs `/bin/echo`.

4. **Vault CRUD was skipped**: Vault operations require a password
   file with actual encrypted content via the `gman` crate's
   `LocalProvider`. The `add_secret` method also prompts for a
   password via `inquire`. Testing vault requires either mocking
   the terminal or using `LocalProvider` directly with a pre-created
   password file — deferred to a future iteration.

## Final counts

| Category | Tests |
|---|---|
| Unit tests (iterations 1-13) | 475 |
| Integration tests (iteration 14) | 22 |
| **Total** | **497** |
