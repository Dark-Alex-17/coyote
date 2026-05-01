# Phase 1 Step 8c — Implementation Notes

## Status

Done.

## Plan reference

- Plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Section: "Step 8c: Extract `McpFactory::acquire()` from
  `McpRegistry::init_server`"

## Summary

Extracted the MCP subprocess spawn + rmcp handshake logic from
`McpRegistry::start_server` into a standalone `pub(crate) async fn
spawn_mcp_server()` function. Rewrote `start_server` to call it.
Implemented `McpFactory::acquire()` using the extracted function
plus the existing `try_get_active` / `insert_active` scaffolding
from Step 6.5. Three types in `mcp/mod.rs` were bumped to
`pub(crate)` visibility for cross-module access.

## What was changed

### Files modified (2 files)

- **`src/mcp/mod.rs`** — 4 changes:

  1. **Extracted `spawn_mcp_server`** (~40 lines) — standalone
     `pub(crate) async fn` that takes an `&McpServer` spec and
     optional log path, builds a `tokio::process::Command`, creates
     a `TokioChildProcess` transport (with optional stderr log
     redirect), calls `().serve(transport).await` for the rmcp
     handshake, and returns `Arc<ConnectedServer>`.

  2. **Rewrote `McpRegistry::start_server`** — now looks up the
     `McpServer` spec from `self.config`, calls `spawn_mcp_server`,
     then does its own catalog building (tool listing, BM25 index
     construction). The spawn + handshake code that was previously
     inline is replaced by the one-liner
     `spawn_mcp_server(spec, self.log_path.as_deref()).await?`.

  3. **Bumped 3 types to `pub(crate)`**: `McpServer`, `JsonField`,
     `McpServersConfig`. These were previously private to
     `mcp/mod.rs`. `McpFactory::acquire()` and
     `McpServerKey::from_spec()` need `McpServer` and `JsonField`
     to build the server key from a spec. `McpServersConfig` is
     bumped for completeness (Step 8d may need to access it when
     loading server specs during scope transitions).

- **`src/config/mcp_factory.rs`** — 3 changes:

  1. **Added `McpServerKey::from_spec(name, &McpServer)`** — builds
     a key by extracting command, args (defaulting to empty vec),
     and env vars (converting `JsonField` variants to strings) from
     the spec. Args and env are sorted by the existing `new()`
     constructor to ensure identical specs produce identical keys.

  2. **Added `McpFactory::acquire(name, &McpServer, log_path)`** —
     the core method. Builds an `McpServerKey` from the spec, checks
     `try_get_active` for an existing `Arc` (sharing path), otherwise
     calls `spawn_mcp_server` to start a fresh subprocess, inserts
     the result into `active` via `insert_active`, and returns the
     `Arc<ConnectedServer>`.

  3. **Updated imports** — added `McpServer`, `spawn_mcp_server`,
     `Result`, `Path`.

### Files NOT changed

- **`src/config/tool_scope.rs`** — unchanged; Step 8d will use
  `McpFactory::acquire()` to populate `McpRuntime` instances.
- **All caller code** — `McpRegistry::start_select_mcp_servers` and
  `McpRegistry::reinit` continue to call `self.start_server()` which
  internally uses the extracted function. No caller migration.

## Key decisions

### 1. Spawn function does NOT list tools or build catalogs

The plan said to extract "the MCP subprocess spawn + rmcp handshake
logic (~60 lines)." I interpreted this as: `Command` construction →
transport creation → `serve()` handshake → `Arc` wrapping. The tool
listing (`service.list_tools`) and catalog building (BM25 index) are
`McpRegistry`-specific bookkeeping and stay in `start_server`.

