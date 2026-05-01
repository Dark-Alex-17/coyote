# Architecture Plan: Loki REST API Service Mode

## The Core Problem

Today, Loki's `Config` struct is a god object — it holds both server-wide configuration (LLM providers, vault, tool definitions) and per-interaction mutable state (current role, session, agent, supervisor, inbox, tool tracker) in one `Arc<RwLock<Config>>`. CLI and REPL both mutate this singleton directly. Adding a third interface (REST API) that handles concurrent users makes this untenable.

## Design Pattern: Engine + Context + Emitter

The refactor splits Loki into three layers:

```
┌─────────┐  ┌─────────┐  ┌─────────┐
│   CLI   │  │  REPL   │  │   API   │   ← Thin adapters (frontends)
└────┬────┘  └────┬────┘  └────┬────┘
     │            │            │
     ▼            ▼            ▼
   ┌──────────────────────────────┐
   │     RunRequest + Emitter     │   ← Uniform request shape
   └──────────────┬───────────────┘
                  ▼
   ┌──────────────────────────────┐
   │          Engine::run()       │   ← Single core entrypoint
   │  (input → messages → LLM    │
   │   → tool loop → events)     │
   └──────────────┬───────────────┘
                  │
     ┌────────────┼────────────┐
     ▼            ▼            ▼
  AppState   RequestContext  SessionStore
  (global,   (per-request,  (file-backed,
   immutable) mutable)       per-session lock)
```

---

## 1. Split Config → AppState (global) + RequestContext (per-request)

### AppState — created once at startup, wrapped in `Arc`, never mutated during requests:

```rust
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,           // deserialized config.yaml (frozen)
    pub providers: ProviderRegistry,      // LLM client configs + OAuth tokens
    pub vault: Arc<VaultService>,         // encrypted credential storage (internal locking)
    pub tools: Arc<ToolRegistry>,         // tool definitions, function dirs, visible_tools
    pub mcp_global: Arc<McpGlobalConfig>, // global MCP settings (not live instances)
    pub sessions: Arc<dyn SessionStore>,  // file-backed session persistence
    pub rag_defaults: RagDefaults,        // embedding model, chunk size, etc.
}
```

### RequestContext — created per CLI invocation, per REPL turn, or per API request:

```rust
pub struct RequestContext {
    pub app: Arc<AppState>,               // borrows global state
    pub request_id: Uuid,
    pub mode: FrontendMode,               // Cli | Repl | Api
    pub cancel: CancellationToken,        // unified cancellation

    // per-request mutable state (was on Config)
    pub session: SessionHandle,
    pub convo: ConversationState,         // messages, last_message, tool_call_tracker
    pub agent: Option<AgentRuntime>,      // supervisor, MCP instances, inbox, escalation
    pub overrides: Overrides,             // model, role, rag, dry_run, etc.
    pub auth: Option<AuthContext>,        // API-only; None for CLI/REPL
}

pub struct Overrides {
    pub role: Option<String>,
    pub model: Option<String>,
    pub rag: Option<RagConfig>,
    pub agent: Option<AgentSpec>,
    pub dry_run: bool,
    pub macro_mode: bool,
}
```

### What changes for existing code

Every function that currently takes `&GlobalConfig` (i.e., `Arc<RwLock<Config>>`) and calls `.read()` / `.write()` gets refactored to take `&AppState` for reads and `&mut RequestContext` for mutations. The `config.write().set_model(...)` pattern becomes `ctx.overrides.model = Some(...)`.

### REPL special case

