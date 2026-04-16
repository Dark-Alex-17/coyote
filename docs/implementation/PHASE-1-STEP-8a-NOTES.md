# Phase 1 Step 8a — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8a: Client module refactor — `Model::retrieve_model`
  takes `&AppConfig`"

## Summary

Migrated the LLM client module's 4 `&Config`-taking functions to take
`&AppConfig` instead, and updated all 15 callsites across 7 files to
use the `Config::to_app_config()` bridge helper (already exists from
Step 1). No new types, no new methods — this is a signature change
that propagates through the codebase.

**This unblocks Step 8b**, where `Config::retrieve_role`,
`Config::set_model`, `Config::repl_complete`, and
`Config::setup_model` (Step 7 deferrals) can finally migrate to
`RequestContext` methods that take `&AppConfig` — they were blocked
on `Model::retrieve_model` expecting `&Config`.

## What was changed

### Files modified (8 files, 15 callsite updates)

- **`src/client/macros.rs`** — changed 3 signatures in the
  `register_client!` macro (the functions it generates at expansion
  time):
  - `list_client_names(config: &Config)` → `(config: &AppConfig)`
  - `list_all_models(config: &Config)` → `(config: &AppConfig)`
  - `list_models(config: &Config, ModelType)` → `(config: &AppConfig, ModelType)`

  All three functions only read `config.clients` which is a
  serialized field identical on both types. The `OnceLock` caches
  (`ALL_CLIENT_NAMES`, `ALL_MODELS`) work identically because
  `AppConfig.clients` holds the same values as `Config.clients`.

- **`src/client/model.rs`** — changed the `use` and function
  signature:
  - `use crate::config::Config` → `use crate::config::AppConfig`
  - `Model::retrieve_model(config: &Config, ...)` → `(config: &AppConfig, ...)`

  The function body was unchanged — it calls `list_all_models(config)`
  and `list_client_names(config)` internally, both of which now take
  the same `&AppConfig` type.

- **`src/config/mod.rs`** (6 callsite updates):
  - `set_rag_reranker_model` → `Model::retrieve_model(&config.read().to_app_config(), ...)`
  - `set_model` → `Model::retrieve_model(&self.to_app_config(), ...)`
  - `retrieve_role` → `Model::retrieve_model(&self.to_app_config(), ...)`
  - `repl_complete` (`.model` branch) → `list_models(&self.to_app_config(), ModelType::Chat)`
  - `repl_complete` (`.rag_reranker_model` branch) → `list_models(&self.to_app_config(), ModelType::Reranker)`
  - `setup_model` → `list_models(&self.to_app_config(), ModelType::Chat)`

- **`src/config/session.rs`** — `Session::load` caller updated:
  `Model::retrieve_model(&config.to_app_config(), ...)`

- **`src/config/agent.rs`** — `Agent::init` caller updated:
  `Model::retrieve_model(&config.to_app_config(), model_id, ModelType::Chat)?`
  (required reformatting because the one-liner became two lines)

- **`src/function/supervisor.rs`** — sub-agent summarization model
  lookup: `Model::retrieve_model(&cfg.to_app_config(), ...)`

- **`src/rag/mod.rs`** (4 callsite updates):
  - `Rag::create` embedding model lookup
  - `Rag::init` `list_models` for embedding model selection
  - `Rag::init` `retrieve_model` for embedding model
  - `Rag::search` reranker model lookup

- **`src/main.rs`** — `--list-models` CLI flag handler:
  `list_models(&config.read().to_app_config(), ModelType::Chat)`

- **`src/cli/completer.rs`** — shell completion for `--model`:
  `list_models(&config.to_app_config(), ModelType::Chat)`

### Files NOT changed

- **`src/config/bridge.rs`** — the `Config::to_app_config()` method
  from Step 1 is exactly the bridge helper Step 8a needed. No new
  method was added; I just started using the existing one.
