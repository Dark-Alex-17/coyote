# Phase 1 Implementation Plan: Extract AppState from Config

## Overview

Split the monolithic `Config` struct into:
- **`AppConfig`** — immutable server-wide settings (deserialized from `config.yaml`)
- **`RequestContext`** — per-request mutable state (current role, session, agent, supervisor, etc.)

The existing `GlobalConfig` (`Arc<RwLock<Config>>`) type alias is replaced. CLI and REPL continue working identically. No API code is added in this phase.

**Estimated effort:** ~3-4 weeks (originally estimated 1-2 weeks; revised during implementation as Steps 6.5 and 7 deferred their semantic rewrites to an expanded Step 8)
**Risk:** Medium — touches 91 callsites across 15 modules
**Mitigation:** Incremental migration with tests passing at every step
**Sub-step tracking:** Each step has per-step implementation notes in `docs/implementation/PHASE-1-STEP-*-NOTES.md`

---

## Current State: Config Field Classification

### Serialized Fields (from config.yaml → AppConfig)

These are loaded from disk once and should be immutable during request processing:

| Field | Type | Notes |
|---|---|---|
| `model_id` | `String` | Default model ID |
| `temperature` | `Option<f64>` | Default temperature |
| `top_p` | `Option<f64>` | Default top_p |
| `dry_run` | `bool` | Can be overridden per-request |
| `stream` | `bool` | Can be overridden per-request |
| `save` | `bool` | Whether to persist to messages.md |
| `keybindings` | `String` | REPL keybinding style |
| `editor` | `Option<String>` | Editor command |
| `wrap` | `Option<String>` | Text wrapping |
| `wrap_code` | `bool` | Code block wrapping |
| `vault_password_file` | `Option<PathBuf>` | Vault password location |
| `function_calling_support` | `bool` | Global function calling toggle |
| `mapping_tools` | `IndexMap<String, String>` | Tool aliases |
| `enabled_tools` | `Option<String>` | Default enabled tools |
| `visible_tools` | `Option<Vec<String>>` | Visible tool list |
| `mcp_server_support` | `bool` | Global MCP toggle |
| `mapping_mcp_servers` | `IndexMap<String, String>` | MCP server aliases |
| `enabled_mcp_servers` | `Option<String>` | Default enabled MCP servers |
| `repl_prelude` | `Option<String>` | REPL prelude config |
| `cmd_prelude` | `Option<String>` | CLI prelude config |
| `agent_session` | `Option<String>` | Default agent session |
| `save_session` | `Option<bool>` | Session save behavior |
| `compression_threshold` | `usize` | Session compression threshold |
| `summarization_prompt` | `Option<String>` | Compression prompt |
| `summary_context_prompt` | `Option<String>` | Summary context prompt |
| `rag_embedding_model` | `Option<String>` | RAG embedding model |
| `rag_reranker_model` | `Option<String>` | RAG reranker model |
| `rag_top_k` | `usize` | RAG top-k results |
| `rag_chunk_size` | `Option<usize>` | RAG chunk size |
| `rag_chunk_overlap` | `Option<usize>` | RAG chunk overlap |
| `rag_template` | `Option<String>` | RAG template |
| `document_loaders` | `HashMap<String, String>` | Document loader mappings |
| `highlight` | `bool` | Syntax highlighting |
| `theme` | `Option<String>` | Color theme |
| `left_prompt` | `Option<String>` | REPL left prompt format |
| `right_prompt` | `Option<String>` | REPL right prompt format |
| `user_agent` | `Option<String>` | HTTP User-Agent |
| `save_shell_history` | `bool` | Shell history persistence |
| `sync_models_url` | `Option<String>` | Models sync URL |
| `clients` | `Vec<ClientConfig>` | LLM provider configs |

### Runtime Fields (#[serde(skip)] → RequestContext)

These are created at runtime and are per-request/per-session mutable state:

| Field | Type | Destination |
|---|---|---|
| `vault` | `GlobalVault` | `AppState.vault` (shared service) |
| `macro_flag` | `bool` | `RequestContext.macro_flag` |
| `info_flag` | `bool` | `RequestContext.info_flag` |
| `agent_variables` | `Option<AgentVariables>` | `RequestContext.agent_variables` |
| `model` | `Model` | `RequestContext.model` |
| `functions` | `Functions` | `RequestContext.tool_scope.functions` (unified in Step 6) |
| `mcp_registry` | `Option<McpRegistry>` | **REMOVED.** Replaced by per-`ToolScope` `McpRuntime`s produced by a new `McpFactory` on `AppState`. See the architecture doc's "Tool Scope Isolation" section. |
| `working_mode` | `WorkingMode` | `RequestContext.working_mode` |
| `last_message` | `Option<LastMessage>` | `RequestContext.last_message` |
| `role` | `Option<Role>` | `RequestContext.role` |
| `session` | `Option<Session>` | `RequestContext.session` |
| `rag` | `Option<Arc<Rag>>` | `RequestContext.rag` |
| `agent` | `Option<Agent>` | `RequestContext.agent` (agent spec + role + RAG) |
| `tool_call_tracker` | `Option<ToolCallTracker>` | `RequestContext.tool_scope.tool_tracker` (unified in Step 6) |
| `supervisor` | `Option<Arc<RwLock<Supervisor>>>` | `RequestContext.agent_runtime.supervisor` |
| `parent_supervisor` | `Option<Arc<RwLock<Supervisor>>>` | `RequestContext.agent_runtime.parent_supervisor` |
| `self_agent_id` | `Option<String>` | `RequestContext.agent_runtime.self_agent_id` |
| `current_depth` | `usize` | `RequestContext.agent_runtime.current_depth` |
| `inbox` | `Option<Arc<Inbox>>` | `RequestContext.agent_runtime.inbox` |
| `root_escalation_queue` | `Option<Arc<EscalationQueue>>` | `RequestContext.agent_runtime.escalation_queue` (shared from the root via `Arc`) |

**Note on `ToolScope` and `AgentRuntime`:** during Phase 1 Step 0 the new `RequestContext` struct keeps `functions`, `tool_call_tracker`, supervisor/inbox/escalation fields as **flat fields** mirroring today's `Config`. This is deliberate — it makes the field-by-field migration mechanical. In Step 6.5 these fields collapse into two sub-structs:

- `ToolScope { functions, mcp_runtime, tool_tracker }` — owned by every active `RoleLike` scope, rebuilt on role/session/agent transitions via `McpFactory::acquire()`.
- `AgentRuntime { spec, rag, supervisor, inbox, escalation_queue, todo_list, self_agent_id, parent_supervisor, current_depth, auto_continue_count }` — owned only when an agent is active.

**Two behavior changes land during Step 6.5** that tighten today's code:

1. `todo_list` becomes `Option<TodoList>`. Today the code always allocates `TodoList::default()` for every agent, even when `auto_continue: false`. Since the todo tools and instructions are only exposed when `auto_continue: true`, the allocation is wasted. The new shape skips allocation unless the agent opts in. No user-visible change.

