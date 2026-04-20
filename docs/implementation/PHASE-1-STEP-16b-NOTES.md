# Phase 1 Step 16b — Implementation Notes

## Status

Done.

## Plan reference

- Parent plan: `docs/implementation/PHASE-1-STEP-16-NOTES.md`
- Sub-phase goal: "Extract `install_builtins()` as top-level function"

## Summary

Extracted `Agent::install_builtin_agents()` and `Macro::install_macros()`
from inside `Config::init` into a new top-level free function
`config::install_builtins()`. Called once from `main.rs` before any
config-loading path.

Both functions are Config-independent — they just copy embedded
agent/macro assets from the binary into the user's config directory.
Extracting them clears the way for `Config::init`'s eventual
deletion in Step 16f.

## What was changed

### `src/config/mod.rs`

**Added:**
```rust
pub fn install_builtins() -> Result<()> {
    Agent::install_builtin_agents()?;
    Macro::install_macros()?;
    Ok(())
}
```

Placed after the `Config::Default` impl, before the `impl Config`
block. Module-level `pub fn` (not a method on any type).

**Removed from `Config::init` (inside the async `setup` closure):**
- `Agent::install_builtin_agents()?;` (was at top of setup block)
- `Macro::install_macros()?;` (was at bottom of setup block)

### `src/main.rs`

**Added:**
- `install_builtins` to the `use crate::config::{...}` import list
- `install_builtins()?;` call after `setup_logger()?` and before any
  of the three config-loading paths (oauth, vault flags, main config)

### Placement rationale

The early-return paths (`cli.completions`, `cli.tail_logs`)
legitimately don't need builtins — they return before touching any
config. Those skip the install.

The three config paths (oauth via `Config::init_bare`, vault flags
via `Config::init_bare`, main via `Config::init`) all benefit from
builtins being installed once at startup. `install_builtins()` is
idempotent — it checks file existence and skips if already present —
so calling it unconditionally in the common path is safe.

## Behavior parity

- `install_builtin_agents` and `install_macros` are static methods
  with no `&self` or Config arguments. Nothing observable changes
  about their execution.
- The two functions ran on every `Config::init` call before. Now
  they run once per `main.rs` invocation, which is equivalent for
  the REPL and CLI paths.
- `Config::init_bare()` no longer triggers the installs
  transitively. The oauth and vault-flag paths now rely on `main.rs`
  having called `install_builtins()?` first. This is a minor
  behavior shift — those paths previously installed builtins as a
  side effect of calling `Config::init_bare`. Since we now call
  `install_builtins()` unconditionally in `main.rs` before those
  paths, the observable behavior is identical.

## Files modified

- `src/config/mod.rs` — added `install_builtins()` free function;
  removed 2 calls from `Config::init`.
- `src/main.rs` — added import; added `install_builtins()?` call
  after logger setup.

## Assumptions made

1. **`install_builtins` should always run unconditionally.** Even
   if the user is only running `--completions` or `--tail-logs`
   (early-return paths), those return before the install call.
   The three config-using paths all benefit from it. No downside to
   running it early.

2. **Module-level `pub fn` is the right API surface.** Could have
   made it a method on `AppState` or `AppConfig`, but:
   - It's called before any config/state exists
   - It has no `self` parameter
   - It's a static side-effectful operation (filesystem)
   A free function at the module level is the honest signature.

3. **No tests added.** `install_builtins` is a thin wrapper around
   two side-effectful functions that write files. Testing would
   require filesystem mocking or temp dirs, which is
   disproportionate for a 3-line function. The underlying
   `install_builtin_agents` and `install_macros` functions have
   existing behavior in the codebase; the extraction doesn't change
   their contracts.

## Open questions

1. **Should `install_builtins` accept a "skip install" flag?**
   Currently it always runs. For a server/REST API deployment, you
   might want to skip this to avoid writing to the user's config
   dir at startup. Deferring this question until REST API path
   exists — can add a flag or a `_skip_install()` variant later.

2. **Do CI/test environments break because of the filesystem write?**
   The install functions already existed in the codebase and ran on
   every Config::init. No new risk introduced. Watch for flaky
   tests after this change, but expected clean.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean, zero warnings
- `cargo test` — 122 passing, zero failures
- Grep confirmation:
  - `install_builtin_agents` only defined in `src/config/agent.rs`
    and called only via `install_builtins`
  - `install_macros` only defined in `src/config/macros.rs` and
    called only via `install_builtins`
  - `install_builtins` has one caller (`main.rs`)

## Remaining work for Step 16

- **16c**: Migrate `Vault::init(&Config)` → `Vault::init(&AppConfig)`;
  eventually move vault ownership to `AppState`.
- **16d**: Build `AppState::init(app_config, ...).await`.
- **16e**: Switch `main.rs` and all 15 `Config::init()` callers to
  the new flow.
- **16f**: Delete `Config` runtime fields, `bridge.rs`, `Config::init`,
  duplicated methods.

## Migration direction preserved

After 16b, `Config::init` no longer handles builtin-asset installation.
This is a small but meaningful piece of responsibility removal — when
`Config::init` is eventually deleted in 16f, we don't need to worry
about orphaning the install logic.

Startup flow now:
```
main()
    → install_builtins()?        [NEW: extracted from Config::init]
    → if oauth: Config::init_bare → oauth flow
    → if vault flags: Config::init_bare → vault handler
    → else: Config::init → to_app_config → AppState → ctx → run
```

The `Config::init` calls still do:
- load_envs
- set_wrap
- load_functions
- load_mcp_servers
- setup_model
- setup_document_loaders
- setup_user_agent

Those move to `AppConfig::from_config` (already built in 16a but
unused) and `AppState::init` (16d) in later sub-phases.