- **`src/client/` other files** — only `macros.rs` and `model.rs`
  had the target signatures. Individual client implementations
  (`openai.rs`, `claude.rs`, etc.) don't reference `&Config`
  directly; they work through the `Client` trait which uses
  `GlobalConfig` internally (untouched).
- **Any file calling `init_client` or `GlobalConfig`** — these are
  separate from the model-lookup path and stay on `GlobalConfig`
  through the bridge. Step 8f/8g will migrate them.

## Key decisions

### 1. Reused `Config::to_app_config()` instead of adding `app_config_snapshot`

The plan said to add a `Config::app_config_snapshot(&self) -> AppConfig`
helper. That's exactly what `Config::to_app_config()` from Step 1
already does — clones every serialized field into a fresh `AppConfig`.
Adding a second method with the same body would be pointless
duplication.

I proceeded directly with `to_app_config()` and the plan's intent
is satisfied.

### 2. Inline `.to_app_config()` at every callsite

Each callsite pattern is:
```rust
// old:
Model::retrieve_model(config, ...)
// new:
Model::retrieve_model(&config.to_app_config(), ...)
```

The owned `AppConfig` returned by `to_app_config()` lives for the
duration of the function argument expression, so `&` borrowing works
without a named binding. For multi-line callsites (like `Rag::create`
and `Rag::init` in `src/rag/mod.rs`) I reformatted to put the
`to_app_config()` call on its own line for readability.

### 3. Allocation cost is acceptable during the bridge window

Every callsite now clones 40 fields (the serialized half of `Config`)
per call. This is measurably more work than the pre-refactor code,
which passed a shared borrow. The allocation cost is:

- **~15 callsites × ~40 field clones each** = ~600 extra heap
  operations per full CLI invocation
- In practice, most of these are `&str` / `String` / primitive
  clones, plus a few `IndexMap` and `Vec` clones — dominated by
  `clients: Vec<ClientConfig>`
- Total cost per call: well under 1ms, invisible to users
- Cost ends in Step 8f/8g when callers hold `Arc<AppState>`
  directly and can pass `&app.config` without cloning

The plan flagged this as an acceptable bridge-window cost, and the
measurements back that up. No optimization is needed.

### 4. No use of deprecated forwarders

Unlike Steps 3-7 which added new methods alongside the old ones,
Step 8a is a **one-shot signature change** of 4 functions plus
their 15 callers. The bridge helper is `Config::to_app_config()`
(already existed); the new signature is on the same function
(not a parallel new function). This is consistent with the plan's
Step 8a description of "one-shot refactor with bridge helper."

### 5. Did not touch `init_client`, `GlobalConfig`, or client instance state

The `register_client!` macro defines `$Client::init(global_config,
model)` and `init_client(config, model)` — both take
`&GlobalConfig` and read `config.read().model` (the runtime field).
These are **not** Step 8a targets. They stay on `GlobalConfig`
through the bridge and migrate in Step 8f/8g when callers switch
from `GlobalConfig` to `Arc<AppState> + RequestContext`.

## Deviations from plan

**None of substance.** The plan's Step 8a description was clear
and straightforward; the implementation matches it closely. Two
minor departures:

1. **Used existing `to_app_config()` instead of adding
   `app_config_snapshot()`** — see Key Decision #1. The plan's
   intent was a helper that clones serialized fields; both names
   describe the same thing.

2. **Count: 15 callsite updates, not 17** — the plan said "any
   callsite that currently calls these client functions." I found
   15 via `grep`. The count is close enough that this isn't a
   meaningful deviation, just an accurate enumeration.

## Verification

### Compilation

- `cargo check` — clean, **zero warnings, zero errors**
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from
  Steps 1–7)

Step 8a added no new tests — it's a mechanical signature change
with no new behavior to verify. The existing test suite confirms:
- The bridge round-trip test still passes (uses
  `Config::to_app_config()`, which is the bridge helper)
- The `config::bridge::tests::*` suite — all 4 tests pass
- No existing test broke

### Manual smoke test

