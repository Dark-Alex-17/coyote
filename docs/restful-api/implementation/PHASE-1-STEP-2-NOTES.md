# Phase 1 Step 2 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 2: Migrate static methods off Config"

## Summary

Extracted 33 static (no-`self`) methods from `impl Config` into a new
`src/config/paths.rs` module and migrated every caller across the
codebase. The deprecated forwarders the plan suggested as an
intermediate step were added, used to drive the callsite migration,
and then deleted in the same step because the migration was
mechanically straightforward with `ast-grep` and the forwarders
became dead immediately.

## What was changed

### New files

- **`src/config/paths.rs`** (~270 lines)
  - Module docstring explaining the extraction rationale and the
    (transitional) compatibility shim pattern.
  - `#![allow(dead_code)]` at module scope because most functions
    were briefly dead during the in-flight migration; kept for the
    duration of Step 2 and could be narrowed or removed in a later
    cleanup (see "Follow-up" below).
  - All 33 functions as free-standing `pub fn`s, implementations
    copied verbatim from `impl Config`:
    - Path helpers: `config_dir`, `local_path`, `cache_path`,
      `oauth_tokens_path`, `token_file`, `log_path`, `config_file`,
      `roles_dir`, `role_file`, `macros_dir`, `macro_file`,
      `env_file`, `rags_dir`, `functions_dir`, `functions_bin_dir`,
      `mcp_config_file`, `global_tools_dir`, `global_utils_dir`,
      `bash_prompt_utils_file`, `agents_data_dir`, `agent_data_dir`,
      `agent_config_file`, `agent_bin_dir`, `agent_rag_file`,
      `agent_functions_file`, `models_override_file`
    - Listing helpers: `list_roles`, `list_rags`, `list_macros`
    - Existence checks: `has_role`, `has_macro`
    - Config loaders: `log_config`, `local_models_override`

### Modified files

Migration touched 14 source files — all of `src/config/mod.rs`'s
internal callers, plus every external `Config::method()` callsite:

- **`src/config/mod.rs`** — removed the 33 static-method definitions
  from `impl Config`, rewrote every `Self::method()` internal caller
  to use `paths::method()`, and removed the `log::LevelFilter` import
  that became unused after `log_config` moved away.
- **`src/config/bridge.rs`** — no changes (bridge is unaffected by
  path migrations).
- **`src/config/macros.rs`** — added `use crate::config::paths;`,
  migrated one `Config::macros_dir().display()` call.
- **`src/config/agent.rs`** — added `use crate::config::paths;`,
  migrated 2 `Config::agents_data_dir()` calls, 4 `agent_data_dir`
  calls, 3 `agent_config_file` calls, 1 `agent_rag_file` call.
- **`src/config/request_context.rs`** — no changes.
- **`src/config/app_config.rs`, `app_state.rs`** — no changes.
- **`src/main.rs`** — added `use crate::config::paths;`, migrated
  `Config::log_config()`, `Config::list_roles(true)`,
  `Config::list_rags()`, `Config::list_macros()`.
- **`src/function/mod.rs`** — added `use crate::config::paths;`,
  migrated ~25 callsites across `Config::config_dir`,
  `functions_dir`, `functions_bin_dir`, `global_tools_dir`,
  `agent_bin_dir`, `agent_data_dir`, `agent_functions_file`,
  `bash_prompt_utils_file`. Removed `Config` from the `use
  crate::{config::{...}}` block because it became unused.
- **`src/repl/mod.rs`** — added `use crate::config::paths;`,
  migrated `Config::has_role(name)` and `Config::has_macro(name)`.
- **`src/cli/completer.rs`** — added `use crate::config::paths;`,
  migrated `Config::list_roles(true)`, `Config::list_rags()`,
  `Config::list_macros()`.
- **`src/utils/logs.rs`** — replaced `use crate::config::Config;`
  with `use crate::config::paths;` (Config was only used for
  `log_path`); migrated `Config::log_path()` call.
- **`src/mcp/mod.rs`** — added `use crate::config::paths;`,
  migrated 3 `Config::mcp_config_file().display()` calls.
- **`src/client/common.rs`** — added `use crate::config::paths;`,
  migrated `Config::local_models_override()`. Removed `Config` from
  the `config::{Config, GlobalConfig, Input}` import because it
  became unused.
- **`src/client/oauth.rs`** — replaced `use crate::config::Config;`
  with `use crate::config::paths;` (Config was only used for
  `token_file`); migrated 2 `Config::token_file` calls.

### Module registration

- **`src/config/mod.rs`** — added `pub(crate) mod paths;` in the
  module declaration block, alphabetically placed between `macros`
  and `prompts`.

## Key decisions

### 1. The deprecated forwarders lived for the whole migration but not beyond