2. A unified `RagCache` on `AppState` serves **both** standalone RAGs (attached via `.rag <name>`) and agent-owned RAGs (loaded from an agent's `documents:` field). Today, both paths independently call `Rag::load` from disk on every use; with the cache, any scope requesting the same `RagKey` shares the same `Arc<Rag>`. Standalone RAG lives in `ctx.rag`; agent RAG lives in `ctx.agent_runtime.rag`. Roles and Sessions do **not** own RAG (the structs have no RAG fields) — this is true today and unchanged by the refactor. `rebuild_rag` and `edit_rag_docs` call `RagCache::invalidate()`.

See `docs/REST-API-ARCHITECTURE.md` section 5 for the full `ToolScope`, `McpFactory`, `RagCache`, and MCP pooling designs.

---

## Migration Strategy: The Facade Pattern

**Do NOT rewrite everything at once.** Instead, use a transitional facade that keeps the old `Config` working while new code uses the split types.

### Step 0: Add new types alongside Config (no breaking changes)  ✅ DONE

Create the new structs in new files. `Config` stays untouched. Nothing breaks.

**Files created:**
- `src/config/app_config.rs` — `AppConfig` struct (the serialized half)
- `src/config/request_context.rs` — `RequestContext` struct (the runtime half)
- `src/config/app_state.rs` — `AppState` struct (Arc-wrapped global services, no `mcp_registry` — see below)

**`AppConfig`** is essentially the current `Config` struct but containing ONLY the serialized fields (no `#[serde(skip)]` fields). It should derive `Deserialize` identically so the existing `config.yaml` still loads.

**Important change from the original plan:** `AppState` does NOT hold an `McpRegistry`. MCP server processes are scoped per `RoleLike`, not process-wide. An `McpFactory` service will be added to `AppState` in Step 6.5. See `docs/REST-API-ARCHITECTURE.md` section 5 for the design rationale.

```rust
// src/config/app_config.rs
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(rename(serialize = "model", deserialize = "model"))]
    pub model_id: String,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub dry_run: bool,
    pub stream: bool,
    pub save: bool,
    pub keybindings: String,
    pub editor: Option<String>,
    pub wrap: Option<String>,
    pub wrap_code: bool,
    vault_password_file: Option<PathBuf>,
    pub function_calling_support: bool,
    pub mapping_tools: IndexMap<String, String>,
    pub enabled_tools: Option<String>,
    pub visible_tools: Option<Vec<String>>,
    pub mcp_server_support: bool,
    pub mapping_mcp_servers: IndexMap<String, String>,
    pub enabled_mcp_servers: Option<String>,
    pub repl_prelude: Option<String>,
    pub cmd_prelude: Option<String>,
    pub agent_session: Option<String>,
    pub save_session: Option<bool>,
    pub compression_threshold: usize,
    pub summarization_prompt: Option<String>,
    pub summary_context_prompt: Option<String>,
    pub rag_embedding_model: Option<String>,
    pub rag_reranker_model: Option<String>,
    pub rag_top_k: usize,
    pub rag_chunk_size: Option<usize>,
    pub rag_chunk_overlap: Option<usize>,
    pub rag_template: Option<String>,
    pub document_loaders: HashMap<String, String>,
    pub highlight: bool,
    pub theme: Option<String>,
    pub left_prompt: Option<String>,
    pub right_prompt: Option<String>,
    pub user_agent: Option<String>,
    pub save_shell_history: bool,
    pub sync_models_url: Option<String>,
    pub clients: Vec<ClientConfig>,
}
```

```rust
// src/config/app_state.rs
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub vault: GlobalVault,
    // NOTE: no `mcp_registry` field. MCP runtime is scoped per-`ToolScope`
    // on `RequestContext`, not process-wide. An `McpFactory` will be added
    // here later (Step 6 / Phase 5) to pool and ref-count MCP processes
    // across concurrent ToolScopes. See architecture doc section 5.
}
```

```rust
// src/config/request_context.rs
pub struct RequestContext {
    pub app: Arc<AppState>,

    // per-request flags
    pub macro_flag: bool,
    pub info_flag: bool,
    pub working_mode: WorkingMode,

    // active context
    pub model: Model,
    pub functions: Functions,
    pub role: Option<Role>,
    pub session: Option<Session>,
    pub rag: Option<Arc<Rag>>,
    pub agent: Option<Agent>,
    pub agent_variables: Option<AgentVariables>,

    // conversation state
    pub last_message: Option<LastMessage>,
    pub tool_call_tracker: Option<ToolCallTracker>,

    // agent supervision
    pub supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub parent_supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub self_agent_id: Option<String>,
    pub current_depth: usize,
    pub inbox: Option<Arc<Inbox>>,
    pub root_escalation_queue: Option<Arc<EscalationQueue>>,
}
```

### Step 1: Make Config constructible from AppConfig + RequestContext

Add conversion methods so the old `Config` can be built from the new types, and vice versa. This is the bridge that lets us migrate incrementally.

```rust
// On Config:
impl Config {
    /// Extract the global portion into AppConfig
    pub fn to_app_config(&self) -> AppConfig { /* copy serialized fields */ }

    /// Extract the runtime portion into RequestContext
    pub fn to_request_context(&self, app: Arc<AppState>) -> RequestContext { /* copy runtime fields */ }

    /// Reconstruct Config from the split types (for backward compat during migration)
    pub fn from_parts(app: &AppState, ctx: &RequestContext) -> Config { /* merge back */ }
}
```

**Test:** After this step, `Config::from_parts(config.to_app_config(), config.to_request_context())` round-trips correctly. Existing tests still pass.

### Step 2: Migrate static methods off Config

There are ~30 static methods on Config (no `self` parameter). These are pure utility functions that don't need Config at all — they compute file paths, list directories, etc.

**Target:** Move these to a standalone `paths` module or keep on `AppConfig` where appropriate.

| Method | Move to |
|---|---|
| `config_dir()` | `paths::config_dir()` |
| `local_path(name)` | `paths::local_path(name)` |
| `cache_path()` | `paths::cache_path()` |
| `oauth_tokens_path()` | `paths::oauth_tokens_path()` |
| `token_file(client)` | `paths::token_file(client)` |
| `log_path()` | `paths::log_path()` |
| `config_file()` | `paths::config_file()` |
| `roles_dir()` | `paths::roles_dir()` |
| `role_file(name)` | `paths::role_file(name)` |
| `macros_dir()` | `paths::macros_dir()` |
| `macro_file(name)` | `paths::macro_file(name)` |
| `env_file()` | `paths::env_file()` |
| `rags_dir()` | `paths::rags_dir()` |
| `functions_dir()` | `paths::functions_dir()` |
| `functions_bin_dir()` | `paths::functions_bin_dir()` |
| `mcp_config_file()` | `paths::mcp_config_file()` |
| `global_tools_dir()` | `paths::global_tools_dir()` |
| `global_utils_dir()` | `paths::global_utils_dir()` |
| `bash_prompt_utils_file()` | `paths::bash_prompt_utils_file()` |
| `agents_data_dir()` | `paths::agents_data_dir()` |
| `agent_data_dir(name)` | `paths::agent_data_dir(name)` |
| `agent_config_file(name)` | `paths::agent_config_file(name)` |
| `agent_bin_dir(name)` | `paths::agent_bin_dir(name)` |
| `agent_rag_file(agent, rag)` | `paths::agent_rag_file(agent, rag)` |
| `agent_functions_file(name)` | `paths::agent_functions_file(name)` |
| `models_override_file()` | `paths::models_override_file()` |
| `list_roles(with_builtin)` | `Role::list(with_builtin)` or `paths` |
| `list_rags()` | `Rag::list()` or `paths` |
| `list_macros()` | `Macro::list()` or `paths` |
| `has_role(name)` | `Role::exists(name)` |
| `has_macro(name)` | `Macro::exists(name)` |
| `sync_models(url, abort)` | Standalone function or on `AppConfig` |
| `local_models_override()` | Standalone function |
| `log_config()` | Standalone function |

**Approach:** Create `src/config/paths.rs`, move functions there, and add `#[deprecated]` forwarding methods on `Config` that call the new locations. Compile, run tests, fix callsites module by module, then remove the deprecated methods.

**Callsite count:** Low — most of these are called from 1-3 places. This is a quick-win step.

### Step 3: Migrate global-read methods to AppConfig

These methods only read serialized config values and should live on `AppConfig`:

| Method | Current Signature | New Home |
|---|---|---|
| `vault_password_file` | `&self -> PathBuf` | `AppConfig` |
| `editor` | `&self -> Result<String>` | `AppConfig` |
| `sync_models_url` | `&self -> String` | `AppConfig` |
| `light_theme` | `&self -> bool` | `AppConfig` |
| `render_options` | `&self -> Result<RenderOptions>` | `AppConfig` |
| `print_markdown` | `&self, text -> Result<()>` | `AppConfig` |
| `rag_template` | `&self, embeddings, sources, text -> String` | `AppConfig` |
| `select_functions` | `&self, role -> Option<Vec<...>>` | `AppConfig` |
| `select_enabled_functions` | `&self, role -> Vec<...>` | `AppConfig` |
| `select_enabled_mcp_servers` | `&self, role -> Vec<...>` | `AppConfig` |

**Same pattern:** Add new methods on `AppConfig`, add `#[deprecated]` forwarding on `Config`, migrate callers, remove.

### Step 4: Migrate global-write methods

These modify serialized config settings (`.set` command, environment loading):

| Method | Notes |
|---|---|
| `set_wrap` | Modifies `self.wrap` |
| `update` | Generic key-value update of config settings |
| `load_envs` | Applies env var overrides |
| `load_functions` | Initializes function definitions |
| `load_mcp_servers` | Starts MCP servers |
| `setup_model` | Sets default model |
| `setup_document_loaders` | Sets default doc loaders |
| `setup_user_agent` | Sets user agent string |

The `load_*` / `setup_*` methods are initialization-only (called once in `Config::init`). They become part of `AppState` construction.

`update` and `set_wrap` are runtime mutations of global config. For the API world, these should require a config reload. For now, they can stay as methods that mutate `AppConfig` through interior mutability or require a mutable reference during REPL setup.

### Step 5: Migrate request-read methods to RequestContext

Pure reads of per-request state:

| Method | Notes |
|---|---|
| `state` | Returns flags for active role/session/agent/rag |
| `messages_file` | Path depends on active agent |
| `sessions_dir` | Path depends on active agent |
| `session_file` | Path depends on active agent |
| `rag_file` | Path depends on active agent |
| `info` | Reads current agent/session/role/rag |
| `role_info` | Reads current role |
| `session_info` | Reads current session |
| `agent_info` | Reads current agent |
| `agent_banner` | Reads current agent |
| `rag_info` | Reads current rag |
| `list_sessions` | Depends on sessions_dir (agent context) |
| `list_autoname_sessions` | Depends on sessions_dir |
| `is_compressing_session` | Reads session state |
| `role_like_mut` | Returns mutable ref to role-like |

### Step 6: Migrate request-write methods to RequestContext

Mutations of per-request state:

| Method | Notes |
|---|---|
| `use_prompt` | Sets temporary role |
| `use_role` / `use_role_obj` | Sets role on session or self |
| `exit_role` | Clears role |
| `edit_role` | Edits and re-applies role |
| `use_session` | Sets session |
| `exit_session` | Saves and clears session |
| `save_session` | Persists session |
| `empty_session` | Clears session messages |
| `set_save_session_this_time` | Session flag |
| `compress_session` / `maybe_compress_session` | Session compression |
| `autoname_session` / `maybe_autoname_session` | Session naming |
| `use_rag` / `exit_rag` / `edit_rag_docs` / `rebuild_rag` | RAG lifecycle |
| `use_agent` / `exit_agent` / `exit_agent_session` | Agent lifecycle |
| `apply_prelude` | Sets role/session from prelude config |
| `before_chat_completion` | Pre-LLM state updates |
| `after_chat_completion` | Post-LLM state updates |
| `discontinuous_last_message` | Message state |
| `init_agent_shared_variables` | Agent vars |
| `init_agent_session_variables` | Agent session vars |

### Step 6.5: Unify tool/MCP fields into `ToolScope` and agent fields into `AgentRuntime`

After Step 6, `RequestContext` has many flat fields that logically cluster into two sub-structs. This step collapses them and introduces three new services on `AppState`.

**New types:**

```rust
pub struct ToolScope {
    pub functions: Functions,
    pub mcp_runtime: McpRuntime,
    pub tool_tracker: ToolCallTracker,
}

pub struct McpRuntime {
    servers: HashMap<String, Arc<McpServerHandle>>,
}

pub struct AgentRuntime {
    pub spec: AgentSpec,
    pub rag: Option<Arc<Rag>>,                   // shared across siblings of same type
    pub supervisor: Supervisor,
    pub inbox: Arc<Inbox>,
    pub escalation_queue: Arc<EscalationQueue>,  // shared from root
    pub todo_list: Option<TodoList>,             // Some(...) only when auto_continue: true
    pub self_agent_id: String,
    pub parent_supervisor: Option<Arc<Supervisor>>,
    pub current_depth: usize,
    pub auto_continue_count: usize,
}
```

**New services on `AppState`:**

```rust
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub vault: GlobalVault,
    pub mcp_factory: Arc<McpFactory>,
    pub rag_cache: Arc<RagCache>,
}

pub struct McpFactory {
    active: Mutex<HashMap<McpServerKey, Weak<McpServerHandle>>>,
    // idle pool + reaper added in Phase 5; Step 6.5 ships the no-pool version
}

impl McpFactory {
    pub async fn acquire(&self, key: &McpServerKey) -> Result<Arc<McpServerHandle>>;
}

pub struct RagCache {
    entries: RwLock<HashMap<RagKey, Weak<Rag>>>,
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub enum RagKey {
    Named(String),   // standalone: rags/<name>.yaml
    Agent(String),   // agent-owned: agents/<name>/rag.yaml
}

impl RagCache {
    pub async fn load(&self, key: &RagKey) -> Result<Option<Arc<Rag>>>;
    pub fn invalidate(&self, key: &RagKey);
}
```

**`RequestContext` after collapse:**

```rust
pub struct RequestContext {
    pub app: Arc<AppState>,
    pub macro_flag: bool,
    pub info_flag: bool,
    pub working_mode: WorkingMode,
    pub model: Model,
    pub agent_variables: Option<AgentVariables>,

    pub role: Option<Role>,
    pub session: Option<Session>,
    pub rag: Option<Arc<Rag>>,  // session/standalone RAG, not agent RAG
    pub agent: Option<Agent>,

    pub last_message: Option<LastMessage>,

    pub tool_scope: ToolScope,                // replaces functions + tool_call_tracker + global mcp_registry
    pub agent_runtime: Option<AgentRuntime>,  // replaces supervisor + inbox + escalation_queue + todo + self_id + parent + depth; holds shared agent RAG
}
```

**What this step does:**

1. Implement `McpRuntime` and `ToolScope`.
2. Implement `McpFactory` — **no pool, no idle handling, no reaper.** `acquire()` checks `active` for an upgradable `Weak`, otherwise spawns fresh. `Drop` on `McpServerHandle` tears down the subprocess directly. Pooling lands in Phase 5.
3. Implement `RagCache` with `RagKey` enum, weak-ref sharing, and per-key serialization for concurrent first-load.
4. Implement `AgentRuntime` with the shape above. `todo_list` is `Option` — only allocated when `agent.spec.auto_continue == true`. `rag` is served from `RagCache` during activation via `RagKey::Agent(name)`.
5. Rewrite scope transitions (`use_role`, `use_session`, `use_agent`, `exit_*`, `Config::update`) to:
   - Resolve the effective enabled-tool / enabled-MCP-server list using priority `Agent > Session > Role > Global`
   - Build a fresh `McpRuntime` by calling `McpFactory::acquire()` for each required server key
   - Construct a new `ToolScope` wrapping the runtime + resolved `Functions`
   - Swap `ctx.tool_scope` atomically
6. `use_rag` (standalone / `.rag <name>` path) is rewritten to call `app.rag_cache.load(RagKey::Named(name))` and assign the result to `ctx.rag`. No role/session RAG changes because roles/sessions do not own RAG.
7. Agent activation additionally:
   - Calls `app.rag_cache.load(RagKey::Agent(agent_name))` and stores the returned `Arc<Rag>` in the new `AgentRuntime.rag`
   - Allocates `todo_list: Some(TodoList::default())` only when `auto_continue: true`; otherwise `None`
   - Constructs the `AgentRuntime` and assigns it to `ctx.agent_runtime`
   - **Preserves today's clobber behavior for standalone RAG:** does NOT save `ctx.rag` anywhere. When the agent exits, the user's previous `.rag <name>` selection is not restored (matches current behavior). Stacking / restoration is flagged as a Phase 2+ enhancement.
8. `exit_agent` drops `ctx.agent_runtime` (which drops the agent's `Arc<Rag>`; the cache entry becomes evictable if no other scope holds it) and rebuilds `ctx.tool_scope` from the now-topmost `RoleLike`.
9. Sub-agent spawning (in `function/supervisor.rs`) constructs a fresh `RequestContext` for the child from the shared `AppState`:
   - Its own `ToolScope` via `McpFactory::acquire()` calls for the child agent's `mcp_servers`
   - Its own `AgentRuntime` with:
     - `rag` via `rag_cache.load(RagKey::Agent(child_agent_name))` — shared with parent/siblings of same type
     - Fresh `Supervisor`, fresh `Inbox`, `current_depth = parent.depth + 1`
     - `parent_supervisor = Some(parent.supervisor.clone())` (for messaging)
     - `escalation_queue = parent.escalation_queue.clone()` — one queue, rooted at the human
     - `todo_list` honoring the child's own `auto_continue` flag
10. Old `Agent::init` logic that mutates a global `McpRegistry` is removed — that's now `McpFactory::acquire()` producing scope-local handles.
11. `rebuild_rag` and `edit_rag_docs` are updated to determine the correct `RagKey` (check `ctx.agent_runtime` first — if present use `RagKey::Agent(spec.name)`, otherwise use the standalone name from `ctx.rag`'s origin) and call `rag_cache.invalidate(&key)` before reloading.

**What this step preserves:**

- **Diff-based reinit for REPL users** — when you `.exit role` from `[github, jira]` back to global `[github]`, the new `ToolScope` is built by calling `McpFactory::acquire("github")`. Without pooling (Phase 1), this respawns `github`. With pooling (Phase 5), `github`'s `Arc` is still held by no one, but the idle pool keeps it warm, so revival is instant. The Phase 1 version has a mild regression here that Phase 5 fixes.
- **Agent-vs-non-agent compatibility** — today's `Agent::init` reinits a global registry; after this step, agent activation replaces `ctx.tool_scope` with an agent-specific one, and `exit_agent` restores the pre-agent scope by rebuilding from the (now-active) role/session/global lists.
- **Todo semantics from the user's perspective** — today's behavior is "todos are available when `auto_continue: true`". After Step 6.5, it's still "todos are available when `auto_continue: true`" — the only difference is we skip the wasted `TodoList::default()` allocation for the other agents.

**Risk:** Medium–high. This is where the Phase 1 refactor stops being mechanical and starts having semantic implications. Five things to watch:

1. **Parent scope restoration on `exit_agent`.** Today, `exit_agent` tears down the agent's MCP set but leaves the registry in whatever state `reinit` put it — the parent's original MCP set is NOT restored. Users don't notice because the next scope activation (or REPL exit) reinits anyway. In the new design, `exit_agent` MUST rebuild the parent's `ToolScope` from the still-active role/session/global lists so the user sees the expected state. Test this carefully.
2. **`McpFactory` contention.** With many concurrent sub-agents (say, 4 siblings each needing different MCP sets), the factory's mutex could become a bottleneck during `acquire()`. Hold the lock only while touching `active`, never while awaiting subprocess spawn.
3. **`RagCache` concurrent first-load.** If two consumers request the same `RagKey` simultaneously and neither finds a cached entry, both will try to `Rag::load` from disk. Use per-key `tokio::sync::Mutex` or `OnceCell` to serialize the first load — the second caller blocks briefly and receives the shared Arc. This applies equally to standalone and agent RAGs.
4. **Weak ref staleness in `RagCache`.** The `Weak<Rag>` in the map might point to a dropped `Rag`. The `load()` path MUST attempt `Weak::upgrade()` before returning; if upgrade fails, treat it as a miss and reload.
5. **`rebuild_rag` / `edit_rag_docs` race.** If a user runs `.rag rebuild` while another scope holds the same `Arc<Rag>` (concurrent API session, running sub-agent, etc.), the cache invalidation must NOT yank the Arc out from under the active holder. The `Arc` keeps its reference alive — invalidation just ensures *future* loads read fresh. This is the correct behavior for both standalone and agent RAG; worth confirming in tests.
6. **Identifying the right `RagKey` during rebuild.** `rebuild_rag` today operates on `Config.rag` without knowing its origin. In the new model, the code needs to check `ctx.agent_runtime` first to determine if the active RAG is agent-owned (`RagKey::Agent`) or standalone (`RagKey::Named`). Get this wrong and you invalidate the wrong cache entry, silently breaking subsequent loads.

### Step 7: Tackle mixed methods (THE HARD PART)

These 17 methods conditionally read global config OR per-request state depending on what's active. They need to be split into explicit parameter passing.

| Method | Why it's mixed | Refactoring approach |
|---|---|---|
| `current_model` | Returns agent model, session model, role model, or global model | Take `(&AppConfig, &RequestContext) -> &Model` — check ctx first, fall back to app |
| `extract_role` | Builds role from session/agent/role or global settings | Take `(&AppConfig, &RequestContext) -> Role` |
| `sysinfo` | Reads global settings + current rag/session/agent | Take `(&AppConfig, &RequestContext) -> String` |
| `set_temperature` | Sets on role-like or global | Split: `ctx.set_temperature()` for role-like, `app.set_temperature()` for global |
| `set_top_p` | Same pattern as temperature | Same split |
| `set_enabled_tools` | Same pattern | Same split |
| `set_enabled_mcp_servers` | Same pattern | Same split |
| `set_save_session` | Sets on session or global | Same split |
| `set_compression_threshold` | Sets on session or global | Same split |
| `set_rag_reranker_model` | Sets on rag or global | Same split |
| `set_rag_top_k` | Sets on rag or global | Same split |
| `set_max_output_tokens` | Sets on role-like model or global model | Same split |
| `set_model` | Sets on role-like or global | Same split |
| `retrieve_role` | Loads role, merges with current model settings | Take `(&AppConfig, &RequestContext, name) -> Role` |
| `use_role_safely` | Takes GlobalConfig, does take/replace pattern | Refactor to `(&mut RequestContext, name)` |
| `use_session_safely` | Takes GlobalConfig, does take/replace pattern | Refactor to `(&mut RequestContext, name)` |
| `save_message` | Reads `save` flag (global) + writes to messages_file (agent-dependent path) | Take `(&AppConfig, &RequestContext, input, output)` |
| `render_prompt_left/right` | Reads prompt format (global) + current model/session/agent/role (request) | Take `(&AppConfig, &RequestContext) -> String` |
| `generate_prompt_context` | Same as prompt rendering | Take `(&AppConfig, &RequestContext) -> HashMap` |
| `repl_complete` | Reads both global config and request state for completions | Take `(&AppConfig, &RequestContext, cmd, args) -> Vec<...>` |

**Common pattern for `set_*` methods:** The current code does something like:
```rust
fn set_temperature(&mut self, value: Option<f64>) {
    if let Some(role_like) = self.role_like_mut() {
        role_like.set_temperature(value);
    } else {
        self.temperature = value;
    }
}
```

This becomes:
```rust
// On RequestContext:
fn set_temperature(&mut self, value: Option<f64>, app_defaults: &AppConfig) {
    if let Some(role_like) = self.role_like_mut() {
        role_like.set_temperature(value);
    }
    // Global default mutation goes through a separate path if needed
}
```

### Step 8: The Caller Migration Epic (absorbed scope from Steps 6.5 and 7)

**Important:** the original plan described Step 8 as "rewrite main.rs and repl/mod.rs entry points." During implementation, Steps 6.5 and 7 deliberately deferred their semantic rewrites to Step 8 so the bridge pattern (add new methods alongside old, don't migrate callers yet) stayed consistent. As a result, Step 8 now absorbs:

- **Original Step 8 scope:** entry point rewrite (`main.rs`, `repl/mod.rs`)
- **From Step 6.5 deferrals:** `McpFactory::acquire()` implementation, scope transition rewrites (`use_role`, `use_session`, `use_agent`, `exit_agent`), RAG lifecycle via `RagCache` (`use_rag`, `edit_rag_docs`, `rebuild_rag`), session compression/autoname, `apply_prelude`, sub-agent spawning
- **From Step 7 deferrals:** `Model::retrieve_model` client module refactor, `retrieve_role`, `set_model`, `repl_complete`, `setup_model`, `update` dispatcher, `set_rag_reranker_model`, `set_rag_top_k`, `use_role_safely`/`use_session_safely` elimination, `use_prompt`, `edit_role`

This is a large amount of work. Step 8 is split into **8 sub-steps (8a–8h)** for clarity and reviewability. Each sub-step keeps the build green and can be completed as a standalone unit.

**Dependency graph between sub-steps:**

```
                                                    ┌─────────┐
                                                    │   8a    │  client module refactor
                                                    │ (Model: │  (Model::retrieve_model, list_models,
                                                    │  &App-  │   list_all_models! → &AppConfig)
                                                    │ Config) │
                                                    └────┬────┘
                                                         │ unblocks
                                                         ▼
                                                    ┌─────────┐
                                                    │   8b    │  remaining Step 7 deferrals
                                                    │ (Step 7 │  (retrieve_role, set_model, setup_model,
                                                    │  debt)  │   use_prompt, edit_role, update, etc.)
                                                    └────┬────┘
                                                         │
            ┌──────────┐                                 │
            │    8c    │                                 │
            │(McpFac-  │──┐                              │
            │ tory::   │  │ unblocks                     │
            │acquire())│  │                              │
            └──────────┘  │                              │
                          ▼                              │
                     ┌─────────┐                         │
                     │   8d    │                         │
                     │ (scope  │──┐                      │
                     │ trans.) │  │ unblocks             │
                     └─────────┘  │                      │
                                  ▼                      │
                              ┌─────────┐                │
                              │   8e    │                │
                              │ (RAG +  │──┐             │
                              │session) │  │             │
                              └─────────┘  │             │
                                           ▼             ▼
                                          ┌───────────────────┐
                                          │      8f + 8g      │
                                          │  caller migration │
                                          │  (main.rs, REPL)  │
                                          └─────────┬─────────┘
                                                    ▼
                                              ┌──────────┐
                                              │    8h    │  remaining callsites
                                              │ (sweep)  │  (priority-ordered)
                                              └──────────┘
```

---

#### Step 8a: Client module refactor — `Model::retrieve_model` takes `&AppConfig`

Target: remove the `&Config` dependency from the LLM client infrastructure so Step 8b's mixed-method migrations (`retrieve_role`, `set_model`, `repl_complete`, `setup_model`) can proceed.

**Files touched:**
- `src/client/model.rs` — `Model::retrieve_model(config: &Config, ...)` → `Model::retrieve_model(config: &AppConfig, ...)`
- `src/client/macros.rs` — `list_all_models!` macro takes `&AppConfig` instead of `&Config`
- `src/client/*.rs` — `list_models`, helper functions updated to take `&AppConfig`
- Any callsite in `src/config/`, `src/main.rs`, `src/repl/`, etc. that calls these client functions — updated to pass `&config.app_config_snapshot()` or equivalent during the bridge window

**Bridge strategy:** add a helper `Config::app_config_snapshot(&self) -> AppConfig` that clones the serialized fields into an `AppConfig`. Callsites that currently pass `&*config.read()` pass `&config.read().app_config_snapshot()` instead. This is slightly wasteful (clones ~40 fields per call) but keeps the bridge window working without a mass caller rewrite. Step 8f/8g will eliminate the clones when callers hold `Arc<AppState>` directly.

**Verification:** full build green. All existing tests pass. CLI/REPL manual smoke test: `loki --model openai:gpt-4o "hello"` still works.

**Risk:** Low. Mechanical refactor. The bridge helper absorbs the signature change cost.

---

#### Step 8b: Finish Step 7's deferred mixed-method migrations

With Step 8a done, the methods that transitively depended on `Model::retrieve_model(&Config)` can now migrate to `RequestContext` with `&AppConfig` parameters.

**Methods migrated to `RequestContext`:**
- `retrieve_role(&self, app: &AppConfig, name: &str) -> Result<Role>`
- `set_model_on_role_like(&mut self, app: &AppConfig, model_id: &str) -> Result<bool>` (paired with `AppConfig::set_model_default`)
- `repl_complete(&self, app: &AppConfig, cmd: &str, args: &[&str]) -> Vec<(String, Option<String>)>`
- `setup_model(&mut self, app: &AppConfig) -> Result<()>` — actually, `setup_model` writes to `self.model_id` (serialized) AND `self.model` (runtime). The split: `AppConfig::ensure_default_model_id()` picks the first available model and updates `self.model_id`; `RequestContext::reload_current_model(&AppConfig)` refreshes `ctx.model` from the app config's id.
- `use_prompt(&mut self, app: &AppConfig, prompt: &str) -> Result<()>` — trivial wrapper around `extract_role` (already done) + `use_role_obj` (Step 6)
- `edit_role(&mut self, app: &AppConfig, abort_signal: AbortSignal) -> Result<()>` — calls `app.editor()`, `upsert_role`, `use_role` (still deferred to 8d)

**RAG-related deferrals:**
- `set_rag_reranker_model` and `set_rag_top_k` get split: the runtime branch (update the active `Rag`) becomes a `RequestContext` method taking `Arc<Rag>` mutation, and the global branch becomes `AppConfig::set_rag_reranker_model_default` / `AppConfig::set_rag_top_k_default`.

**`update` dispatcher:** Once all the individual `set_*` methods exist on both types, `update` migrates to `RequestContext::update(&mut self, app: &mut AppConfig, data: &str) -> Result<()>`. The dispatcher's body becomes a match that calls the appropriate split pair for each key.

**`use_role_safely` / `use_session_safely`:** Still not eliminated in 8b — they're wrappers around the still-`Config`-based `use_role` and `use_session`. Eliminated in 8f/8g when callers switch to `&mut RequestContext`.

**Verification:** full build green. All tests pass. Smoke test: `.set temperature 0.7`, `.set enabled_tools fs`, `.model openai:gpt-4o` all work in REPL.

**Risk:** Low. Same bridge pattern, now unblocked by 8a.

---

#### Step 8c: Extract `McpFactory::acquire()` from `McpRegistry::init_server`

Target: give `McpFactory` a working `acquire()` method so Step 8d can build real `ToolScope` instances.

**Files touched:**
- `src/mcp/mod.rs` — extract the MCP subprocess spawn + rmcp handshake logic (currently inside `McpRegistry::init_server`, ~60 lines) into a standalone function:
  ```rust
  pub(crate) async fn spawn_mcp_server(
      spec: &McpServer,
      log_path: Option<&Path>,
      abort_signal: &AbortSignal,
  ) -> Result<ConnectedServer>
  ```
  `McpRegistry::init_server` then calls this helper and does its own bookkeeping. Backward-compatible for bridge callers.
- `src/config/mcp_factory.rs` — implement `McpFactory::acquire(spec: &McpServer, log_path, abort_signal) -> Result<Arc<ConnectedServer>>`:
  1. Build an `McpServerKey` from the spec
  2. Try `self.try_get_active(&key)` → share if upgraded
  3. Otherwise call `spawn_mcp_server(spec, ...).await` → wrap in `Arc` → `self.insert_active(key, &arc)` → return
- Write a couple of integration tests that exercise the factory's sharing behavior with a mock server spec (or document why a real integration test needs Phase 5's pooling work)

**What this step does NOT do:** no caller migration, no `ToolScope` construction, no changes to `McpRegistry::reinit`. Step 8d does those.

**Verification:** new unit tests pass. Existing tests pass. `McpRegistry` still works for all current callers.

**Risk:** Medium. The spawn logic is intricate (child process + stdio handshake + error recovery). Extracting without a behavior change requires careful diff review.

---

#### Step 8d: Scope transition rewrites — `use_role`, `use_session`, `use_agent`, `exit_agent`

Target: build real `ToolScope` instances via `McpFactory` when scopes change. This is where Step 6.5's scaffolding stops being scaffolding.

**New methods on `RequestContext`:**
- `use_role(&mut self, app: &AppConfig, name: &str, abort_signal: AbortSignal) -> Result<()>`:
  1. Call `self.retrieve_role(app, name)?` (from 8b)
  2. Resolve the role's `enabled_mcp_servers` list
  3. Build a fresh `ToolScope` by calling `app.mcp_factory.acquire(spec, ...)` for each required server
  4. Populate `ctx.tool_scope.functions` with the role's effective function list via `select_functions(app, &role)`
  5. Swap `ctx.tool_scope` atomically
  6. Call `self.use_role_obj(role)` (from Step 6)
- `use_session(&mut self, app: &AppConfig, session_name: Option<&str>, abort_signal) -> Result<()>` — same pattern, with session-specific handling for `agent_session_variables`
- `use_agent(&mut self, app: &AppConfig, agent_name: &str, session_name: Option<&str>, abort_signal) -> Result<()>` — builds an `AgentRuntime` (Step 6.5 scaffolding), populates `ctx.agent_runtime`, activates the optional inner session
- `exit_agent(&mut self, app: &AppConfig) -> Result<()>` — drops `ctx.agent_runtime`, rebuilds `ctx.tool_scope` from the now-topmost RoleLike (role/session/global), cancels the supervisor, clears RAG if it came from the agent

**Key invariant: parent scope restoration on `exit_agent`.** Today's `Config::exit_agent` leaves the `McpRegistry` in whatever state the agent left it. The new `exit_agent` explicitly rebuilds `ctx.tool_scope` from the current role/session/global enabled-server lists so the user sees the expected state after exiting an agent. This is a semantic improvement over today's behavior (which technically has a latent bug that nobody notices because the next scope activation fixes it).

**What this step does NOT do:** no caller migration. `Config::use_role`, `Config::use_session`, etc. are still on `Config` and still work for existing callers. The `_safely` wrappers are still around.

**Verification:** new `RequestContext::use_role` etc. have unit tests. Full build green. Existing tests pass. No runtime behavior change because nothing calls the new methods yet.

**Risk:** Medium–high. This is the first time `McpFactory::acquire()` is exercised outside unit tests. Specifically watch:
- **`McpFactory` mutex contention** — hold the `active` lock only during HashMap mutation, never across subprocess spawn or `await`
- **Parent scope restoration correctness** — write a targeted test that activates an agent with `[github]`, exits, activates a role with `[jira]`, and verifies the tool scope has only `jira` (not `github` leftover)

---

#### Step 8e: RAG lifecycle + session compression + `apply_prelude`

Target: migrate the Category C deferrals from Step 6 (session/RAG lifecycle methods that currently take `&GlobalConfig`).

**New methods on `RequestContext`:**
- `use_rag(&mut self, app: &AppConfig, name: Option<&str>, abort_signal) -> Result<()>` — routes through `app.rag_cache.load(RagKey::Named(name))`
- `edit_rag_docs(&mut self, app: &AppConfig, abort_signal) -> Result<()>` — determines the `RagKey` (Agent or Named) from `ctx.agent_runtime` / `ctx.rag` origin, calls `app.rag_cache.invalidate(&key)`, reloads
- `rebuild_rag(&mut self, app: &AppConfig, abort_signal) -> Result<()>` — same pattern as `edit_rag_docs`
- `compress_session(&mut self, app: &AppConfig) -> Result<()>` — reads `app.summarization_prompt`, `app.summary_context_prompt`, mutates `ctx.session`. Async, does an LLM call via an existing `Input::from_str` pattern.
- `maybe_compress_session(&mut self, app: &AppConfig) -> bool` — checks `ctx.session.needs_compression(app.compression_threshold)`, triggers compression if so. Returns whether compression was triggered; caller decides whether to spawn a background task (the task spawning moves to the caller's responsibility, not the method's).
- `autoname_session(&mut self, app: &AppConfig) -> Result<()>` — same pattern, uses `CREATE_TITLE_ROLE` and `Input::from_str`
- `maybe_autoname_session(&mut self, app: &AppConfig) -> bool` — same return-bool pattern
- `apply_prelude(&mut self, app: &AppConfig, abort_signal) -> Result<()>` — parses `app.repl_prelude` / `app.cmd_prelude`, calls the new `self.use_role()` / `self.use_session()` from 8d

**The `GlobalConfig`-taking static methods go away.** Today's code uses the pattern `Config::maybe_compress_session(config: GlobalConfig)` which takes an owned `Arc<RwLock<Config>>` and spawns a background task. After 8e, the new `RequestContext::maybe_compress_session` returns a bool; callers that want async compression spawn the task themselves with their `RequestContext` context. This is simpler and more explicit.

**Verification:** new methods have unit tests where feasible. Full build green. `compress_session` and `autoname_session` are tricky to unit-test because they do LLM calls; mock the LLM or skip the full path in tests.

**Risk:** Medium. The session compression flow is the most behavior-sensitive — getting the semantics wrong here results in lost session history. Write a targeted integration test that feeds 10+ user messages into a session, triggers compression, and verifies the session's summary is preserved.

---

#### Step 8f: Entry point rewrite — `main.rs`

Target: rewrite `main.rs` to construct `AppState` + `RequestContext` explicitly instead of using `GlobalConfig`.

**Specific changes:**
- `Config::init()` → `AppState::init()` which:
  1. Loads `config.yaml` into `AppConfig`
  2. Applies environment variable overrides (calls `AppConfig::load_envs` from Step 4)
  3. Calls `AppConfig::setup_document_loaders` / `AppConfig::setup_user_agent` (Step 4)
  4. Constructs the `Vault`, `McpFactory`, `RagCache`
  5. Returns `Arc<AppState>`
- `main::run()` constructs a `RequestContext` from the `AppState` and threads it through to subcommands
- `main::start_directive(ctx: &mut RequestContext, ...)` — signature change
- `main::create_input(ctx: &RequestContext, ...)` — signature change
- `main::shell_execute(ctx: &mut RequestContext, ...)` — signature change
- All 18 `main.rs` callsites updated

**`load_functions` and `load_mcp_servers`:** These are initialization-time methods that populate `ctx.tool_scope.functions` and `ctx.tool_scope.mcp_runtime`. They move from `Config` to a new `RequestContext::bootstrap_tools(&mut self, app: &AppConfig, abort_signal) -> Result<()>` that:
1. Initializes `Functions` via `Functions::init(visible_tools)` (existing code)
2. Resolves the initial enabled-MCP-server list from `app.enabled_mcp_servers`
3. Calls `app.mcp_factory.acquire()` for each
4. Assigns the result to `ctx.tool_scope`

This replaces the `Config::load_functions` + `Config::load_mcp_servers` call sequence in today's `main.rs`.

**Verification:** CLI smoke tests from the original plan's Step 8 verification checklist. Specifically:
- `loki "hello"` — plain prompt
- `loki --role explain "what is TCP"` — role activation
- `loki --session my-project "..."` — session
- `loki --agent sisyphus "..."` — agent activation
- `loki --info` — sysinfo output

Each should produce output matching the pre-Step-8 behavior exactly.

**Risk:** High. `main.rs` is the primary entry point; any regression here is user-visible. Write smoke tests that compare CLI output byte-for-byte with a recorded baseline.

---

#### Step 8g: REPL rewrite — `repl/mod.rs`

Target: rewrite `repl/mod.rs` to use `&mut RequestContext` instead of `GlobalConfig`.

**Specific changes:**
- `Repl` struct: `config: GlobalConfig` → `ctx: RequestContext` (long-lived, mutable across turns)
- `run_repl_command(ctx: &mut RequestContext, ...)` — signature change
- `ask(ctx: &mut RequestContext, ...)` — signature change
- Every dot-command handler updated. Dot-commands that take the `GlobalConfig` pattern (like the `_safely` wrappers) are **eliminated** — they just call `ctx.use_role(...)` directly.
- All 39 command handlers migrated
- All 12 `repl/mod.rs` internal callsites updated

**`use_role_safely` / `use_session_safely` elimination:** these wrappers exist only because `Config::use_role` is `&mut self` and the REPL holds `Arc<RwLock<Config>>`. After Step 8g, the REPL holds `RequestContext` directly (no lock), so the wrappers are no longer needed and get deleted.

**Verification:** REPL smoke tests matching the pre-Step-8 behavior. Specifically:
- Start REPL, issue a prompt → should see same output
- `.role explain`, `.session my-session`, `.agent sisyphus`, `.exit agent` — should all work identically
- `.set temperature 0.7` then `.info` — should show updated temperature
- Ctrl-C during an LLM call — should cleanly abort
- `.macro run-tests` — should execute without errors

**Risk:** High. Same reason as 8f — this is a user-visible entry point. Test every dot-command.

---

#### Step 8h: Remaining callsite sweep

Target: migrate the remaining modules in priority order (lowest callsite count first, keeping the build green after each module):

| Priority | Module | Callsites | Notes |
|---|---|---|---|
| 1 | `render/mod.rs` | 1 | `render_stream` just reads config — trivial |
| 2 | `repl/completer.rs` | 1 | Just reads for completions |
| 3 | `repl/prompt.rs` | 1 | Just reads for prompt rendering |
| 4 | `function/user_interaction.rs` | 1 | Just reads for user prompts |
| 5 | `function/mod.rs` | 2 | `eval_tool_calls` reads config |
| 6 | `config/macros.rs` | 3 | `macro_execute` reads and writes |
| 7 | `function/todo.rs` | 4 | Todo handlers read/write agent state |
| 8 | `config/input.rs` | 6 | Input creation — reads config |
| 9 | `rag/mod.rs` | 6 | RAG init/search |
| 10 | `function/supervisor.rs` | 8 | Sub-agent spawning — complex |
| 11 | `config/agent.rs` | 12 | Agent init — complex, many mixed concerns |

**Sub-agent spawning** (`function/supervisor.rs`) is the most complex item in the sweep. Each child agent gets a fresh `RequestContext` forked from the parent's `Arc<AppState>`:
- Own `ToolScope` built by calling `app.mcp_factory.acquire()` for the child's `mcp_servers` list
- Own `AgentRuntime` with fresh supervisor, fresh inbox, `current_depth = parent.depth + 1`
- `parent_supervisor = Some(parent.agent_runtime.supervisor.clone())` — weakly linked to parent for messaging
- `escalation_queue = parent.agent_runtime.escalation_queue.clone()` — `Arc`-shared from root
- RAG served via `app.rag_cache.load(RagKey::Agent(child_name))` — shared with any sibling of the same type

`config/agent.rs` — `Agent::init` is currently tightly coupled to `Config`. It needs to be rewritten to take `&AppState` + `&mut RequestContext`. Some of its complexity (MCP server startup, RAG loading) moves into `RequestContext::use_agent` from Step 8d; `Agent::init` becomes just the spec-loading portion.

**Verification:** after each module migrates, run full `cargo check` + `cargo test`. After all modules migrate, run the full smoke test suite from 8f and 8g.

**Risk:** Medium. The sub-agent spawning and `config/agent.rs` work is complex, but the bridge pattern means we can take each module independently.

---

### Step 8i: Migrate `Rag` module away from `GlobalConfig`

`src/rag/mod.rs` has 6 `GlobalConfig` references. `Rag::init`, `Rag::load`, `Rag::create`, and `Rag::refresh_document_paths` all take `&GlobalConfig` and read serialized fields from it.

1. Change `Rag::init`, `Rag::load`, `Rag::create` to take `&AppConfig` instead of `&GlobalConfig`. They read: `document_loaders`, `rag_embedding_model`, `rag_reranker_model`, `rag_top_k`. All of these are on `AppConfig`.
2. Change `Rag::refresh_document_paths` similarly.
3. Remove the `config: GlobalConfig` field from the `Rag` struct if it holds one. Replace with individual fields or pass `&AppConfig` to methods that need it.
4. Update all callers: `RequestContext::use_rag`, `RequestContext::edit_rag_docs`, `RequestContext::rebuild_rag` (currently bridge wrappers via `to_global_config()` — rewrite to pass `&AppConfig` directly). `Agent::init` Rag loading (still on GlobalConfig — leave for Step 8k).
5. Wire `RagCache::load` to use the migrated `Rag::load(&AppConfig, ...)`.
6. Run QA checklist items 13 (RAG).

**Blocked by:** Nothing. Leaf dependency.
**Unblocks:** Step 8k (Agent::init), Step 8l (supervisor).

---

### Step 8j: Migrate `Input` and chat completion chain away from `GlobalConfig`

The `Input` struct holds `config: GlobalConfig` as a field and reads from it in 10+ methods (`stream()`, `create_client()`, `prepare_completion_data()`, `build_messages()`, `echo_messages()`, `use_embeddings()`). The chat completion functions in `client/common.rs` get `GlobalConfig` from `client.global_config()`.

1. Change `Input` to hold `app: Arc<AppConfig>` + runtime fields extracted at construction time (model, role, session data, functions) instead of `GlobalConfig`. The key insight: `Input` is short-lived (one chat turn), so all config data can be captured at construction time.
2. Change `Input::from_str` and `Input::from_files` to take `&AppConfig` + `&RequestContext` (or just `&RequestContext` since it has `app: Arc<AppState>`).
3. Change `create_client()` to use the captured `AppConfig` fields instead of `self.config.read()`.
4. Change `prepare_completion_data()` — `select_functions` currently reads from `Config`. Move function selection to `Input` construction time (capture the effective function list).
5. Change `build_messages()` — reads session, role, agent from config. Capture at construction time.
6. Change `client/common.rs` — `call_chat_completions` and `call_chat_completions_streaming` no longer need `client.global_config()` for `eval_tool_calls`. Thread `&AppConfig` + `&mut RequestContext` or a callback.
7. Migrate `eval_tool_calls` in `function/mod.rs` to take `&AppConfig` + `&mut RequestContext` instead of `&GlobalConfig`. It reads: `tool_call_tracker`, `current_depth`, `root_escalation_queue` — these are on `RequestContext`. It also calls `call.eval(config)` for each tool — the `ToolCall::eval` method needs the same migration.
8. Remove `Input::from_str_ctx`, `Input::from_files_ctx`, `Input::from_files_with_spinner_ctx` bridge constructors (replaced by the proper constructors).
9. Update `main.rs` and `repl/mod.rs` to use the new `Input` constructors directly.
10. Run QA checklist items 2-6 (CLI), 8 (REPL chat), 12 (sub-agent escalation), 22 (error handling).

**Effort:** High. This is the largest single migration — `Input` touches everything.
**Blocked by:** Nothing (can proceed in parallel with 8i, but 8i is simpler).
**Unblocks:** Step 8k (Agent::init), Step 8l (supervisor), Step 9.

---

### Step 8k: Migrate `Agent::init` and agent lifecycle away from `GlobalConfig`

`Agent::init` in `config/agent.rs` takes `&GlobalConfig` and does ~200 lines of setup: loads agent config, compiles tools, starts MCP servers via `McpRegistry::reinit`, loads RAG, resolves model, initializes supervisor.

1. Change `Agent::init` to take `&AppState` + `&mut RequestContext` (or `&AppConfig` + `&mut RequestContext`).
2. Replace `McpRegistry::reinit` call with `McpFactory::acquire()` pattern from `rebuild_tool_scope` (Step 8d).
3. Replace `Rag::load(&GlobalConfig)` with `Rag::load(&AppConfig)` (from Step 8i).
4. Replace `Functions::init_agent` model resolution with `Model::retrieve_model(&AppConfig)` (already migrated in Step 8a).
5. Move `Config::use_agent` logic into `RequestContext::use_agent` — this was deferred from Step 8d because `Agent::init` wasn't migrated.
6. Remove the `to_global_config()` escape hatch from the `.agent` REPL handler and `main.rs` agent path.
7. Remove `sync_config_to_ctx` / `sync_ctx_to_config` calls from the `.agent` handler (no longer needed — everything operates on `RequestContext` directly).
8. Run QA checklist items 4 (CLI agent), 11 (REPL agents), 12 (sub-agent escalation).

**Effort:** High. `Agent::init` is deeply coupled.
**Blocked by:** Step 8i (Rag migration), Step 8j (Input migration — for sub-agent chat within Agent::init's RAG initialization prompt).
**Unblocks:** Step 8l (supervisor).

---

### Step 8l: Migrate `supervisor.rs` sub-agent spawning away from `GlobalConfig`

`function/supervisor.rs` has 17 `GlobalConfig` references. `handle_spawn_agent` creates a child `GlobalConfig` via `Config::default()` + field copying, then calls `Agent::init` on it.

1. Change `handle_spawn_agent` to create a child `RequestContext` instead of a child `GlobalConfig`. Use `AppState` (shared, immutable) + a fresh `RequestContext` for the child.
2. The child `RequestContext` gets: its own session, model, functions, agent, supervisor reference, incremented `current_depth`, its own `inbox`, the shared `root_escalation_queue`.
3. Replace `Agent::init(&child_global_config)` with the migrated `Agent::init(&AppState, &mut child_ctx)` from Step 8k.
4. Migrate `handle_collect_agent`, `handle_check_agent`, `handle_cancel_agent`, `handle_list_agents` — these read from child `GlobalConfig`; switch to reading from child `RequestContext`.
5. Migrate `handle_reply_escalation` — reads `root_escalation_queue` from `GlobalConfig`; switch to `RequestContext`.
6. Migrate teammate messaging (`handle_send_message`, `handle_check_inbox`).
7. Migrate `function/todo.rs` — reads/writes agent todo list from `GlobalConfig`; switch to `RequestContext`.
8. Migrate `function/user_interaction.rs` — reads `current_depth`, `root_escalation_queue` from `GlobalConfig`; switch to `RequestContext`.
9. Run QA checklist items 11 (REPL agents), 12 (sub-agent escalation — CRITICAL), 21 (auto-continuation).

**Effort:** Very high. Most complex migration — sub-agent lifecycle touches everything.
**Blocked by:** Step 8k (Agent::init migration).
**Unblocks:** Step 8m (REPL cleanup).

---

### Step 8m: REPL cleanup — eliminate `GlobalConfig` from REPL

After Steps 8i-8l, all internal modules operate on `RequestContext`. The REPL can be cleaned up:

1. Remove `sync_ctx_to_config` and `sync_config_to_ctx` helpers — no longer needed.
2. Remove the `config: GlobalConfig` field from `Repl` struct.
3. Rewrite `ReplCompleter` to take a shared reference to `RequestContext` state (e.g., `Arc<RwLock<RequestContext>>` or a snapshot struct).
4. Rewrite `ReplPrompt` similarly.
5. Rewrite `ReplHighlighter` similarly.
6. `run_repl_command` drops the `config: &GlobalConfig` parameter.
7. `ask` drops the `config: &GlobalConfig` parameter.
8. Remove `to_global_config()` calls from the REPL.
9. Remove `.exit role/session/agent` MCP reinit via `McpRegistry::reinit` — replace with `rebuild_tool_scope` pattern.
10. Replace `Config::update`, `Config::delete` with `RequestContext` equivalents (may need new methods).
11. Remove `macro_execute_ctx` bridge — rewrite `macro_execute` to take `&mut RequestContext`.
12. Run full QA checklist (all 22 sections).

**Effort:** High. Reedline component redesign is the trickiest part.
**Blocked by:** Steps 8i-8l (all internal modules migrated).
**Unblocks:** Step 9.

---

### Step 11: Migrate `eval_tool_calls` and `ToolCall::eval`

`eval_tool_calls` in `function/mod.rs` takes `&GlobalConfig` and reads
`tool_call_tracker`, `current_depth`, `agent`, `functions`. `ToolCall::eval`
dispatches to tool handlers passing `&GlobalConfig`.

1. Change `eval_tool_calls` to take `&GlobalConfig` → `&mut RequestContext`. It reads:
   - `tool_call_tracker` → on `RequestContext`
   - `current_depth` → on `RequestContext`
   - `root_escalation_queue` → on `RequestContext`
   - `agent` → on `RequestContext`
   - `functions` → on `RequestContext`
2. Change `ToolCall::eval` to take `&mut RequestContext` instead of `&GlobalConfig`
3. Change `call_chat_completions*` in `client/common.rs` from
   `runtime: &GlobalConfig` to `ctx: &mut RequestContext`
4. Update callers: `main.rs`, `repl/mod.rs`, `supervisor.rs`

**Blocked by:** Nothing (Steps 8i-10 provide all prerequisites).
**Unblocks:** Steps 12-13.

---

### Step 12: Migrate tool handlers

All tool handlers take `&GlobalConfig`. Change to `&mut RequestContext`:

1. `function/supervisor.rs` — `handle_supervisor_tool` and all 12 sub-handlers.
   Key changes:
   - `config.read().supervisor` → `ctx.supervisor`
   - `config.read().current_depth` → `ctx.current_depth`
   - `config.write().root_escalation_queue` → `ctx.root_escalation_queue`
   - `config.read().inbox` → `ctx.inbox`
   - `config.read().self_agent_id` → `ctx.self_agent_id`
   - `run_child_agent` takes `RequestContext` instead of `GlobalConfig` for child
     (child still needs `GlobalConfig` for `before_chat_completion`/`after_chat_completion`
     which are called on the child's `Config` — OR migrate those to
     `RequestContext::before_chat_completion`/`after_chat_completion` which already exist)

2. `function/todo.rs` — `handle_todo_tool` and sub-handlers.
   - `config.write().agent.as_mut()` → `ctx.agent.as_mut()`

3. `function/user_interaction.rs` — `handle_user_interaction_tool`.
   - `config.read().current_depth` → `ctx.current_depth`
   - `config.read().root_escalation_queue` → `ctx.root_escalation_queue`

4. `function/mod.rs` — MCP tool handlers (`invoke_mcp_tool`,
   `search_mcp_tools`, `describe_mcp_tool`).
   - `config.read().mcp_registry` → `ctx.tool_scope.mcp_runtime`
     (requires matching the `McpRegistry` API to `McpRuntime` API,
     or adding equivalent methods to `McpRuntime`)

**Effort:** High. `supervisor.rs` is the most complex (17 refs).
**Blocked by:** Step 11.
**Unblocks:** Step 13.

---

### Step 13: Migrate `run_child_agent` to `RequestContext`

`run_child_agent` in `supervisor.rs` runs the child agent's chat loop
using a `GlobalConfig`. After Steps 11-12, the loop body calls
`ctx.before_chat_completion`, `call_chat_completions(ctx)`,
`ctx.after_chat_completion` — all on `RequestContext`.

1. Change `run_child_agent` to take `RequestContext` instead of
   `GlobalConfig`
2. Build the child `RequestContext` in `handle_spawn` instead of
   the child `GlobalConfig`
3. The child `RequestContext` gets its own: session, model, functions,
   agent, supervisor, `current_depth`, `inbox`, `self_agent_id`,
   `root_escalation_queue` (shared with parent)
4. Remove child `GlobalConfig` construction entirely

**Blocked by:** Step 12.
**Unblocks:** Step 14.

---

### Step 14: Migrate `Input` constructors and REPL

After Steps 11-13, the tool chain runs on `RequestContext`. The REPL
no longer needs `GlobalConfig` for the tool chain.

1. Change `Input::from_str` and `Input::from_files` to take
   `&AppConfig` + `&RequestContext` instead of `&GlobalConfig`.
   `capture_input_config` reads `stream`, `session`, `rag`,
   `functions` — all on RequestContext/AppConfig already.
2. Change `ask` in `repl/mod.rs` to drop the `config: &GlobalConfig`
   parameter. Pass `&RequestContext` to Input construction.
3. Remove `sync_ctx_to_config` and `sync_config_to_ctx` helpers.
4. Remove `config: GlobalConfig` field from `Repl` struct.
5. Rewrite `ReplCompleter` to read from a shared snapshot of
   `RequestContext` state (e.g., `Arc<RwLock<RequestContext>>`
   wrapping the Repl's ctx, or a `ReplState` snapshot struct
   refreshed after each command).
6. Rewrite `ReplPrompt` and `ReplHighlighter` similarly.
7. Rewrite `macro_execute` to take `&mut RequestContext` instead
   of `&GlobalConfig`.
8. Remove `to_global_config()` from `RequestContext`.
9. Remove `reinit_mcp_registry` bridge helper.

**Blocked by:** Steps 11-13.
**Unblocks:** Step 15.

---

### Step 15: Delete `Config` struct and `GlobalConfig`

At this point no code references `GlobalConfig`.

1. Delete `src/config/bridge.rs`
2. Delete `Config::from_parts`, `to_app_config`, `to_request_context`
3. Delete all remaining methods on `Config` that were duplicated
4. Delete `#[serde(skip)]` runtime fields from `Config`
5. Rename `Config` to `RawConfig` if still needed for YAML
   deserialization, or delete entirely if `AppConfig` handles it
6. Delete `pub type GlobalConfig = Arc<RwLock<Config>>`
7. Remove `#[allow(dead_code)]` from `AppConfig`/`RequestContext`
   impl blocks
8. Move `RequestContext`'s flat runtime fields into `ToolScope` and
   `AgentRuntime` where appropriate:
   - `functions`, `tool_call_tracker` → `ToolScope`
   - `supervisor`, `parent_supervisor`, `self_agent_id`,
     `current_depth`, `inbox`, `root_escalation_queue` → `AgentRuntime`
9. Run `cargo check`, `cargo test`, `cargo clippy` — all clean
10. Run full QA checklist

**Blocked by:** Step 14.

---

### Step 16: Complete Config → AppConfig Migration (Post-QA)

**Status:** PENDING — to be completed after QA testing phase

The current bridge has a bug: `Config::init` mutates Config during startup (env vars, model resolution, etc.), but `to_app_config()` only copies serialized fields, losing those mutations.

Current startup flow (broken):
```
YAML → Config (serde deserialize)
    → config.load_envs()        ← mutates Config
    → config.setup_model()      ← resolves model
    → config.load_mcp_servers() ← starts MCP
    → cfg.to_app_config()       ← COPIES ONLY serialized fields!
    → AppConfig loses mutations
```

**Problem:** Mutations in Config are lost when building AppConfig.

**Solution:** Move mutations AFTER the bridge:

1. Move `load_envs()`, `set_wrap()`, `setup_model()`, `load_mcp_servers()`, `setup_document_loaders()`, `setup_user_agent()` from Config to AppConfig
2. In `main.rs`, apply these mutations AFTER `to_app_config()` 
3. Delete duplicated methods from AppConfig (they become reachable)
4. Simplify Config to pure serde deserialization only
5. Remove bridge if Config becomes just a deserialization target (or keep for backwards compat)

**Files to modify:**
- `src/config/mod.rs` — remove init mutations, keep only serde + deserialization
- `src/config/app_config.rs` — enable mutations, remove duplication
- `src/main.rs` — reorder bridge + mutations

**Goal:** Config becomes a simple POJO. All runtime configuration lives in AppConfig/AppState.

**Blocked by:** QA testing (Step 16 can begin after tests pass)

---

Phase 1 complete.

---

## Callsite Migration Summary

| Module | Functions to Migrate | Handled In |
|---|---|---|
| `config/mod.rs` | 120 methods (30 static, 10 global-read, 8 global-write, 35 request-read/write, 17 mixed) | Steps 2-7 (mechanical duplication), Step 10 (deletion) |
| `client/` macros and `model.rs` | `Model::retrieve_model`, `list_all_models!`, `list_models` | Step 8a |
| `main.rs` | `run`, `start_directive`, `shell_execute`, `create_input`, `apply_prelude_safely` | Step 8f |
| `repl/mod.rs` | `run_repl_command`, `ask`, plus 39 command handlers | Step 8g |
| `config/agent.rs` | `Agent::init`, agent lifecycle methods | Step 8h (partial) + Step 8d (scope transitions) |
| `function/supervisor.rs` | Sub-agent spawning, task management | Step 8h |
| `config/input.rs` | `Input::from_str`, `from_files`, `from_files_with_spinner` | Step 8h |
| `rag/mod.rs` | RAG init, load, search | Step 8e (lifecycle) + Step 8h (remaining) |
| `mcp/mod.rs` | `McpRegistry::init_server` spawn logic extraction | Step 8c |
| `function/mod.rs` | `eval_tool_calls` | Step 8h |
| `function/todo.rs` | Todo handlers | Step 8h |
| `function/user_interaction.rs` | User prompt handler | Step 8h |
| `render/mod.rs` | `render_stream` | Step 8h |
| `repl/completer.rs` | Completion logic | Step 8h |
| `repl/prompt.rs` | Prompt rendering | Step 8h |
| `config/macros.rs` | `macro_execute` | Step 8h |

### Step 8 effort estimates

| Sub-step | Effort | Risk |
|---|---|---|
| 8a — client module refactor | 0.5–1 day | Low |
| 8b — Step 7 deferrals | 0.5–1 day | Low |
| 8c — `McpFactory::acquire()` extraction | 1 day | Medium |
| 8d — scope transition rewrites | 1–2 days | Medium–high |
| 8e — RAG + session lifecycle migration | 1–2 days | Medium |
| 8f — `main.rs` rewrite | 1 day | High |
| 8g — `repl/mod.rs` rewrite | 1–2 days | High |
| 8h — remaining callsite sweep | 1–2 days | Medium |

**Total estimated Step 8 effort: ~7–12 days.** The "total Phase 1 effort" from the plan header needs to be updated once Step 8 finishes.

---

## Verification Checkpoints

After each step, verify:

1. **`cargo check`** — no compilation errors
2. **`cargo test`** — all existing tests pass
3. **Manual smoke test** — CLI one-shot prompt works, REPL starts and processes a prompt
4. **No behavior changes** — identical output for identical inputs

## Risk Factors

### Phase-wide risks

| Risk | Severity | Mitigation |
|---|---|---|
| Bridge-window duplication drift — bug fixed in `Config::X` but not `RequestContext::X` or vice versa | Medium | Keep the bridge window as short as possible. Step 8 should finish within 2 weeks of Step 7 ideally. Any bug fix during Steps 8a-8h must be applied to both places if the method is still duplicated. |
| Sub-agent spawning semantics change subtly | High | Cross-agent MCP trampling is a latent bug today that Step 8d/8h fixes. Write targeted integration tests for the sub-agent spawning path before and after Step 8h to verify semantics match (or improve intentionally). |
| Long-running Phase 1 blocking Phase 2+ work | Medium | Phase 2 (Engine + Emitter) can start prep work in parallel with Step 8h — the final callsite sweep doesn't block the new Engine design. |

### Step 8 sub-step risks

| Sub-step | Risk | Severity | Mitigation |
|---|---|---|---|
| 8a | Client macro refactor breaks LLM provider integration | Low | All LLM providers use the same `Model::retrieve_model` entry point. Test with at least 2 providers (openai + another) before declaring 8a done. |
| 8b | `update` dispatcher has ~15 cases — easy to miss one | Low | Enumerate every `.set` key handled today; check each is in the new dispatcher. |
| 8c | Extracting `spawn_mcp_server` introduces behavior differences (e.g., error handling, abort signal propagation) | Medium | Do a line-by-line diff review. Write a test that kills an in-flight spawn via the abort signal. |
| 8d | `McpFactory` mutex contention under parallel sub-agent spawning | Medium | Hold the `active` lock only during HashMap operations, never across `await`. Benchmark with 4 concurrent scope transitions before declaring 8d done. |
| 8d | Parent scope restoration on `exit_agent` differs from today's implicit behavior | High | Write a targeted test: activate global→role(jira)→agent(github,slack)→exit_agent. Verify scope is role(jira), not agent's (github,slack) or stale. |
| 8e | Session compression loses messages when triggered mid-request | High | Integration test: feed 10+ user messages, compress, verify summary preserves all user intent. Also test concurrent compression (REPL background task + foreground turn). |
| 8e | `rebuild_rag` / `edit_rag_docs` pick the wrong `RagKey` variant | Medium | Test both paths explicitly: agent-scoped rebuild and standalone rebuild. Assert the right cache entry is invalidated. |
| 8f | CLI output bytes differ from pre-refactor baseline | High | Record baseline CLI outputs for 10 common invocations. After 8f, diff byte-for-byte. Any difference is a regression unless explicitly justified. |
| 8g | REPL dot-command behavior regresses silently | High | Test every dot-command end-to-end: `.role`, `.session`, `.agent`, `.rag`, `.set`, `.info`, `.exit *`, `.compress session`, etc. |
| 8h | Sub-agent spawning in `function/supervisor.rs` shares state incorrectly between parent and child | High | Integration test: parent activates agent A (github), spawns child B (jira), verify B's tool scope has only jira and parent's has only github. Each parallel child has independent tool scopes. |
| 8h | `Agent::init` refactor drops initialization logic | Medium | `Agent::init` is ~100 lines today. Diff the old vs new init paths line-by-line. |

### Legacy risks (resolved during the refactor)

These risks from the original plan have been addressed by the step-by-step scaffolding approach:

| Original risk | How it was resolved |
|---|---|
| `use_role_safely` / `use_session_safely` use take/replace pattern | Eliminated entirely in Step 8g — REPL holds `&mut RequestContext` directly, no lock take/replace needed |
| `Agent::init` creates MCP servers, functions, RAG on Config | Resolved in Step 8d + Step 8h — MCP via `McpFactory::acquire()`, RAG via `RagCache`, functions via `RequestContext::bootstrap_tools` |
| Sub-agent spawning clones Config | Resolved in Step 8h — children get fresh `RequestContext` forked from `Arc<AppState>` |
| Input holds `GlobalConfig` clone | Resolved in Step 8f — `Input` now holds references to the context it needs from `RequestContext`, not an owned clone |
| Concurrent REPL operations spawn tasks with `GlobalConfig` clone | Resolved in Step 8e — task spawning moves to the caller's responsibility with explicit `RequestContext` context |

## What This Phase Does NOT Do

- No REST API server code
- No Engine::run() unification (that's Phase 2)
- No Emitter trait (that's Phase 2)
- No SessionStore abstraction (that's Phase 3)
- No UUID-based sessions (that's Phase 3)
- No agent isolation refactoring (that's Phase 5)
- No new dependencies added

The sole goal is: **split Config into immutable global + mutable per-request, with identical external behavior.**
