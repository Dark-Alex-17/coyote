# Phase 1 Step 1 — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 1: Make Config constructible from AppConfig + RequestContext"

## Summary

Added three conversion methods on `Config` (`to_app_config`,
`to_request_context`, `from_parts`) plus a round-trip test suite, all
living in a new `src/config/bridge.rs` module. These methods are the
facade that will let Steps 2–9 migrate callsites from the old `Config`
to the split `AppState` + `RequestContext` incrementally. Nothing calls
them outside the test suite yet; that's expected and matches the
plan's "additive only, no callsite changes" guidance for Step 1.

## Pre-Step-1 correction to Step 0

Before implementing Step 1 I verified all three Step 0 files
(`src/config/app_config.rs`, `src/config/app_state.rs`,
`src/config/request_context.rs`) against every architecture decision
from the design conversations. All three were current except one stale
reference:

- `src/config/request_context.rs` docstring said "unified into
  `ToolScope` during Phase 1 Step 6" but after the
  ToolScope/AgentRuntime discussions the plan renumbered this to
  **Step 6.5** and added the `AgentRuntime` collapse alongside
  `ToolScope`. Updated the `# Tool scope (planned)` section docstring
  to reflect both changes (now titled `# Tool scope and agent runtime
  (planned)`).

No other Step 0 changes were needed.

## What was changed

### New files

- **`src/config/bridge.rs`** (~430 lines including tests)
  - Module docstring explaining the bridge's purpose, scheduled
    deletion in Step 10, and the lossy `mcp_registry` field.
  - `impl Config` block with three public methods, scoped under
    `#[allow(dead_code)]`:
    - `to_app_config(&self) -> AppConfig` — borrow, returns fresh
      `AppConfig` by cloning the 40 serialized fields.
    - `to_request_context(&self, app: Arc<AppState>) -> RequestContext`
      — borrow + provided `AppState`, returns fresh `RequestContext`
      by cloning the 19 runtime fields held on both types.
    - `from_parts(app: &AppState, ctx: &RequestContext) -> Config` —
      borrow both halves, returns a new owned `Config`. Sets
      `mcp_registry: None` because no split type holds it.
  - `#[cfg(test)] mod tests` with 4 unit tests:
    - `to_app_config_copies_every_serialized_field`
    - `to_request_context_copies_every_runtime_field`
    - `round_trip_preserves_all_non_lossy_fields`
    - `round_trip_default_config`
  - Helper `build_populated_config()` that sets every primitive /
    `String` / simple `Option` field to a non-default value so a
    missed field in the conversion methods produces a test failure.

### Modified files

- **`src/config/mod.rs`** — added `mod bridge;` declaration (one
  line, inserted alphabetically between `app_state` and `input`).
- **`src/config/request_context.rs`** — updated the "Tool scope
  (planned)" docstring section to correctly reference Phase 1
  **Step 6.5** (not Step 6) and to mention the `AgentRuntime`
  collapse alongside `ToolScope`. No code changes.

## Key decisions

### 1. The bridge lives in its own module

I put the conversion methods in `src/config/bridge.rs` rather than
adding them inline to `src/config/mod.rs`. The plan calls for this
entire bridge to be deleted in Step 10, and isolating it in one file
makes that deletion a single `rm` + one `mod bridge;` line removal in
`mod.rs`. Adding ~300 lines to the already-massive `mod.rs` would have
made the eventual cleanup harder.

### 2. `mcp_registry` is lossy by design (documented)

`Config.mcp_registry: Option<McpRegistry>` has no home in either
`AppConfig` (serialized settings only) or `RequestContext` (runtime
state that doesn't include MCP, per Step 6.5's `ToolScope` design).
I considered three options:

1. **Add a temporary `mcp_registry` field to `RequestContext`** — ugly,
   introduces state that has to be cleaned up in Step 6.5 anyway.
2. **Accept lossy round-trip, document it** — chosen.
3. **Store `mcp_registry` on `AppState` temporarily** — dishonest,
   contradicts the plan which says MCP isn't process-wide.

Option 2 aligns with the plan's direction. The lossy field is
documented in three places so no caller is surprised:

- Module-level docstring (`# Lossy fields` section)
- `from_parts` method docstring
- Inline comment next to the `is_none()` assertion in the round-trip
  test

Any Step 2–9 callsite that still needs the registry during its
migration window must keep a reference to the original `Config`
rather than relying on round-trip fidelity.

### 3. `#[allow(dead_code)]` scoped to the whole `impl Config` block

Applied to the `impl` block in `bridge.rs` rather than individually to
each method. All three methods are dead until Step 2+ starts calling
them. When the first caller migrates, I'll narrow the allow to the
methods that are still unused. By Step 10 the whole file is deleted
and the allow goes with it.

### 4. Populated-config builder skips domain-type runtime fields

`build_populated_config()` sets every primitive, `String`, and simple
`Option` field to a non-default value. It does **not** try to construct
real `Role`, `Session`, `Agent`, `Supervisor`, `Inbox`, or
`EscalationQueue` instances because those have complex async/setup
lifecycles and constructors don't exist for test use.