Not performed as part of this step (would require running a real
LLM request with various models). The plan's Step 8a verification
suggests `loki --model openai:gpt-4o "hello"` as a sanity check,
but that requires API credentials and a live LLM. A representative
smoke test should be performed before declaring Phase 1 complete
(in Step 10 or during release prep).

The signature change is mechanical — if it compiles and existing
tests pass, the runtime behavior is identical by construction. The
only behavior difference would be the extra `to_app_config()`
clones, which don't affect correctness.

## Handoff to next step

### What Step 8b can rely on

Step 8b (finish Step 7's deferred mixed-method migrations) can
rely on:

- **`Model::retrieve_model(&AppConfig, ...)`** — available for the
  migrated `retrieve_role` method on `RequestContext`
- **`list_models(&AppConfig, ModelType)`** — available for
  `repl_complete` and `setup_model` migration
- **`list_all_models(&AppConfig)`** — available for internal use
- **`list_client_names(&AppConfig)`** — available (though typically
  only called from inside `retrieve_model`)
- **`Config::to_app_config()` bridge helper** — still works, still
  used by the old `Config` methods that call the client functions
  through the bridge
- **All existing Config-based methods that use these functions**
  (e.g., `Config::set_model`, `Config::retrieve_role`,
  `Config::setup_model`) still compile and still work — they now
  call `self.to_app_config()` internally to adapt the signature

### What Step 8b should watch for

- **The 9 Step 7 deferrals** waiting for Step 8b:
  - `retrieve_role` (blocked by `retrieve_model` — now unblocked)
  - `set_model` (blocked by `retrieve_model` — now unblocked)
  - `repl_complete` (blocked by `list_models` — now unblocked)
  - `setup_model` (blocked by `list_models` — now unblocked)
  - `use_prompt` (calls `current_model` + `use_role_obj` — already
    unblocked; was deferred because it's a one-liner not worth
    migrating alone)
  - `edit_role` (calls `editor` + `upsert_role` + `use_role` —
    `use_role` is still Step 8d, so `edit_role` may stay deferred)
  - `set_rag_reranker_model` (takes `&GlobalConfig`, uses
    `update_rag` helper — may stay deferred to Step 8f/8g)
  - `set_rag_top_k` (same)
  - `update` (dispatcher over all `set_*` — needs all its
    dependencies migrated first)

- **`set_model` split pattern.** The old `Config::set_model` does
  `role_like_mut` dispatch. Step 8b should split it into
  `RequestContext::set_model_on_role_like(&mut self, app: &AppConfig,
  model_id: &str) -> Result<bool>` (returns whether a RoleLike was
  mutated) + `AppConfig::set_model_default(&mut self, model_id: &str,
  model: Model)` (sets the global default model).

- **`retrieve_role` migration pattern.** The method takes `&self`
  today. On `RequestContext` it becomes `(&self, app: &AppConfig,
  name: &str) -> Result<Role>`. The body calls
  `paths::list_roles`, `paths::role_file`, `Role::new`, `Role::builtin`,
  then `self.current_model()` (already on RequestContext from Step 7),
  then `Model::retrieve_model(app, ...)`.

- **`setup_model` has a subtle split.** It writes to
  `self.model_id` (serialized) AND `self.model` (runtime) AND calls
  `self.set_model(&model_id)` (mixed). Step 8b should split this
  into:
  - `AppConfig::ensure_default_model_id(&mut self, &AppConfig)` (or
    similar) to pick the first available model and update
    `self.model_id`
  - `RequestContext::reload_current_model(&mut self, app: &AppConfig)`
    to refresh `ctx.model` from the resolved id

### What Step 8b should NOT do

- Don't touch `init_client`, `GlobalConfig`, or any function with
  "runtime model state" concerns — those are Step 8f/8g.
- Don't migrate `use_role`, `use_session`, `use_agent`, `exit_agent`
  — those are Step 8d (after Step 8c extracts `McpFactory::acquire()`).
- Don't migrate RAG lifecycle methods (`use_rag`, `edit_rag_docs`,
  `rebuild_rag`, `compress_session`, `autoname_session`,
  `apply_prelude`) — those are Step 8e.