The REPL keeps a long-lived `RequestContext` that persists across turns (just like today's Config singleton does). State-changing dot-commands (`.model`, `.role`, `.session`) mutate the REPL's own context. This preserves current behavior exactly.

---

## 2. Unified Dispatch: The Engine

Instead of `start_directive()` in `main.rs` and `ask()` in `repl/mod.rs` being separate code paths, both call one core function:

```rust
pub struct Engine {
    pub app: Arc<AppState>,
    pub agent_factory: Arc<dyn AgentFactory>,
}

impl Engine {
    pub async fn run(
        &self,
        ctx: &mut RequestContext,
        req: RunRequest,
        emitter: &dyn Emitter,
    ) -> Result<RunOutcome, CoreError> {
        // 1. Apply any CoreCommand (set role, model, session, etc.)
        // 2. Build Input from req.input + ctx (role messages, session history, RAG)
        // 3. Create LLM client from provider registry
        // 4. call_chat_completions[_streaming](), emitting events via emitter
        // 5. Tool result loop (recursive)
        // 6. Persist session updates
        // 7. Return outcome (session_id, message_id)
    }
}

pub struct RunRequest {
    pub input: UserInput,                  // text, files, media
    pub command: Option<CoreCommand>,      // normalized dot-command
    pub stream: bool,
}

pub enum CoreCommand {
    SetRole(String),
    SetModel(String),
    StartSession { name: Option<String> },
    StartAgent { name: String, variables: HashMap<String, String> },
    Continue,
    Regenerate,
    CompressSession,
    Info,
    // ... one variant per REPL dot-command
}
```

### How frontends use it

| Frontend | Context lifetime | How it calls Engine |
|---|---|---|
| CLI | Single invocation, then exit | Creates `RequestContext`, calls `engine.run()` once, exits |
| REPL | Long-lived across turns | Keeps `RequestContext`, calls `engine.run()` per line, dot-commands become `CoreCommand` variants |
| API | Per HTTP request, but session persists | Loads `RequestContext` from `SessionStore` per request, calls `engine.run()`, persists back |

---

## 3. Output Abstraction: The Emitter Trait

The core never writes to stdout or formats JSON. It emits structured semantic events:

```rust
pub enum Event<'a> {
    Started { request_id: Uuid, session_id: Uuid },
    AssistantDelta(&'a str),              // streaming token
    AssistantMessageEnd { full_text: &'a str },
    ToolCall { name: &'a str, args: &'a str },
    ToolResult { name: &'a str, result: &'a str },
    Info(&'a str),
    Error(CoreError),
}

#[async_trait]
pub trait Emitter: Send + Sync {
    async fn emit(&self, event: Event<'_>) -> Result<(), EmitError>;
}
```

### Three implementations

- **`TerminalEmitter`** — wraps the existing `SseHandler` → `markdown_stream` / `raw_stream` logic. Renders to terminal with crossterm. Used by both CLI and REPL.
- **`JsonEmitter`** — collects all events, returns a JSON response body at the end. Used by non-streaming API requests.
- **`SseEmitter`** — converts each `Event` to an SSE frame, pushes into a `tokio::sync::mpsc` channel that axum streams to the client. Used by streaming API requests.

---

## 4. Session Isolation for API

### Session IDs

UUID-based for API consumers. CLI/REPL keep human-readable names as aliases.

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, alias: Option<&str>) -> Result<SessionHandle>;
    async fn open(&self, id: SessionId) -> Result<SessionHandle>;
    async fn open_by_name(&self, name: &str) -> Result<SessionHandle>;  // CLI/REPL compat
}
```

### File layout

```
~/.config/loki/sessions/
  by-id/<uuid>/state.yaml       # canonical storage
  by-name/<name> -> <uuid>      # symlink or mapping file for CLI/REPL
```

### Concurrency

Each `SessionHandle` holds a `tokio::sync::Mutex` so two concurrent API requests to the same session serialize properly. For v1 this is sufficient — no need for a database.

---

## 5. Tool Scope Isolation (formerly "Agent Isolation")

**Correction:** An earlier version of this document singled out agents as the owner of "live tool and MCP runtime." That was wrong. Loki allows MCP servers and tools to be configured at **every** `RoleLike` level — global, role, session, and agent — with resolution priority `Agent > Session > Role > Global`. Agents aren't uniquely coupled to MCP lifecycle; they're just the most visibly coupled scope in today's code.

The correct abstraction is **`ToolScope`**: every active `RoleLike` owns one. A `ToolScope` is a self-contained unit holding the resolved function declarations, live MCP runtime handles, and the tool-call tracker for whichever scope is currently on top of the stack.

### Today's behavior (to match in v1)

`McpRegistry::reinit()` is already **diff-based**: given a new enabled-server list, it stops only the servers that are no longer needed, leaves still-needed ones alive, and starts only the missing ones. This is correct single-tenant behavior but the registry is a process-wide singleton, so two concurrent consumers with different MCP sets trample each other.

### Target design

```rust
pub struct ToolScope {
    pub functions: Functions,              // resolved declarations for this scope
    pub mcp_runtime: McpRuntime,           // live handles to MCP processes
    pub tool_tracker: ToolCallTracker,     // per-scope call tracking
}