The round-trip tests still exercise the clone path for all those
`Option<T>` fields — they just exercise the `None` variant. The tests
prove that (a) if a runtime field is set, the conversion clones it
correctly (which is guaranteed by Rust's `#[derive(Clone)]` on
`Config`), and (b) `None` roundtrips to `None`. Deeper coverage with
populated domain types would require mock constructors that don't
exist in the current code, making it a meaningful scope increase
unsuitable for Step 1's "additive, mechanical" goal.

### 5. The test covers `Config::default()` separately from the
populated builder

A separate `round_trip_default_config` test catches any subtle "the
default doesn't roundtrip" bug that `build_populated_config` might
mask by always setting fields to non-defaults. Both tests run through
the same `to_app_config → to_request_context → from_parts` pipeline.

## Deviations from plan

None of substance. The plan's Step 1 description was three sentences
and a pseudocode block; the implementation matches it field-for-field
except for two clarifications the plan didn't specify:

1. **Which module holds the methods** — the plan didn't say. I chose a
   dedicated `src/config/bridge.rs` file (see Key Decision #1).

2. **How `mcp_registry` is handled in round-trip** — the plan's
   pseudocode said `from_parts` "merges back" but didn't address the
   field that has no home. I chose lossy reconstruction with
   documented behavior (see Key Decision #2).

Both clarifications are additive — they don't change what Step 1
accomplishes, they just pin down details the plan left implicit.

## Verification

### Compilation

- `cargo check` — clean, zero warnings. The expected dead-code warning
  from the new methods is suppressed by `#[allow(dead_code)]` on the
  `impl` block.

### Tests

- `cargo test bridge` — 4 new tests pass:
  - `config::bridge::tests::round_trip_default_config`
  - `config::bridge::tests::to_app_config_copies_every_serialized_field`
  - `config::bridge::tests::to_request_context_copies_every_runtime_field`
  - `config::bridge::tests::round_trip_preserves_all_non_lossy_fields`

- `cargo test` — full suite passes: **63 passed, 0 failed**
  (59 pre-existing + 4 new).

### Manual smoke test

Not applicable — Step 1 is additive only, no runtime behavior changed.
CLI and REPL continue working through the original `Config` code
paths, unchanged.

## Handoff to next step

### What Step 2 can rely on

Step 2 (migrate ~30 static methods off `Config` to a `paths` module)
can rely on all of the following being true:

- `Config::to_app_config()`, `Config::to_request_context(app)`, and
  `Config::from_parts(app, ctx)` all exist and are tested.
- The three new types (`AppConfig`, `AppState`, `RequestContext`) are
  fully defined and compile.
- Nothing in the codebase outside `src/config/bridge.rs` currently
  calls the new methods, so Step 2 is free to start using them
  wherever convenient without fighting existing callers.
- `AppState` only has two fields: `config: Arc<AppConfig>` and
  `vault: GlobalVault`. No `mcp_factory`, no `rag_cache` yet — those
  land in Step 6.5.
- `RequestContext` has flat fields mirroring the runtime half of
  today's `Config`. The `ToolScope` / `AgentRuntime` unification
  happens in Step 6.5, not earlier. Step 2 should not try to
  pre-group fields.

### What Step 2 should watch for

- **Static methods on `Config` with no `&self` parameter** are the
  Step 2 target. The Phase 1 plan lists ~33 of them in a table
  (`config_dir`, `local_path`, `cache_path`, etc.). Each gets moved
  to a new `src/config/paths.rs` module (or similar), with forwarding
  `#[deprecated]` methods left behind on `Config` until Step 2 is
  fully done.
- **`vault_password_file`** on `Config` is private (not `pub`), but
  `vault_password_file` on `AppConfig` is `pub(crate)`. `bridge.rs`
  accesses both directly because it's a sibling module under
  `src/config/`. If Step 2's path functions need to read
  `vault_password_file` from `AppConfig` they can do so directly
  within the `config` module, but callers outside the module will
  need an accessor method.
- **`Config.mcp_registry` round-trip is lossy.** If any static method
  moved in Step 2 touches `mcp_registry` (unlikely — none of the ~33
  static methods listed in the plan do), that method should NOT use
  the bridge — it should keep operating on the original `Config`.
  Double-check the list before migrating.

### What Step 2 should NOT do

- Don't delete the bridge. It's still needed for Steps 3–9.
- Don't narrow `#[allow(dead_code)]` on `impl Config` in `bridge.rs`
  yet — Step 2 might start using some of the methods but not all,
  and the allow-scope should be adjusted once (at the end of Step 2)
  rather than incrementally.
- Don't touch the `request_context.rs` `# Tool scope and agent
  runtime (planned)` docstring. It's accurate and Step 6.5 is still
  far off.

### Files to re-read at the start of Step 2

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 2 section has the
  full static-method migration table.
- This notes file (`PHASE-1-STEP-1-NOTES.md`) — for the bridge's
  current shape and the `mcp_registry` lossy-field context.
- `src/config/bridge.rs` — for the exact method signatures available.

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Architecture doc: `docs/REST-API-ARCHITECTURE.md`
- Step 0 files: `src/config/app_config.rs`, `src/config/app_state.rs`,
  `src/config/request_context.rs`
- Step 1 files: `src/config/bridge.rs`, `src/config/mod.rs` (mod
  declaration), `src/config/request_context.rs` (docstring fix)