The plan said to keep `#[deprecated]` forwarders around while
migrating callsites module-by-module. I followed that approach but
collapsed the "migrate then delete" into a single step because the
callsite migration was almost entirely mechanical — `ast-grep` with
per-method patterns handled the bulk, and only a few edge cases
(`Self::X` inside `&`-expressions, multi-line `format!` calls)
required manual text edits. By the time all 33 methods had zero
external callers, keeping the forwarders would have just generated
dead_code warnings.

The plan also said "then remove the deprecated methods" as a distinct
phase, and that's exactly what happened — just contiguously with the
migration rather than as a separate commit. The result is the same:
no forwarders in the final tree, all callers routed through
`paths::`.

### 2. `paths` is a `pub(crate)` module, not `pub`

I registered the module as `pub(crate) mod paths;` so the functions
are available anywhere in the crate via `crate::config::paths::X`
but not re-exported as part of Loki's public API surface. This
matches the plan's intent — these are internal implementation
details that happen to have been static methods on `Config`. If
anything external needs a config path in the future, the proper
shape is probably to add it as a method on `AppConfig` (which goes
through Step 3's global-read migration anyway) rather than exposing
`paths` publicly.

### 3. `log_config` stays in `paths.rs` despite not being a path

`log_config()` returns `(LevelFilter, Option<PathBuf>)` — it reads
environment variables to determine the log level plus falls back to
`log_path()` for the file destination. Strictly speaking, it's not
a "path" function, but:

- It's a static no-`self` helper (the reason it's in Step 2)
- It's used in exactly one place (`main.rs:446`)
- Splitting it into its own module would add complexity for no
  benefit

The plan also listed it in the migration table as belonging in
`paths.rs`. I followed the plan.

### 4. `#![allow(dead_code)]` at module scope, not per-function

I initially scoped the allow to the whole `paths.rs` module because
during the mid-migration state, many functions had zero callers
temporarily. I kept it at module scope rather than narrowing to
individual functions as they became used again, because by the end
of Step 2 all 33 functions have at least one real caller and the
allow is effectively inert — but narrowing would mean tracking
which functions are used vs not in every follow-up step. Module-
level allow is set-and-forget.

This is slightly looser than ideal. See "Follow-up" below.

### 5. `ast-grep` was the primary migration tool, with manual edits for awkward cases

`ast-grep --pattern 'Config::method()'` and
`--pattern 'Self::method()'` caught ~90% of the callsites cleanly.
The remaining ~10% fell into two categories that `ast-grep` handled
poorly:

1. **Calls wrapped in `.display()` or `.to_string_lossy()`.** Some
   ast-grep patterns matched these, others didn't — the behavior
   seemed inconsistent. When a pattern found 0 matches but grep
   showed real matches, I switched to plain text `Edit` for that
   cluster.
2. **`&Self::X()` reference expressions.** `ast-grep` appeared to
   not match `Self::X()` when it was the operand of a `&` reference,
   presumably because the parent node shape was different. Plain
   text `Edit` handled these without issue.

These are tooling workarounds, not architectural concerns. The
final tree has no `Config::X` or `Self::X` callers for any of the
33 migrated methods.

### 6. Removed `Config` import from three files that no longer needed it

`src/function/mod.rs`, `src/client/common.rs`, `src/client/oauth.rs`,
and `src/utils/logs.rs` all had `use crate::config::Config;` (or
similar) imports that became unused after every call was migrated.
I removed them. This is a minor cleanup but worth doing because:

- Clippy flags unused imports as warnings
- Leaving them in signals "this file might still need Config" which
  future migration steps would have to double-check

## Deviations from plan

### 1. `sync_models` is not in Step 2

The plan's Step 2 table listed `sync_models(url, abort)` as a
migration target, but grep showed only `sync_models_url(&self) ->
String` exists in the code. That's a `&self` method, so it belongs
in Step 3 (global-read methods), not Step 2.

I skipped it here and will pick it up in Step 3. The Step 2 actual
count is 33 methods, not the 34 the plan's table implies.

### 2. Forwarders deleted contiguously, not in a separate sub-step