pub struct McpRuntime {
    servers: HashMap<String, Arc<McpServerHandle>>,  // live, ref-counted
}

pub struct McpFactory {
    shared_servers: Mutex<HashMap<McpServerKey, Weak<McpServerHandle>>>,
}

impl McpFactory {
    /// Produce a runtime with handles for the requested enabled servers.
    /// Shared across ToolScopes via Arc when configs match; isolated when they differ.
    pub async fn build_runtime(&self, enabled: &[String]) -> Result<McpRuntime>;
}
```

**`McpFactory` lives on `AppState`.** It does NOT hold any live servers itself — it holds weak refs so that when the last `ToolScope` using a given server drops its `Arc`, the process is torn down.

**`ToolScope` lives on `RequestContext`.** It replaces the current `functions`, `tool_call_tracker`, and (implicit) global `mcp_registry` fields. Every active scope — whether that's "just the REPL with its global MCP set" or "an agent with its own MCP set" — owns exactly one `ToolScope`.

### Scope transitions

When a `RoleLike` activates or exits:

1. Resolve the effective enabled-tool and enabled-MCP-server lists using priority `Agent > Session > Role > Global`.
2. Ask `McpFactory::build_runtime(enabled)` for an `McpRuntime`. The factory reuses existing `Arc<McpServerHandle>`s where keys match; spawns new processes where they don't.
3. Construct a new `ToolScope` with the runtime + resolved `Functions`.
4. Assign it to `ctx.tool_scope`. The old `ToolScope` drops; any `Arc<McpServerHandle>`s with no other references shut down their processes.

This preserves today's diff-based behavior for single-tenant (REPL) and makes it correct for multi-tenant (API).

### Sharing vs isolation (the key property)

`McpServerKey` encodes server name + command + args + env vars. Two `ToolScope`s requesting the **same key** share the same `Arc<McpServerHandle>`. Two requesting **different keys** (e.g., different per-user API keys baked into the env) get separate processes. This gives us:

- **Isolation by default** — different configs = different processes, no cross-tenant leakage
- **Sharing by coincidence** — identical configs = one process, ref-counted
- **Clean cleanup** — processes die automatically when the last scope releases them

### Agent-specific state

Agents still own some state that's genuinely agent-only (not in `ToolScope`): the supervisor, inbox, escalation queue, optional todo list, sub-agent handles, and the parent/child tree. That state lives in an `AgentRuntime`:

```rust
pub struct AgentRuntime {
    pub spec: AgentSpec,
    pub rag: Option<Arc<Rag>>,                   // shared across sibling sub-agents
    pub supervisor: Supervisor,
    pub inbox: Arc<Inbox>,
    pub escalation_queue: Arc<EscalationQueue>,  // root-shared for user interaction
    pub todo_list: Option<TodoList>,             // present only when auto_continue: true
    pub self_agent_id: String,
    pub parent_supervisor: Option<Arc<Supervisor>>,
    pub current_depth: usize,
    pub auto_continue_count: usize,
}
```

Three things to notice in this shape:

1. **`todo_list: Option<TodoList>`** — today's code eagerly allocates a `TodoList::default()` for every agent, but the todo tools and auto-continuation prompts are only exposed when `auto_continue: true`. Switching to `Option` lets us skip the allocation entirely for agents that don't opt in, and makes the "is this agent using todos?" question a type-level check rather than a config lookup. The semantics users see are unchanged.

2. **`rag: Option<Arc<Rag>>`** — agent RAG is an `Arc`, not an owned `Rag`. Today, every sub-agent of the same type independently calls `Rag::load()` and deserializes its own copy of the embeddings from disk. That means a parent spawning 4 parallel siblings of the same agent type pays the deserialize cost 5 times and holds 5 copies of identical vectors in memory. Sharing via `Arc` fixes both.

3. **No `mcp_runtime`** — MCP lives on `ToolScope`, not here. Agents get their tools through `ctx.tool_scope` like everyone else.

An `AgentRuntime` goes into `ctx.agent_runtime` **in addition to** the `ToolScope` — they're orthogonal concerns. An agent has both a `ToolScope` (its resolved tools + MCP) and an `AgentRuntime` (its supervision/messaging/RAG/todo state).

### RAG Cache (unified for standalone + agent RAG)

RAG in Loki comes from exactly two places today:

1. **Standalone RAG**, attached via the `.rag <name>` REPL command or the equivalent API call. Persists across role/session switches. Lives in `ctx.rag: Option<Arc<Rag>>`.
2. **Agent RAG**, loaded from the `documents:` field of an agent's `config.yaml` when the agent is activated. Lives in `ctx.agent_runtime.rag: Option<Arc<Rag>>` for the agent's lifetime.

Roles and Sessions do **not** own RAG — the `Role` and `Session` structs have no RAG fields. This is true today and the refactor preserves it.

Since both standalone and agent RAGs are ultimately `Arc<Rag>` instances loaded from disk YAML files, a single cache can serve both. `AppState` holds one:

```rust
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub vault: GlobalVault,
    pub mcp_factory: Arc<McpFactory>,
    pub rag_cache: Arc<RagCache>,
}

