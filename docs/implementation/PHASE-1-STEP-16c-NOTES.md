# Phase 1 Step 16c — Implementation Notes

## Status

Done.

## Plan reference

- Parent plan: `docs/implementation/PHASE-1-STEP-16-NOTES.md`
- Sub-phase goal: "Migrate Vault onto AppState; Vault::init takes
  `&AppConfig`"

## Summary

Changed `Vault::init(&Config)` and `Vault::init_bare()` to operate on
`AppConfig` instead of `Config`. Simplified `Vault::handle_vault_flags`
to accept a `&Vault` directly instead of extracting one from a Config
argument. Deleted the duplicate `Config::vault_password_file` method
(the canonical version lives on `AppConfig`).

Vault is now fully Config-independent at the signature level. The
`Config.vault` runtime field still exists because it's populated
inside `Config::init` (for the current bridge-era flow), but nothing
about `Vault`'s API references `Config` anymore. The field itself
gets deleted in Step 16f when Config becomes a pure POJO.

## What was changed

### `src/vault/mod.rs`

**Signature change:**
```rust
// Before
pub fn init(config: &Config) -> Self
// After
pub fn init(config: &AppConfig) -> Self
```

**`Vault::init_bare` now uses `AppConfig::default()`** instead of
`Config::default()` to get the default vault password file path.
Behavioral parity — both `.default().vault_password_file()` calls
resolve to the same path (the fallback from `gman::config`).

**`Vault::handle_vault_flags` simplified:**
```rust
// Before
pub fn handle_vault_flags(cli: Cli, config: Config) -> Result<()>
// After
pub fn handle_vault_flags(cli: Cli, vault: &Vault) -> Result<()>
```

The old signature took a `Config` by value just to extract
`config.vault`. The new signature takes the Vault directly, which
decouples this function from Config entirely. Callers pass
`&config.vault` or equivalent.

**Import updated:**
- `use crate::config::Config` removed
- `use crate::config::AppConfig` added

### `src/config/mod.rs`

**In `Config::init`:**
```rust
// Before
let vault = Vault::init(config);
// After
let vault = Vault::init(&config.to_app_config());
```

This is a transitional call — `Config::init` builds a temporary
`AppConfig` via the bridge just to satisfy the new signature. This
temporary conversion disappears in Step 16e when `main.rs` stops
calling `Config::init` entirely and `AppConfig` is built first.

**Deleted `Config::vault_password_file`** method. The identical body
lives on `AppConfig`. All callers go through AppConfig now.

### `src/main.rs`

**Vault-flags path:**
```rust
// Before
return Vault::handle_vault_flags(cli, Config::init_bare()?);
// After
let cfg = Config::init_bare()?;
return Vault::handle_vault_flags(cli, &cfg.vault);
```

This is a minor restructure — same observable behavior, but the
Vault is extracted from the Config and passed directly. Makes the
vault-flags path obvious about what it actually needs (a Vault, not
a Config).

## Behavior parity

- `Vault::init` reads `config.vault_password_file()` — identical
  method on both Config and AppConfig (removed from Config in this
  step, kept on AppConfig).
- Password file initialization (`ensure_password_file_initialized`)
  still runs in `Vault::init` as before.
- `Vault::init_bare` fallback path resolves to the same default
  password file location.
- `handle_vault_flags` operates on the same Vault instance either
  way — just receives it directly instead of indirectly via Config.

## Files modified

- `src/vault/mod.rs` — imports, `init` signature, `init_bare`
  fallback source, `handle_vault_flags` signature.
- `src/config/mod.rs` — transitional `to_app_config()` call in
  `Config::init`; deleted duplicate `vault_password_file` method.
- `src/main.rs` — vault-flags path takes `&cfg.vault` directly.

## Assumptions made

1. **`AppConfig::default().vault_password_file()` behaves identically
   to `Config::default().vault_password_file()`.** Verified by
   comparing method bodies — identical logic, same fallback via
   `gman::config::Config::local_provider_password_file()`. Tests
   confirm 122 passing, no regressions.

2. **Transitional `&config.to_app_config()` in `Config::init` is
   acceptable.** The conversion happens once per Config::init call
   — trivial cost for a non-hot path. Disappears entirely in 16e.

3. **`handle_vault_flags` taking `&Vault` is a strict improvement.**
   The old signature took `Config` by value (wasteful for a function
   that only needed one field). The new signature is honest about
   its dependency.

4. **`Config.vault` field stays for now.** The `#[serde(skip)]` field
   on Config still exists because `Config::init` populates it for
   downstream Bridge flow consumers. Deletion deferred to 16f.

## Open questions

1. **Should `Vault::init` return `Result<Self>` instead of panicking?**
   Currently uses `.expect("Failed to initialize password file")`.
   The vault flags path can't do anything useful without a vault,
   so panic vs early return is pragmatically equivalent. Leaving
   as-is to minimize change surface in 16c.

2. **Is `Config::init_bare` still needed after 16e?** It's called
   from the oauth path in `main.rs`. In 16e we'll audit whether
   those paths really need full Config init or just an AppConfig.
   Deferred to 16e.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean, zero warnings
- `cargo test` — 122 passing, zero failures
- Grep confirmation:
  - `Vault::init(` has one caller (in `Config::init`); one
    remaining via test path (none found — deferred to 16d/16e where
    AppState::init will own vault construction)
  - `Vault::init_bare(` has one caller (via `interpolate_secrets`
    flow); no other references
  - `Config::vault_password_file` — zero references anywhere
  - `Vault::handle_vault_flags` — single caller in `main.rs`,
    signature verified

## Remaining work for Step 16

- **16d**: Build `AppState::init(app_config, ...).await` that takes
  ownership of vault construction (replacing the current
  `AppState { vault: cfg.vault.clone(), ... }` pattern).
- **16e**: Switch `main.rs` and all 15 `Config::init()` callers to
  the new flow.
- **16f**: Delete `Config.vault` field, `Config::init`, bridge.rs,
  and all remaining Config runtime fields.

## Migration direction preserved

Before 16c:
```
Vault::init(&Config)           ← tight coupling to Config
Vault::init_bare()             ← uses Config::default() internally
handle_vault_flags(Cli, Config) ← takes Config, extracts vault
Config::vault_password_file()  ← duplicate with AppConfig's
```

After 16c:
```
Vault::init(&AppConfig)        ← depends only on AppConfig
Vault::init_bare()             ← uses AppConfig::default() internally
handle_vault_flags(Cli, &Vault) ← takes Vault directly
Config::vault_password_file()  ← DELETED
```

The Vault module now has no Config dependency in its public API.
This means Step 16d can build `AppState::init` that calls
`Vault::init(&app_config)` without touching Config at all. It also
means `Config` is one step closer to being a pure POJO — one fewer
method on its surface, one fewer implicit dependency.
