# Phase 1 Step 8i — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8i: Migrate `Rag` module away from `GlobalConfig`"

## Summary

Migrated the `Rag` module's public API from `&GlobalConfig` to
`&AppConfig` + `&[ClientConfig]`. The `Rag` struct now holds
`app_config: Arc<AppConfig>` and `clients_config: Vec<ClientConfig>`
instead of `config: GlobalConfig`. A private `build_temp_global_config`
bridge method remains for `init_client` calls (client module still
takes `&GlobalConfig` — Step 8j scope).

`RequestContext::use_rag`, `edit_rag_docs`, and `rebuild_rag` were
rewritten to call Rag methods directly with `&AppConfig`, eliminating
3 `to_global_config()` escape hatches.

## What was changed

### Files modified

- **`src/rag/mod.rs`** — struct field change + all method signatures:
  - `Rag` struct: `config: GlobalConfig` → `app_config: Arc<AppConfig>`
    + `clients_config: Vec<ClientConfig>`
  - `Rag::init`, `load`, `create`: `&GlobalConfig` → `&AppConfig` + `&[ClientConfig]`
  - `Rag::create_config`: `&GlobalConfig` → `&AppConfig`
  - `Rag::refresh_document_paths`: `&GlobalConfig` → `&AppConfig`
  - Added `build_temp_global_config()` private bridge for `init_client`
  - Updated `Clone` and `Debug` impls

- **`src/config/request_context.rs`** — rewrote `use_rag`,
  `edit_rag_docs`, `rebuild_rag` to call Rag methods directly with
  `&AppConfig` instead of bridging through `to_global_config()`

- **`src/config/mod.rs`** — updated `Config::use_rag`,
  `Config::edit_rag_docs`, `Config::rebuild_rag` to extract
  `AppConfig` and `clients` before calling Rag methods

- **`src/config/agent.rs`** — updated `Agent::init`'s Rag loading
  to pass `&AppConfig` + `&clients`

- **`src/config/app_config.rs`** — added `clients: Vec<ClientConfig>`
  field (was missing; needed by Rag callers)

- **`src/config/bridge.rs`** — added `clients` to `to_app_config()`
  and `from_parts()` conversions

## Key decisions

### 1. `clients_config` captured at construction time

`init_client` reads `config.read().clients` to find the right client
implementation. Rather than holding a `GlobalConfig`, the Rag struct
captures `clients_config: Vec<ClientConfig>` at construction time.
This is safe because client configs don't change during a Rag's
lifetime.

### 2. `build_temp_global_config` bridge for init_client

`init_client` and each client's `init` method still take `&GlobalConfig`
(Step 8j scope). The bridge builds a minimal `Config::default()` with
just the `clients` field populated. This is sufficient because
`init_client` only reads `config.read().clients` and
`config.read().model`.

### 3. `AppConfig` gained a `clients` field

`AppConfig` was missing `clients: Vec<ClientConfig>`. This field is
needed by any code that calls Rag methods (and eventually by
`init_client` when it's migrated in Step 8j). Added to `AppConfig`,
`to_app_config()`, and `from_parts()`.

## Verification

- `cargo check` — clean, zero warnings
- `cargo clippy` — clean
- `cargo test` — 63 passed, 0 failed

## GlobalConfig reference count

| Module | Before 8i | After 8i | Delta |
|---|---|---|---|
| `rag/mod.rs` | 6 | 1 (bridge only) | -5 |
| `request_context.rs` `to_global_config()` calls | 5 | 2 | -3 |

## Handoff to next step

Step 8j (Input + eval_tool_calls migration) can proceed. It can
now use `AppConfig.clients` for client initialization.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8i
- Step 8h notes: `docs/implementation/PHASE-1-STEP-8h-NOTES.md`
- QA checklist: `docs/QA-CHECKLIST.md` — items 13 (RAG)