pub struct RagCache {
    entries: RwLock<HashMap<RagKey, Weak<Rag>>>,
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub enum RagKey {
    Named(String),   // standalone RAG: rags/<name>.yaml
    Agent(String),   // agent-owned RAG: agents/<name>/rag.yaml
}

impl RagCache {
    /// Returns a shared Arc<Rag> for the given key. If another scope
    /// holds a live reference, returns that exact Arc. Otherwise loads
    /// from disk, stores a Weak for future sharing, returns a fresh Arc.
    /// Concurrent first-load is serialized via per-key locks.
    pub async fn load(&self, key: &RagKey) -> Result<Option<Arc<Rag>>>;

    /// Invalidates the cache entry. Called by rebuild_rag / edit_rag_docs
    /// so the next load reads from disk. Does NOT affect existing Arc
    /// holders — they keep their old Rag until they drop it.
    pub fn invalidate(&self, key: &RagKey);
}
```

Why the enum: agent RAGs and standalone RAGs live at different paths on disk and could theoretically have overlapping names (an agent called "docs" and a standalone rag called "docs"). Keeping them in distinct namespaces avoids collisions and keeps the cache lookups unambiguous.

Why `Weak`: we don't want the cache to pin RAGs in memory forever. If no scope holds an `Arc<Rag>` for key X, the `Weak` becomes dangling, and the next `load()` reads fresh. "Share while in use, drop when nobody needs it" without a manual reaper.

**Concurrency wrinkle:** if two consumers request the same key at exactly the same time and neither finds a live entry, both will race to load from disk. Fix with per-key `tokio::sync::Mutex` or `once_cell::sync::OnceCell<Arc<Rag>>` — the second caller blocks briefly and receives the shared Arc.

**Invalidation:** both `rebuild_rag` and `edit_rag_docs` call `invalidate()` with the key corresponding to whichever RAG was being operated on (standalone or agent-owned). Existing `Arc<Rag>` holders keep their old reference until they drop it — which is the correct behavior, since you don't want a running request to suddenly see a partially-rebuilt index mid-execution.

### Where RAG attaches in `RequestContext`

Two distinct slots, two distinct purposes, one shared cache:

```rust
pub struct RequestContext {
    // ... other fields ...
    pub rag: Option<Arc<Rag>>,            // standalone RAG from `.rag <name>` or API equivalent
    pub agent_runtime: Option<AgentRuntime>,  // contains its own `rag: Option<Arc<Rag>>` when agent owns one
}
```

When resolving "what RAG should this request use", the engine checks `ctx.agent_runtime.rag` first (agent-owned takes precedence during an agent turn), then falls back to `ctx.rag` (the user's standalone selection). If neither is set, no RAG context is injected into the prompt.

**Behavior preservation:** today's code uses a single `Config.rag` slot that's overwritten by whichever action touched it most recently — `use_rag` and `use_agent` both clobber it. Exiting an agent leaves the overwrite in place; the user has to re-run `.rag <name>` to restore their standalone RAG. The new two-slot design gives us the opportunity to fix that (save `ctx.rag` into the `AgentRuntime` on activation, restore on exit) but **Phase 1 preserves today's clobber-and-forget behavior** to keep the refactor mechanical. The improvement is flagged as a Phase 2+ enhancement.

### Sub-agent spawning

Each child agent gets its **own** `RequestContext` forked from the parent's `Arc<AppState>`. That means each child gets:

- Its own `ToolScope` built from its agent.yaml's `mcp_servers` + `global_tools`, produced by `McpFactory`
- Its own `AgentRuntime` with a fresh supervisor, a fresh inbox, depth = parent.depth + 1
- A `parent_supervisor` reference pointing back at the parent's supervisor for escalation/messaging
- A shared `root_escalation_queue` cloned by `Arc` from the parent's runtime (one queue, one human at the root)
- A shared `rag: Option<Arc<Rag>>` via `AppState.rag_cache.load(RagKey::Agent(child_agent_name))` — if the parent already holds a strong ref, the cache returns the same Arc and no disk I/O happens

Because each child has its own `ToolScope`, **concurrent sub-agents can run with different MCP server sets simultaneously** — something today's singleton registry cannot do. The `McpFactory` pool handles overlap: if child A and child B both need `github` with matching keys, they share one `github` process via `Arc`.

Because sibling sub-agents of the same type share one `Arc<Rag>` through the unified cache, **RAG embeddings are loaded at most once per (standalone or agent) name per process**, regardless of how many siblings or concurrent API sessions reference the same name. The first holder keeps the embeddings warm for everyone else's lifetime, and they drop together once nobody holds a reference.

### MCP Lifecycle Policy (pooling and idle timeout)

`McpFactory` needs an eviction policy so long-running server processes don't accumulate idle MCP subprocesses indefinitely. The design is a two-layer scheme:

```rust
pub struct McpFactory {
    active: Mutex<HashMap<McpServerKey, Weak<McpServerHandle>>>,
    idle: Mutex<HashMap<McpServerKey, IdleEntry>>,
    config: McpFactoryConfig,
}

struct IdleEntry {
    handle: Arc<McpServerHandle>,
    idle_since: Instant,
}

pub struct McpFactoryConfig {
    pub idle_timeout: Duration,              // how long idle servers stay warm
    pub cleanup_interval: Duration,          // how often the reaper runs
    pub max_idle_servers: Option<usize>,     // LRU cap (None = unbounded)
}
```

**Layer 1 — active references via Arc.** Scopes currently using a server hold `Arc<McpServerHandle>`. Standard Rust refcounting. Any live reference keeps the process running, regardless of timers.

**Layer 2 — idle grace period via LRU eviction.** When the last active scope drops its Arc, a custom `Drop` impl on the handle moves it into the idle pool with a timestamp instead of tearing it down immediately. A background reaper task wakes on `cleanup_interval` and evicts entries whose idle time exceeds `idle_timeout`, calling `cancel().await` on the actual MCP subprocess.

Acquisition order on every scope transition:

```rust
impl McpFactory {
    pub async fn acquire(&self, key: &McpServerKey) -> Result<Arc<McpServerHandle>> {
        // 1. Someone else is actively using it — share.
        if let Some(arc) = self.try_reuse_active(key) { return Ok(arc); }
        // 2. Sitting in the idle pool — revive it, zero startup cost.
        if let Some(arc) = self.revive_from_idle(key) { return Ok(arc); }
        // 3. Neither — spawn fresh.
        self.spawn_new(key).await
    }
}
```

**Sensible defaults by deployment mode:**

| Mode | `idle_timeout` default | Rationale |
|---|---|---|
| CLI one-shot | N/A (process exits, everything dies) | No pooling needed |
| REPL | `0` (immediate drop) | Matches today's reactive reinit behavior |
| API server | `5 minutes` | Absorbs burst traffic, caps stale resources |

These are defaults, not mandates. Users should be able to override globally and per-server:

```yaml
# config.yaml
mcp_pool:
  idle_timeout_seconds: 300
  cleanup_interval_seconds: 30
  max_idle_servers: 50
```

```json
// functions/mcp.json
{
  "github":     { "command": "...", "idle_timeout_seconds": 900 },
  "filesystem": { "command": "...", "idle_timeout_seconds": 60 }
}
```

**Optional health checks.** While a handle sits in the idle pool, the reaper can optionally ping it via `tools/list`. If a server has crashed or become unresponsive, it's evicted immediately. Without this, a stale idle entry would make the first real request after revival fail. Worth implementing, but not strictly required for v1.

**Graceful shutdown.** On server shutdown, drain active scopes (let in-flight LLM calls complete or cancel via token), then tear down the idle pool. Give it a bounded drain timeout before force-killing. Especially important for MCP servers holding external transactions or locks.

**Per-tenant isolation.** `McpServerKey` includes env vars in its hash, so two tenants with different `GITHUB_TOKEN`s get distinct keys and therefore distinct processes. Zero cross-tenant leakage by construction.

### Phasing

Phase 1 ships `McpFactory` without the pool — just `acquire()` that always spawns fresh, `Drop` that always tears down. This is correct but inefficient. Phase 5 adds the idle pool, reaper task, health checks, and configuration knobs. Splitting it this way keeps Phase 1 focused on the state split (its actual goal) and Phase 5 focused on the pooling optimization (where it has a clear performance target: warm-path MCP tool calls should have near-zero overhead).

### Lifecycle summary

| Frontend | ToolScope lifetime | AgentRuntime lifetime | RAG lifetime |
|---|---|---|---|
| **CLI one-shot** | One invocation | One invocation (if `--agent`) | One invocation |
| **REPL** | Long-lived, rebuilt on `.role` / `.session` / `.agent` / `.set enabled_mcp_servers` | Lives from `.agent X` until `.exit agent` | Standalone RAG set via `.rag <name>` persists across role/session switches; agent RAG lives as long as the `AgentRuntime`; both come from the shared `RagCache` |
| **API session** | Lives while session is "warm"; rebuilt when client changes role/session/agent | Lives while session is "warm" | Same as REPL; `RagCache` shares `Arc<Rag>`s across concurrent sessions using the same RAG name |
| **Sub-agent (any frontend)** | Lives for the sub-agent task | Lives for the sub-agent task | Shared via `Arc` with parent and siblings through `RagCache` |

---

## 6. Cross-Cutting Concerns

| Concern | Pattern | CLI | REPL | API |
|---|---|---|---|---|
| **Errors** | Core returns `CoreError` enum; frontends map | `render_error()` to stderr | `render_error()` to terminal | `{ "error": { "code": "...", "message": "..." } }` JSON |
| **Cancellation** | `CancellationToken` in `RequestContext` | Ctrl-C handler triggers token | Ctrl-C triggers token | Client disconnect / request timeout triggers token |
| **Auth** | Middleware sets `AuthContext` on `RequestContext` | None (local user) | None (local user) | Bearer token / API key validated by axum middleware |
| **Tracing** | `tracing::Span` per request with request_id, session_id, mode | Log to file | Log to file | Log to file + structured JSON logs |

### Error type

```rust
pub enum CoreError {
    InvalidRequest { msg: String },
    NotFound { msg: String },
    Unauthorized { msg: String },
    Forbidden { msg: String },
    Timeout { msg: String },
    Cancelled,
    Provider { msg: String },
    Tool { msg: String },
    Io { msg: String },
}
```

### Cancellation

Use a `CancellationToken` in `RequestContext`. The core checks it via `tokio::select!` around long awaits (LLM stream, tool execution, MCP IO).

- CLI/REPL: Ctrl-C handler triggers token.
- API: axum provides disconnect detection for SSE/streaming; when the client drops, cancel the token.
- Timeouts: set deadline and translate to token cancellation.

### Auth (API-only initially)

axum middleware authenticates (API key / bearer token), builds `AuthContext`, stores in request extensions, then the handler copies it into `RequestContext`. Core enforces policy only when executing sensitive operations (tools, filesystem, vault).

```rust
pub struct AuthContext {
    pub subject: String,
    pub scopes: Vec<String>,
}
```

---

## 7. API Endpoint Design

```
POST   /v1/completions                    # one-shot prompt (no session)
POST   /v1/sessions                       # create session
POST   /v1/sessions/:id/completions       # prompt within session
DELETE /v1/sessions/:id                   # close session
POST   /v1/sessions/:id/agent             # activate agent on session
DELETE /v1/sessions/:id/agent             # deactivate agent
POST   /v1/sessions/:id/role              # set role on session
POST   /v1/sessions/:id/rag              # attach RAG to session
GET    /v1/models                         # list available models
GET    /v1/agents                         # list available agents
GET    /v1/roles                          # list available roles
```

### Request body for completions

```json
{
  "prompt": "Explain TCP handshake",
  "model": "openai:gpt-4o",
  "stream": true,
  "files": ["path/to/doc.pdf"],
  "role": "explain"
}
```

---

## 8. Implementation Phases

| Phase | Scope | Effort | Risk |
|---|---|---|---|
| **Phase 1: Extract AppState** | Split Config into AppState (global) + per-request state. Keep CLI/REPL working exactly as before. No API yet. | ~1-2 weeks | Medium — touching every file that uses GlobalConfig |
| **Phase 2: Introduce Engine + Emitter** | Unify `start_directive()` and `ask()` behind `Engine::run()`. Create `TerminalEmitter`. CLI/REPL now call Engine. | ~1 week | Low — refactoring existing paths |
| **Phase 3: SessionStore abstraction** | Extract session persistence behind trait. Add UUID-based sessions. CLI/REPL still use name-based aliases. | ~3-5 days | Low |
| **Phase 4: REST API server** | Add `--serve` flag. axum handlers that create `RequestContext`, call `Engine::run()`, return JSON/SSE. Basic auth middleware. | ~1-2 weeks | Low — clean layer on top of Engine |
| **Phase 5: Agent isolation** | Move agent runtime into `RequestContext`. `AgentFactory` creates isolated runtimes per session. | ~1 week | Medium — MCP server lifecycle mgmt |
| **Phase 6: Production hardening** | Rate limiting, proper auth, request validation, health checks, graceful shutdown, deployment configs. | ~1 week | Low |

**Total estimate: ~5-7 weeks** for a production-ready v1.

### Key Risk: Phase 1

Phase 1 is the hardest and riskiest — it touches nearly every module. The mitigation is to do it incrementally: first add `AppState` alongside existing `Config`, then migrate callers module by module, then remove the old `GlobalConfig` type alias. Tests should pass at every intermediate step.

---

## Key Design Decisions & Trade-offs

1. **Eliminates the singleton mutation bottleneck**: concurrency becomes "multiple `RequestContext`s" rather than fighting over `RwLock<Config>`.
2. **Preserves current behavior**: REPL can keep "state-changing commands" by mutating its own long-lived `RequestContext` + persisted `SessionState`.
3. **Streaming becomes portable**: terminal rendering, JSON, and SSE are just different `Emitter`s over the same event stream.
4. **Agent/MCP isolation is explicit**: prevents cross-session conflicts by construction.

## Watch Out For

1. **Persisted vs in-memory drift**: decide which fields live in `SessionState` vs `ConversationState`; persist only what must survive process restarts.
2. **Per-session concurrency semantics**: either serialize requests per session (simplest) or carefully merge message histories; v1 should serialize.
3. **MCP process lifecycle**: if you keep MCP servers alive across requests, tie them to a session runtime and clean them up on session close/TTL.

## Future Considerations

1. Swap file store behind `SessionStore` with sqlite without changing core.
2. Add a stable public API schema for events so clients can render rich tool-call UIs.
3. Actor model (one tokio task per session receiving commands via mpsc) for simplified session+agent lifetime management.