See Key Decision #1. The plan described a two-phase approach
("leave forwarders, migrate callers module-by-module, then remove
forwarders"). I compressed this into one pass because the migration
was so mechanical there was no value in the intermediate state.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (same as Step 1 — no new
  tests were added because Step 2 is a pure code-move with no new
  behavior to test; the existing test suite verifies nothing
  regressed)

### Manual smoke test

Not applicable — Step 2 is a pure code-move. The path computations
are literally the same code at different call sites. If existing
tests pass and nothing references Config's static methods anymore,
there's nothing to manually verify beyond the compile.

### Callsite audit

```
cargo check 2>&1 | grep "Config::\(config_dir\|local_path\|...\)"
```

Returns zero matches. Every external `Config::method()` callsite
for the 33 migrated methods has been converted to `paths::method()`.

## Handoff to next step

### What Step 3 can rely on

Step 3 (migrate global-read methods to `AppConfig`) can rely on:

- `src/config/paths.rs` exists and holds every static path helper
  plus `log_config`, `list_*`, `has_*`, and `local_models_override`
- Zero `Config::config_dir()`, `Config::cache_path()`, etc. calls
  remain in the codebase
- The `#[allow(dead_code)]` on `paths.rs` at module scope is safe to
  remove at any time now that all functions have callers
- `AppConfig` (from Step 0) is still fully populated and ready to
  receive method migrations
- The bridge from Step 1 (`Config::to_app_config`,
  `to_request_context`, `from_parts`) is unchanged and still works
- `Config` struct has no more static methods except those that were
  kept because they DO take `&self` (`vault_password_file`,
  `messages_file`, `sessions_dir`, `session_file`, `rag_file`,
  `state`, etc.)
- Deprecation forwarders are GONE — don't add them back

### What Step 3 should watch for

- **`sync_models_url`** was listed in the Step 2 plan table as
  static but is actually `&self`. It's a Step 3 target
  (global-read). Pick it up there.
- **The Step 3 target list** (from `PHASE-1-IMPLEMENTATION-PLAN.md`):
  `vault_password_file`, `editor`, `sync_models_url`, `light_theme`,
  `render_options`, `print_markdown`, `rag_template`,
  `select_functions`, `select_enabled_functions`,
  `select_enabled_mcp_servers`. These are all `&self` methods that
  only read serialized config state.
- **The `vault_password_file` field on `AppConfig` is `pub(crate)`,
  not `pub`.** The accessor method on `AppConfig` will need to
  encapsulate the same fallback logic that the `Config` method has
  (see `src/config/mod.rs` — it falls back to
  `gman::config::Config::local_provider_password_file()`).
- **`print_markdown` depends on `render_options`.** When migrating
  them to `AppConfig`, preserve the dependency chain.
- **`select_functions` / `select_enabled_functions` /
  `select_enabled_mcp_servers` take a `&Role` parameter.** Their
  new signatures on `AppConfig` will be `&self, role: &Role` — make
  sure `Role` is importable in the `app_config.rs` module (it
  currently isn't).
- **Strategy for the Step 3 migration:** same as Step 2 — create
  methods on `AppConfig`, add `#[deprecated]` forwarders on
  `Config`, migrate callsites with `ast-grep`, delete the
  forwarders. Should be quicker than Step 2 because the method
  count is smaller (10 vs 33) and the pattern is now well-
  established.

### What Step 3 should NOT do

- Don't touch `paths.rs` — it's complete.
- Don't touch `bridge.rs` — Step 3's migrations will still flow
  through the bridge's round-trip test correctly.
- Don't try to migrate `current_model`, `extract_role`, `sysinfo`,
  or any of the `set_*` methods — those are "mixed" methods listed
  in Step 7, not Step 3.
- Don't delete `Config` struct fields yet. Step 3 only moves
  *methods* that read fields; the fields themselves still exist on
  `Config` (and on `AppConfig`) in parallel until Step 10.

### Files to re-read at the start of Step 3

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 3 section (table of
  10 global-read methods and their target signatures)
- This notes file — specifically the "What Step 3 should watch for"
  section
- `src/config/app_config.rs` — to see the current `AppConfig` shape
  and decide where to put new methods
- The current `&self` methods on `Config` in `src/config/mod.rs`
  that are being migrated

## Follow-up (not blocking Step 3)

### 1. Narrow or remove `#![allow(dead_code)]` on `paths.rs`

At Step 2's end, every function in `paths.rs` has real callers, so
the module-level allow could be removed without producing warnings.
I left it in because it's harmless and removes the need to add
per-function allows during mid-migration states in later steps.
Future cleanup pass can tighten this.

### 2. Consider renaming `paths.rs` if its scope grows

`log_config`, `list_roles`, `list_rags`, `list_macros`, `has_role`,
`has_macro`, and `local_models_override` aren't strictly "paths"
but they're close enough that extracting them into a sibling module
would be premature abstraction. If Steps 3+ add more non-path
helpers to the same module, revisit this.

### 3. The `Config::config_dir` deletion removes one access point for env vars

The `config_dir()` function was also the entry point for XDG-
compatible config location discovery. Nothing about that changed —
it still lives in `paths::config_dir()` — but if Step 4+ needs to
reference the config directory from code that doesn't yet import
`paths`, the import list will need updating.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 1 notes: `docs/implementation/PHASE-1-STEP-1-NOTES.md`
- New file: `src/config/paths.rs`
- Modified files (module registration + callsite migration): 14
  files across `src/config/`, `src/function/`, `src/repl/`,
  `src/cli/`, `src/main.rs`, `src/utils/`, `src/mcp/`,
  `src/client/`