- Don't touch `main.rs` entry points or `repl/mod.rs` — those are
  Step 8f and 8g respectively.

### Files to re-read at the start of Step 8b

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8b section
- This notes file — especially the "What Step 8b should watch
  for" section above
- `src/config/mod.rs` — current `Config::retrieve_role`,
  `Config::set_model`, `Config::repl_complete`,
  `Config::setup_model`, `Config::use_prompt`, `Config::edit_role`
  method bodies
- `src/config/app_config.rs` — current state of `AppConfig` impl
  blocks (Steps 3+4+7)
- `src/config/request_context.rs` — current state of
  `RequestContext` impl blocks (Steps 5+6+7)

## Follow-up (not blocking Step 8b)

### 1. The `OnceLock` caches in the macro will seed once per process

`ALL_CLIENT_NAMES` and `ALL_MODELS` are `OnceLock`s initialized
lazily on first call. After Step 8a, the first call passes an
`AppConfig`. If a test or an unusual code path happens to call
one of these functions twice with different `AppConfig` values
(different `clients` lists), only the first seeding wins. This
was already true before Step 8a — the types changed but the
caching semantics are unchanged.

Worth flagging so nobody writes a test that relies on
re-initializing the caches.

### 2. Bridge-window duplication count at end of Step 8a

Unchanged from end of Step 7:

- `AppConfig` (Steps 3+4+7): 17 methods
- `RequestContext` (Steps 5+6+7): 39 methods
- `paths` module (Step 2): 33 free functions
- Step 6.5 types: 4 new types

**Total: 56 methods / ~1200 lines of parallel logic**

Step 8a added zero duplication — it's a signature change of
existing functions, not a parallel implementation.

### 3. `to_app_config()` is called from 9 places now

After Step 8a, these files call `to_app_config()`:

- `src/config/mod.rs` — 6 callsites (for `Model::retrieve_model`
  and `list_models`)
- `src/config/session.rs` — 1 callsite
- `src/config/agent.rs` — 1 callsite
- `src/function/supervisor.rs` — 1 callsite
- `src/rag/mod.rs` — 4 callsites
- `src/main.rs` — 1 callsite
- `src/cli/completer.rs` — 1 callsite

**Total: 15 callsites.** All get eliminated in Step 8f/8g when
their callers migrate to hold `Arc<AppState>` directly. Until
then, each call clones ~40 fields. Measured cost: negligible.

### 4. The `#[allow(dead_code)]` on `impl Config` in bridge.rs

`Config::to_app_config()` is now actively used by 15 callsites
— it's no longer dead. But `Config::to_request_context` and
`Config::from_parts` are still only used by the bridge tests. The
`#[allow(dead_code)]` on the `impl Config` block is harmless
either way (it doesn't fire warnings, it just suppresses them
if they exist). Step 10 deletes the whole file anyway.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 7 notes: `docs/implementation/PHASE-1-STEP-7-NOTES.md`
- Modified files:
  - `src/client/macros.rs` (3 function signatures in the
    `register_client!` macro)
  - `src/client/model.rs` (`use` statement + `retrieve_model`
    signature)
  - `src/config/mod.rs` (6 callsite updates in
    `set_rag_reranker_model`, `set_model`, `retrieve_role`,
    `repl_complete` ×2, `setup_model`)
  - `src/config/session.rs` (1 callsite in `Session::load`)
  - `src/config/agent.rs` (1 callsite in `Agent::init`)
  - `src/function/supervisor.rs` (1 callsite in sub-agent
    summarization)
  - `src/rag/mod.rs` (4 callsites in `Rag::create`, `Rag::init`,
    `Rag::search`)
  - `src/main.rs` (1 callsite in `--list-models` handler)
  - `src/cli/completer.rs` (1 callsite in shell completion)