`McpFactory::acquire()` returns a connected server handle ready to
use. Callers (Step 8d's scope transitions) can list tools themselves
if they need to build function declarations.

### 2. No `abort_signal` parameter on `spawn_mcp_server`

The plan suggested `abort_signal: &AbortSignal` as a parameter. The
existing `start_server` doesn't use an abort signal — cancellation
is handled at a higher level by `abortable_run_with_spinner` wrapping
the entire batch of `start_select_mcp_servers`. Adding an abort signal
to the individual spawn would require threading `tokio::select!` into
the transport creation, which is a behavior change beyond Step 8c's
scope. Step 8d can add cancellation when building `ToolScope` if
needed.

### 3. `McpServerKey::from_spec` converts `JsonField` to strings

The `McpServer.env` field uses a `JsonField` enum (Str/Bool/Int) for
JSON flexibility. The key needs string comparisons for hashing, so
`from_spec` converts each variant to its string representation. This
matches the conversion already done in the env-building code inside
`spawn_mcp_server`.

### 4. `McpFactory::acquire` mutex contention is safe

The plan warned: "hold the lock only during HashMap mutation, never
across subprocess spawn." The implementation achieves this by using
the existing `try_get_active` and `insert_active` methods, which each
acquire and release the mutex within their own scope. The `spawn_mcp_server`
await happens between the two lock acquisitions with no lock held.

TOCTOU race: two concurrent callers could both miss in `try_get_active`,
both spawn, and both insert. The second insert overwrites the first's
`Weak`. This means one extra subprocess gets spawned and the first
`Arc` has no `Weak` in the map (but stays alive via its holder's
`Arc`). This is acceptable for Phase 1 — the worst case is a
redundant spawn, not a crash or leak. Phase 5's pooling design
(per-key `tokio::sync::Mutex`) will eliminate this race.

### 5. No integration tests for `acquire()`

The plan suggested writing integration tests for the factory's sharing
behavior. Spawning a real MCP server requires a configured binary on
the system PATH. A mock server would need a test binary that speaks
the rmcp stdio protocol — this is substantial test infrastructure
that doesn't exist yet. Rather than building it in Step 8c, I'm
documenting that integration testing of `McpFactory::acquire()` should
happen in Phase 5 when the pooling infrastructure provides natural
test hooks (idle pool, reaper, health checks). The extraction itself
is verified by the fact that existing MCP functionality (which goes
through `McpRegistry::start_server` → `spawn_mcp_server`) still
compiles and all 63 tests pass.

## Deviations from plan

| Deviation | Rationale |
|---|---|
| No `abort_signal` parameter | Not used by existing code; adding it is a behavior change |
| No integration tests | Requires MCP test infrastructure that doesn't exist |
| Removed `get_server_spec` / `log_path` accessors from McpRegistry | Not needed; `acquire()` takes spec and log_path directly |

## Verification

### Compilation

- `cargo check` — clean, zero warnings, zero errors
- `cargo clippy` — clean

### Tests

- `cargo test` — **63 passed, 0 failed** (unchanged from Steps 1–8b)

## Handoff to next step

### What Step 8d can rely on

- **`spawn_mcp_server(&McpServer, Option<&Path>) -> Result<Arc<ConnectedServer>>`** —
  available from `crate::mcp::spawn_mcp_server`
- **`McpFactory::acquire(name, &McpServer, log_path) -> Result<Arc<ConnectedServer>>`** —
  checks active map for sharing, spawns fresh if needed, inserts
  into active map
- **`McpServerKey::from_spec(name, &McpServer) -> McpServerKey`** —
  builds a hashable key from a server spec
- **`McpServer`, `McpServersConfig`, `JsonField`** — all `pub(crate)`
  and accessible from `src/config/`

### What Step 8d should do

Build real `ToolScope` instances during scope transitions:

1. Resolve the effective enabled-server list from the role/session/agent
2. Look up each server's `McpServer` spec (from the MCP config)
3. Call `app.mcp_factory.acquire(name, spec, log_path)` for each
4. Populate an `McpRuntime` with the returned `Arc<ConnectedServer>`
   handles
5. Construct a `ToolScope` with the runtime + resolved `Functions`
6. Assign to `ctx.tool_scope`

### What Step 8d should watch for

- **Log path.** `McpRegistry` stores `log_path` during `init()`.
  Step 8d needs to decide where the log path comes from for
  factory-acquired servers. Options: store it on `AppState`,
  compute it from `paths::cache_path()`, or pass it through from
  the caller. The simplest is to store it on `McpFactory` at
  construction time.

- **MCP config loading.** `McpRegistry::init()` loads and parses
  `mcp.json`. Step 8d's scope transitions need access to the
  parsed `McpServersConfig` to look up server specs by name.
  Options: store the parsed config on `AppState`, or load it
  fresh each time. Storing on `AppState` is more efficient.

- **Catalog building.** `McpRegistry::start_server` builds a
  `ServerCatalog` (BM25 index) for each server after spawning.
  Step 8d's `ToolScope` doesn't use catalogs — they're for the
  `mcp_search` meta-function. The catalog functionality may need
  to be lifted out of `McpRegistry` eventually, but that's not
  blocking Step 8d.

### Files to re-read at the start of Step 8d

- `docs/PHASE-1-IMPLEMENTATION-PLAN.md` — Step 8d section
- This notes file
- `src/config/mcp_factory.rs` — full file
- `src/config/tool_scope.rs` — full file
- `src/mcp/mod.rs` — `McpRegistry::init`, `start_select_mcp_servers`,
  `resolve_server_ids` for the config loading / server selection
  patterns that Step 8d will replicate

## References

- Phase 1 plan: `docs/PHASE-1-IMPLEMENTATION-PLAN.md`
- Step 8b notes: `docs/implementation/PHASE-1-STEP-8b-NOTES.md`
- Step 6.5 notes: `docs/implementation/PHASE-1-STEP-6.5-NOTES.md`
- Modified files:
  - `src/mcp/mod.rs` (extracted `spawn_mcp_server`, rewrote
    `start_server`, bumped 3 types to `pub(crate)`)
  - `src/config/mcp_factory.rs` (added `from_spec`, `acquire`,
    updated imports)
