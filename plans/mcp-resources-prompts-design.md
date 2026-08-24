# Design: MCP Server Resources & Prompts Support

- **Status**: v1.3 — GATES PASSED 2026-08-24 (gatekeeper: SEALED after 5 friction fixes;
  Oracle: APPROVE-WITH-CHANGES, all 3 blockers B1-B3 + accepted suggestions folded in)
  (v1.1: full prefix-triple site sweep §4.6, centralized predicate helpers, invoke-sentinel fix R12;
  v1.2: live staged tab-completion for `.prompt` §5.4;
  v1.3: B1 injection-safe prompt submission, B2 runtime-sourced server_features, B3 spill-ext
  sanitization, OQ1/OQ2 resolved)
- **Date**: 2026-08-24
- **Author**: coyote design run (Oracle-verified against coyote @ working tree and rmcp 3.1.2 source)
- **Related memory**: `coyote-mcp-resources-prompts-design` (Oracle ruling), `coyote-escalation-notification-bug` (why we never synthesize assistant turns)

---

## 1. Problem statement

Coyote's MCP client is **tools-only**. Servers that expose resources (files, logs,
DB schemas, live documents) or prompts (server-owned, parameterized message
templates) have those capabilities silently ignored. Additionally, two latent
defects exist in the tools-only path today (§4.1, §8).

### 1.1 Current state (all cites verified 2026-08-24)

| Fact | Location |
|---|---|
| Unit ClientHandler: `ConnectedServer = RunningService<RoleClient, ()>` | `src/mcp/mod.rs:38` |
| `().serve(transport)` at all four connect sites | `src/mcp/mod.rs:600, 659, 676, 725` |
| Only `list_tools` + `call_tool` ever called | `src/mcp/mod.rs:350`, `src/config/tool_scope.rs:66, 113, 146` |
| **Pagination bug**: `list_tools(None)` = first page only, 3 sites | `src/mcp/mod.rs:350`, `src/config/tool_scope.rs:66` (catalog_items), `:113` (describe) |
| 3 meta-functions/server emitted **unconditionally** | `src/function/mod.rs:632-730` (`append_mcp_meta_functions`) |
| Meta-function call sites (3) | `src/config/app_state.rs:73`, `src/config/agent.rs:383-384` (delegates), `src/function/supervisor.rs:640` |
| Prefix constants | `src/mcp/mod.rs:34-36` (`mcp_invoke`, `mcp_search`, `mcp_describe`) |
| **TWO parallel prefix-dispatch chains**; unknown `mcp_*` names fall through to invoke | `src/function/mod.rs:1225-1249` (`eval_mcp`), `:1283+` (`eval`) |
| Concurrent-vs-sequential tool-call partition matches the 3 prefixes | `src/function/mod.rs:287-294` |
| Role tool selection EXCLUDES the 3 prefixes (3 hand-rolled triples) | `src/config/request_context.rs:2013-2017, 2027-2031, 2104-2108` (`select_enabled_functions`) |
| MCP-server selection INCLUDES/constructs the 3 prefixes (5 more triples) + **invoke-name sentinel** | `src/config/request_context.rs:2157-2161, 2170-2175, 2190-2195, 2196-2219, 2221-2225, 2253-2257` (`select_enabled_mcp_servers`) |
| `.list tools` filter is generic `starts_with("mcp_")` — auto-covers new prefixes, NO change | `src/config/request_context.rs:1243-1268` (`concrete_tool_names`) |
| Selection/display tests | `src/config/request_context.rs:5673-5730, 5892-5950` |
| Invoke result = raw serde passthrough (**unbounded base64 risk**) | `src/function/mod.rs:1444` (`invoke_mcp_tool`) |
| `CatalogItem { name, server, description }`, map keyed by bare name | `src/mcp/mod.rs:41-45`, `src/config/tool_scope.rs:75` |
| Tests hard-assert exactly 3 meta-functions/server | `src/function/mod.rs:2211-2295` |
| Cache dir helper | `src/config/paths.rs:37` |

### 1.2 Library facts (rmcp 3.1.2 — already the pinned version, no upgrade needed)

- `list_all_tools()`, `list_all_resources()`, `list_all_resource_templates()`,
  `list_all_prompts()` — cursor-following variants exist on the peer.
- `read_resource(ReadResourceRequestParam)` → `ReadResourceResult { contents: Vec<ResourceContents> }`;
  `ResourceContents` is **untagged** `Text { uri, mime_type, text } | Blob { uri, mime_type, blob }`
  (base64). Untagged deserialization is a defensive-parse risk (§8).
- `get_prompt(GetPromptRequestParam)` → `GetPromptResult { description, messages: Vec<PromptMessage> }`;
  `PromptMessage.role ∈ {User, Assistant}`; prompt **arguments are string-only per the MCP
  spec** — no type schemas exist and we must not invent them.
- `peer_info()` → `Option<InitializeResult>` whose `capabilities: ServerCapabilities` has
  `Option<ToolsCapability> / Option<ResourcesCapability> / Option<PromptsCapability>`.
  `None` peer_info can occur (e.g. handshake variance) — gating must fail open for tools (§4.4).
- **Phases 1, 2, and 2.5 require NO ClientHandler swap.** Everything that does is Phase 3.

## 2. Goals & non-goals

### Goals
1. LLMs can discover and read MCP resources (and expand resource templates) from
   any enabled server that advertises the `resources` capability.
2. Users (primary) and LLMs (secondary) can invoke MCP prompts from servers that
   advertise the `prompts` capability.
3. Resource/tool-result content is **bounded** before it enters model context —
   no unbounded base64, no multi-MB inline dumps.
4. Fix the `list_tools(None)` pagination bug in passing.
5. Meta-function emission becomes capability-gated instead of unconditional.

### Non-goals (explicitly out of scope for this run)
- ClientHandler swap and everything it enables: elicitation, server logging,
  roots, subscriptions, sampling, completion (§7, Phase 3 — deferred).
- Named-variable (`k=v`) support for **macros** — a good standalone enhancement,
  recorded as follow-up F1 (§11), not entangled here.
- Resource subscriptions / change notifications (needs a push channel; deferred).
- Any change to how MCP servers are configured, enabled, or authenticated.

## 3. Settled design decisions (with rationale)

These were adjudicated during design review and are **closed** — do not reopen
during implementation.

### D1 — Unified catalog, one lazy choke point
`CatalogItem` gains `kind` (tool | resource | resource_template | prompt), and
optional `uri`, `mime_type`, `size`. Catalog map keys become `{kind}:{id}` to
prevent collisions between a tool and a resource sharing a name. `catalog_items()`
(tool_scope.rs:61) remains the single live-listing choke point; it lists per kind
only when the server advertises that capability, and a failure in one kind
**warns and degrades** (other kinds still returned). Listings stay lazy — no
startup cost, no caching change.

### D2 — Meta-tool economy: exactly two new tools, capability-gated
One `mcp_read_<server>` (Phase 1) and one `mcp_prompt_<server>` (Phase 2), each
emitted **only when the server advertises the corresponding capability**.
Per-server tool count stays 3–5. Rejected alternatives: per-kind tool families
(context bloat), overloading `mcp_invoke` with resource reads (identity/shape
mismatch: invoke takes a tool name + schema'd args; read takes a URI + paging).

### D3 — Binary content is never inlined; text is paged
New `src/mcp/render.rs` is the single content policy for resource reads (Phase 1)
and tool results (Phase 2.5):
- **Text** → UTF-8-safe slices with `offset`/`max_bytes` paging (50 KB default,
  200 KiB clamp), returning `{ uri, mime_type, text, truncated, total_bytes, next_offset }`.
- **Blobs** → decoded and spilled to `cache_dir()/mcp-resources/<server>/<sha256>.<ext>`,
  never inlined. Rationale: base64 in-context is a context bomb (4/3× size) that
  the model cannot act on anyway; a path is actionable by the user,
  `execute_command`, fs tools, and sibling/parent agents.
- **Mislabeled-text sniff (settled amendment)**: before spilling, coyote attempts
  UTF-8 decode of the blob; if it decodes cleanly, it is **treated as text**
  (paged inline) regardless of the server's mime claim. Servers mislabel
  constantly; the model should never have to round-trip a spill for readable text.
- **Self-describing spill results (settled amendment)**: a spill returns a
  metadata object — `{ spilled: true, path, uri, mime_type (claimed), sniffed,
  size_bytes, sha256 }` — not a bare path, so contexts without fs tools still
  learn everything knowable about the content. `sniffed` is a boolean holding
  the UTF-8 sniff result — **always `false` on a spill** (a clean decode is
  inlined as text instead, never spilled); the field is present for shape
  stability so consumers need not branch on its absence.
- **No behavior branching on tool visibility (settled)**: `mcp_read` returns the
  same shape whether or not the calling context has fs tools enabled.
  Inline-if-no-fs-tools was considered and rejected: same call producing
  different shapes per context is a debugging trap and teaches the model the
  wrong contract. Accepted consequence: an fs-less context cannot post-process a
  spilled binary — but inline base64 would not have helped it either (§8, R7).

### D4 — `mcp_read` gets a `pattern` param (settled amendment)
Optional regex line-filter applied to **text** content after fetch, before
slicing — `fs_grep` semantics (matching lines + 2 lines of context, line numbers
prefixed). Rationale: `enabled_tools` and `enabled_mcp_servers` are independent
config keys, so contexts routinely have MCP servers without the fs suite; for a
2 MB log resource, "lines matching ERROR" is the difference between one call and
forty pages. Costs one optional param instead of replicating the fs toolset.
Discovery ("globbing") needs nothing new — that is what `mcp_search_<server>`
over the unified catalog already does.

### D5 — Prompts: `.prompt` is canonical; macro machinery AND bare-name dispatch both REJECTED
Adjudicated across two review rounds; full rationale preserved because it will
be asked again:

**Why not route prompts through macros** (even with named-variable support and
`isolated: false`, which does run steps in the user's live context):
1. **Content location**: a macro's `steps` are static YAML text interpolated
   client-side; an MCP prompt's content does not exist until invocation —
   `get_prompt(name, args)` is computed **server-side** (that is the point of
   server prompts: the server owns the template and can embed live data). A
   macro could only ever *call* the prompt primitive (`steps: [".prompt gh sum r={{r}}"]`),
   so the primitive must exist regardless and the macro layer is pure indirection.
2. **File-centric lifecycle**: `Macro::load` (src/config/macros.rs:141) reads
   `<name>.yaml` from disk; every `MacroState` (Missing/Invalid/Locked/…) is a
   statement about a file. Prompts are a live catalog that changes at connect
   time. Phantom `Macro` objects break the state machine; materialized files
   drift from the server.
3. **Double-gating**: prompts are already scoped by `enabled_mcp_servers`;
   adding `enabled_macros` on top creates incoherent states and namespace
   collisions with real user macros.
4. **Argument semantics**: macro variables resolve **positionally**
   (macros.rs:184-203) and error on missing values; MCP prompt args are named,
   string-only, and the design wants interactive prompting for missing required
   args.

**Why not bare-name top-level dispatch** (`.summarize` as a custom command,
inserted as a third lookup in the repl fallthrough chain at src/repl/mod.rs:1325-1344):
macro names are **user-chosen and disk-stable**; prompt names are
**server-chosen and change at connect time**. A server update can silently
shadow/get shadowed by a user macro or builtin; two servers exposing the same
prompt name force disambiguation syntax that reinvents `.prompt <server> <name>`
with worse ergonomics; the completer would need live connections.
**REJECTED PERMANENTLY** (user ruling, 2026-08-24): prompts will never be
dispatched as bare-name custom commands. `.prompt <server> <name>` is the only
prompt invocation surface for users, now and later — do not record, propose,
or implement bare-name dispatch as extensibility work.

### D6 — Prompt results are flattened into ONE user-role block
`GetPromptResult.messages` may contain assistant-role messages. We flatten the
entire list into a single user-role message with `[user]` / `[assistant]`
labels — the labels are emitted **unconditionally**, including for
single-message results (they are part of the contract, not formatting sugar;
see R14). **Never synthesize assistant turns in the transcript** — a synthetic
assistant message the model didn't produce is the exact failure mode from the
`__escalation_notification` incident (model imitates phantom transcript
entries). Both surfaces (REPL and meta-tool) use this flattening.

### D7 — Capability gating retrofit, fail-open for tools
`append_mcp_meta_functions` changes signature from `Vec<String>` (server names)
to `Vec<McpServerFeatures>` where
`McpServerFeatures { name, tools: bool, resources: bool, prompts: bool }`,
computed from `Arc<ConnectedServer>` handles (`peer_info()` lives on the
handle). Primary API: **`McpRuntime::server_features()`** — NOT the registry —
because delegate-agent servers are acquired via `McpFactory::acquire`
(mcp_factory.rs:93-118, Weak-cached, agent-spec-keyed) and populate a context's
`mcp_runtime` **without ever entering `McpRegistry`** (supervisor.rs:621-627).
A registry-sourced feature list would silently drop or mis-gate those servers'
meta-functions (R15). A thin `McpRegistry::server_features()` wrapper serves
registry-backed sites. Per-prefix emission:
- `mcp_search_` / `mcp_describe_`: always emitted (they operate on the unified
  catalog, which degrades per kind).
- `mcp_invoke_`: emitted iff tools capability **or `peer_info()` is `None`**
  (fail-open — a handshake hiccup must not silently strip a working server's tools).
- `mcp_read_`: emitted iff resources capability (fail-closed; a read against a
  non-resources server is a guaranteed error).
- `mcp_prompt_`: emitted iff prompts capability (fail-closed, same reason).

**Selection-sentinel interaction (critical)**: `select_enabled_mcp_servers`
(request_context.rs:2221) currently gates a server's enablement on its
**invoke** name being present in declarations, then inserts the whole trio.
Under this gating a resources-only server (no tools capability → no
`mcp_invoke_*` declaration) would fail that gate and lose ALL its
meta-functions, including `mcp_read_*`. The sentinel must change to the
**search** name (always emitted per D7) — see §4.6.

### D8 — Phase 3 (ClientHandler swap bundle) is deferred as one unit
The `ConnectedServer = RunningService<RoleClient, ()>` type alias change ripples
through registry/runtime/auth generics; Phases 1/2/2.5 need none of it. Bundling
elicitation/logging/roots/completion into one later swap avoids paying the
generics churn twice. Sampling is deferred **indefinitely** (server-initiated
LLM spend + prompt-injection surface with no consent UX).

## 4. Phase 1 — Resources

### 4.1 Pagination bug fix (in passing, first commit)
Replace `list_tools(None)` with `list_all_tools()` at all three sites:
`src/mcp/mod.rs:350` (start_server catalog build), `src/config/tool_scope.rs:66`
(catalog_items), `:113` (describe). Any paginating server silently loses tools
today. Note: GitHub-class servers make full lists large — this lands together
with the catalog work, not as a standalone perf regression.

### 4.2 Unified catalog
- `CatalogItem` (mcp/mod.rs:41) gains:
  `kind: CatalogKind` (`Tool | Resource | ResourceTemplate | Prompt`),
  `uri: Option<String>`, `mime_type: Option<String>`, `size: Option<u64>`.
- All catalog maps (mcp/mod.rs `ServerCatalog.items`, tool_scope.rs:67-76) key
  by `"{kind}:{id}"` where id = tool name / resource URI / template uriTemplate /
  prompt name.
- `catalog_items()` (tool_scope.rs:61) lists per kind, gated by the server's
  advertised capabilities; per-kind listing failure logs a warning and degrades
  (returns what succeeded). Uses `list_all_*` variants throughout.
- `mcp_search_<server>` searches the unified catalog; result items now carry
  `kind` so the model knows whether to follow up with describe/invoke or read.
- `mcp_describe_<server>` gains optional `kind` param (default `"tool"`,
  backward compatible): `kind:"resource"` returns the catalog metadata for a URI;
  `kind:"resource_template"` returns the template + its variables;
  `kind:"prompt"` returns name/description/arguments (Phase 2 fills this in).
  The existing `tool` param carries the identifier for **every** kind — tool
  name, resource URI, template uriTemplate, or prompt name — no new param is
  introduced.

### 4.3 `mcp_read_<server>` meta-tool
New prefix constant `MCP_READ_META_FUNCTION_NAME_PREFIX: &str = "mcp_read"`
(mcp/mod.rs:34-36 block). Prefix set audit: `mcp_invoke`, `mcp_search`,
`mcp_describe`, `mcp_read`, `mcp_prompt` — none is a prefix of another; the
`starts_with` dispatch stays sound.

Parameters:
```json
{
  "uri":       { "type": "string",  "required": true,
                 "description": "Resource URI, or a resource template with {var} placeholders" },
  "arguments": { "type": "object",
                 "description": "Template variable values (RFC 6570 Level 1 only)" },
  "pattern":   { "type": "string",
                 "description": "Optional regex; returns only matching lines (with context) from text content" },
  "offset":    { "type": "integer", "default": 0,
                 "description": "Byte offset for paging text. When pattern is set, offsets (and next_offset/total_bytes in the result) refer to the FILTERED stream, not the raw resource" },
  "max_bytes": { "type": "integer", "default": 51200, "description": "Max text bytes to return (clamped to 204800)" }
}
```

Behavior:
1. If `arguments` present, expand the URI template coyote-side — **RFC 6570
   Level 1 only** (simple `{var}` substitution, percent-encoded). Reject
   templates using operators beyond Level 1 with a teaching error.
2. `read_resource(uri)`; parse `ResourceContents` defensively (untagged enum:
   presence of `text` vs `blob` field decides; both/neither → structured error,
   never a panic).
3. Route contents through `render.rs` (§4.5): text → `pattern` filter (if any)
   → UTF-8-safe slice at `offset`/`max_bytes`; blob → sniff → inline-as-text or
   spill (D3). An invalid `pattern` regex → structured teaching error naming
   the parse failure (standard tool-error shape), never a silently ignored
   filter.
4. Multi-content results (a read may return several `ResourceContents`) render
   as an array of rendered items; paging params apply per text item, and the
   **whole response is additionally subject to an overall 204800-byte ceiling**
   — items beyond it are replaced with a truncation marker naming the count
   omitted (N × 200 KiB items must not stack into a context bomb).

**Dispatch wiring (critical)**: the new prefix must be added to **both** dispatch
chains — `eval_mcp` (function/mod.rs:1225-1249) and `eval` (function/mod.rs:1283+).
Invoke is the `else` **fallthrough** in both; a prefix added to only one chain
sends `mcp_read_*` calls into `invoke_mcp_tool` on the other path, producing a
confusing "tool not found on server" error instead of a read. New handlers
extract the server name with **`strip_prefix`, not `replace`** — the existing
handlers' `cmd_name.replace("{PREFIX}_", "")` pattern (function/mod.rs:1380,
1399, 1428) corrupts names containing the prefix mid-string; do not copy it.

### 4.4 Capability gating retrofit
Per D7. Touches:
- `src/mcp/mod.rs` / `src/mcp/tool_scope.rs`: new `McpServerFeatures` struct;
  **`McpRuntime::server_features()`** as the primary API (computed from the
  runtime's `Arc<ConnectedServer>` handles — covers factory-acquired
  delegate-agent servers that never enter the registry, see D7/R15) + a thin
  `McpRegistry::server_features()` wrapper for registry-backed sites.
- `src/function/mod.rs:632`: signature + per-feature emission.
- Call sites: `src/config/app_state.rs:73`, `src/config/agent.rs:383-384`,
  `src/function/supervisor.rs:640` — each switches from
  `list_started_servers()`-style name lists to `server_features()`. The
  supervisor site MUST source features from `ctx.tool_scope.mcp_runtime`
  (its servers come from `McpFactory::acquire`, supervisor.rs:621-627, and are
  absent from the registry); app_state.rs:73 uses the registry wrapper.
- Tests at `src/function/mod.rs:2211-2295` hard-assert exactly 3 meta-functions
  per server and must be rewritten around feature fixtures (tools-only server →
  3; tools+resources → 4; all → 5; `peer_info None` → invoke still present).
  The matrix MUST include a **delegate context with a factory-acquired,
  agent-only server** asserting correct gating — app_state-level fixtures
  cannot catch a registry-vs-runtime sourcing regression.

### 4.5 `src/mcp/render.rs` (new module)
Single content policy for `ResourceContents` (Phase 1) and `CallToolResult`
content (Phase 2.5):
- `render_text(text, mime, pattern, offset, max_bytes) -> RenderedText`
  — UTF-8-boundary-safe slicing (never split a codepoint; round `offset` forward
  and slice end backward to char boundaries); `pattern` filtering happens before
  slicing so paging walks the *filtered* stream.
- `render_blob(b64, claimed_mime, server) -> RenderedBlob`
  — decode (streaming, **50 MiB decoded ceiling** → error beyond), UTF-8 sniff
  (D3), spill to `cache_dir()/mcp-resources/<server>/<sha256>.<ext>` with `ext`
  derived from the claimed mime via a **fixed mime→ext allowlist** (the mime
  string is server-controlled — never derive `ext` by substring; any result not
  matching `[a-z0-9]{1,8}` falls back to `.bin`, closing the path-traversal
  surface, R3), write `0600`, return the self-describing metadata object.
- Size-limit constants (`50 MiB` decode, `204800` slice, `512 MiB` eviction)
  are **named `render.rs` constants, cited in the error/truncation messages**
  so limits are self-explaining; deliberately NOT config keys in v1 (OQ2
  ruling).
- Spill-dir hygiene: files are **untrusted input** — never auto-executed, never
  auto-opened; directory bounded (on write, if the **total across the whole
  `mcp-resources/` tree** — all `<server>` subdirs combined — exceeds 512 MiB,
  evict oldest-mtime files first); path is inside coyote's cache dir so `--info`
  discoverability and OS cache-cleaning conventions apply. Eviction is
  **best-effort** (concurrent coyote processes share the dir — ignore
  `NotFound` on unlink); a just-returned path may be evicted before use, which
  is acceptable: a same-sha re-read regenerates the identical path.

### 4.6 Prefix-predicate centralization — full sweep of triple sites

The three existing prefixes are hand-rolled as `starts_with` triples at
**twelve** sites. Adding `mcp_read`/`mcp_prompt` as a fourth and fifth
condition at each site is exactly the bug pattern that produced R4 — so this
design **centralizes the predicate** instead. New helpers in `src/mcp/mod.rs`:

```rust
pub const MCP_META_FUNCTION_PREFIXES: [&str; 5] =
    [MCP_INVOKE_.., MCP_SEARCH_.., MCP_DESCRIBE_.., MCP_READ_.., MCP_PROMPT_..];
pub fn is_mcp_meta_function(name: &str) -> bool;          // any-prefix predicate
pub fn mcp_meta_function_names(server: &str) -> Vec<String>; // all 5 candidate names for a server
```

Every site below switches to the helpers (behavior per-site noted). Implementers
MUST hit all of them; a missed site fails silently, not loudly:

| Site | Today | Change |
|---|---|---|
| `function/mod.rs:287-294` — partition into concurrent `eval_mcp` vs sequential `eval` | 3-prefix `starts_with` OR-chain | `is_mcp_meta_function`. Miss ⇒ `mcp_read_*` routes to `eval()`, misses its guards too, treated as external argc tool → hard failure |
| `function/mod.rs:1225-1249` (`eval_mcp`) + `:1283+` (`eval`) | per-prefix dispatch arms, invoke = else-fallthrough | add `read`/`prompt` arms to BOTH chains (R4) |
| `request_context.rs:2013-2017, 2027-2031, 2104-2108` (`select_enabled_functions`) | 3 exclusion triples keeping meta-functions out of the `enabled_tools` pool | `!is_mcp_meta_function`. Miss ⇒ new functions leak into the tools pool and get wrongly stripped by role tool filters |
| `request_context.rs:2157-2161, 2170-2175, 2253-2257` (`select_enabled_mcp_servers` inclusion filters) | 3 inclusion triples | `is_mcp_meta_function`. Miss ⇒ new functions **silently dropped from every request** where a role/agent/session sets `enabled_mcp_servers` |
| `request_context.rs:2190-2195` + mapping expansion `:2196-2219` | constructs the 3 names per server | `mcp_meta_function_names(server)`; candidates absent from declarations are already filtered/no-ops downstream (`:2219`, `:2232-2244`), so gated-off names are harmless |
| `request_context.rs:2221-2225` | **sentinel**: server enabled iff its `mcp_invoke_*` name exists in declarations | sentinel switches to the `mcp_search_*` name (always emitted per D7) — fixes the D7 interaction where a resources-only server loses everything |
| `request_context.rs:1243-1268` (`concrete_tool_names`, feeds `.list tools`) | generic `starts_with("mcp_")` | **NO change** — auto-covers new prefixes; regression test pins this |
| `.list mcp-servers` (rc.rs:2765+), `tools_info` (rc.rs:660) | server-level / selection-derived | **NO change** — correct once selection is |
| Tests: `function/mod.rs:2128-2130, 2211-2295`; `mcp/mod.rs:1185-1187`; `request_context.rs:5673-5730, 5892-5950` | assert 3 prefixes / 3-per-server sets | rewrite around feature fixtures (§4.4) + new-prefix selection cases |

## 5. Phase 2 — Prompts

### 5.1 Primary surface: REPL
- `.prompt <server> <name> [key=value ...]` — named args only (prompt args are
  named per spec; there is no positional order to rely on). Values may be quoted.
  Missing **required** args (per the prompt's declared arguments) → interactive
  `inquire` prompt for each, mirroring existing REPL interaction patterns.
- Result submitted **as user input**, flattened per D6 — but **NEVER through
  `run_repl_command`** (R14): prompt content is server-controlled, and
  `run_repl_command`'s non-command branch runs `try_extract_shell_command`
  first (repl/mod.rs:1353-1354 — a leading `!` executes a shell command) while
  unknown `.`-words fall through into `macro_execute` (:1331-1350). Flattened
  text starting with `!` or `.` would be *executed*, not chatted. Submit the
  flattened text directly via the `Input::from_str` + `ask()` path
  (repl/mod.rs:1356-1358), bypassing line parsing entirely.
- `.list prompts` — table of `server / name / description / args` across enabled
  servers (live listing via the unified catalog; degrades per server).
- Completion: live, staged tab-completion for servers → prompts → `key=`
  argument keys — full spec in §5.4.
- Dispatch-order check: `.prompt` is a new builtin arm and therefore shadows any
  user macro named `prompt` (repl fallthrough order: builtins before macros).
  Ship a startup/`.macro list` warning if such a macro exists; document in wiki.

### 5.2 Secondary surface: `mcp_prompt_<server>` meta-tool
New prefix constant `MCP_PROMPT_META_FUNCTION_NAME_PREFIX: &str = "mcp_prompt"`.
Emitted iff prompts capability (D7). Params:
```json
{
  "prompt":    { "type": "string", "required": true },
  "arguments": { "type": "object", "description": "String values only; prompt arguments have no schemas" }
}
```
Returns the flattened one-user-block text (D6) as the tool result — the model
folds it into its own reasoning; we do not inject transcript messages from a
tool result. Missing required args → structured teaching error listing them
(no interactivity on the LLM path). Same dual-dispatch-chain wiring warning as
§4.3.

### 5.3 Catalog/describe integration
Prompts appear in the unified catalog as `kind: prompt` (searchable via
`mcp_search_`); `mcp_describe_<server> {kind:"prompt", tool:"<name>"}` returns
name/description/arguments (names, descriptions, required flags — strings only,
never invented schemas).

### 5.4 Live staged tab-completion for `.prompt`

Discovery is the whole battle for prompts; completion queries the **running**
MCP servers live, per keystroke stage. Wiring: `.prompt` arms in
`repl_complete` (request_context.rs:3267), which already dispatches per command
and arg position; the reedline completer (src/repl/completer.rs:57) delegates
there and fuzzy-filters on the last arg.

**The three stages:**

| Input | Suggestions | Data source | RPC? |
|---|---|---|---|
| `.prompt <TAB>` | server names — only servers that are (a) enabled in the current context, (b) already running, and (c) advertise the prompts capability | `peer_info()` on running servers — local state | **NO** (per ruling: do not list prompts at this stage) |
| `.prompt <server> <TAB>` | prompt names for that server, with descriptions | `list_all_prompts(server)`, queried **live on each TAB** | YES |
| `.prompt <server> <name> <TAB>` | `key=` for each of that prompt's arguments — description shown, required args marked `(required)`; keys already present in the typed args are excluded | same `list_all_prompts` result, matched by name | YES |

**Sync→async bridge**: reedline's `Completer::complete` is synchronous; the
MCP peer calls are async. Use the established in-repo pattern —
`Handle::current()` + `tokio::task::block_in_place(|| h.block_on(...))` — with
precedent at src/vault/mod.rs:162-234 (every vault op) and
src/cli/completer.rs:55-59 (a completer doing exactly this, including the
no-runtime fallback). `block_in_place` requires the multi-thread runtime; the
cli completer's `Handle::try_current()` fallback pattern is the template.
Verified: `read_line` (repl/mod.rs:427) runs inside the async `run` future on
`#[tokio::main]`'s main-thread `block_on`, where `block_in_place` is allowed —
the vault ops exercise exactly this context in production today. Leave a
one-line comment at the bridge noting this **main-thread-block_on dependency**:
if the REPL loop ever moves into `spawn_blocking`, the bridge semantics change.

**Guardrails:**
- Completion NEVER starts or connects a server — only already-running servers
  are consulted (stage 1's capability check is pure local state).
- Stage 1's "enabled in the current context" check reuses the
  `mapping_mcp_servers` expansion from `select_enabled_mcp_servers` — factor a
  small **shared helper** so the completer and request selection cannot drift.
  Features come from the REPL ctx's `McpRuntime::server_features()` (D7), not
  the registry.
- **Never hold the `ctx.read()` guard across the RPC**: completer.rs:32 takes
  the read lock for the whole `repl_complete` call; the `.prompt` arms must
  clone the needed `Arc<ConnectedServer>` handles + metadata and **drop the
  guard before blocking** — parking_lot's writer priority would otherwise stall
  writers AND subsequent readers for up to the full 2s timeout.
- Every completion RPC is bounded by a short timeout (default 2s,
  `tokio::time::timeout`); on timeout or error, return **empty suggestions
  silently** — a keystroke must never surface an error or hang the line editor.
- **Error-handling matrix (all cases = silent empty suggestions, never an
  error):**
  - Enabled but unauthenticated/failed server: never enters
    `registry.running_servers()` (start_server fails with `McpAuthRequired`,
    mcp/mod.rs:337-340, before insertion) → absent from stage 1, `runtime.get()
    == None` for stages 2/3. Structurally cannot error.
  - Non-running or misspelled server name typed manually → `None` lookup →
    empty.
  - Running server whose token expired mid-session → `list_all_prompts` fails
    with the auth-required error (auth_client.rs:51-55) → swallowed to empty.
    The completer MUST NOT initiate re-auth — a TAB keystroke never launches an
    OAuth flow. Auth recovery belongs to the invocation path: `.prompt <server>
    <name>` surfaces the normal auth-required error, same as `mcp_invoke`.
  - Prompt name not found at stage 3 (deleted server-side between TABs) →
    empty.
- Queried live on every TAB, no caching (user ruling: freshness over latency;
  a stale prompt list is worse than a 100 ms pause). If real-world latency
  proves painful, a micro-TTL cache is follow-up F4 — not v1.
- Argument-key suggestions emit `key=` with `append_whitespace: false`
  (create_suggestion already does this) so the cursor lands ready for the value.

## 6. Phase 2.5 — Bound today's tool-result passthrough

`invoke_mcp_tool` (function/mod.rs:1444) currently returns
`serde_json::to_value(CallToolResult)` raw — a tool result embedding an image or
blob ships **unbounded base64 into model context today**. Route
`CallToolResult.content` items through `render.rs`: text content unchanged
unless oversized — **oversized = exceeds 204800 bytes (the render.rs 200 KiB
clamp)**, then sliced to 204800 bytes with a `truncated` marker + note to
re-call with narrower args — image/blob content spilled per D3.
`structured_content` passes through as-is (it is JSON, servers use it
deliberately) but its serialized form is subject to the **same 204800-byte
ceiling** with a truncation marker. This is deliberately sequenced
*after* Phase 1 so render.rs exists and is battle-tested on resources first.

## 7. Phase 3 — Deferred: the ClientHandler swap bundle

Recorded so the deferral is a decision, not an omission. One future run replaces
`()` with a real handler (single generics churn through
registry/runtime/auth):
- **Elicitation → the `user__*` escalation bridge** (highest value: servers can
  ask the user questions mid-call, mapped to coyote's existing escalation queue).
- Server logging → coyote log file. Roots → workspace dir (cheap).
- Completion → `.prompt` tab-completion of argument values.
- Subscriptions → deferred until a push channel exists.
- **Sampling → deferred indefinitely** (server-initiated LLM spend +
  prompt-injection surface, no consent UX).

## 8. Risks & mitigations

| # | Risk | Mitigation |
|---|---|---|
| R1 | Untagged `ResourceContents` mis-parses exotic server payloads | Defensive field-presence parse; structured error, never panic (§4.3) |
| R2 | UTF-8 boundary splits in paging corrupt text | Boundary-rounding slice logic + dedicated tests incl. multibyte fixtures (§4.5) |
| R3 | Spill dir grows unbounded / hosts untrusted files | 512 MiB eviction bound, 0600, never auto-executed, cache-dir location (§4.5) |
| R14 | **Server-controlled prompt content executed as a REPL command/shell line** — flattened GetPromptResult text starting with `!` or `.` routed through `run_repl_command` would be executed, not chatted | `.prompt` submits via `Input::from_str` + `ask()` directly (repl/mod.rs:1356-1358), never through line parsing (§5.1); D6 labels emitted unconditionally; test: prompt result beginning with `!rm`/`.session` is chatted verbatim |
| R15 | Registry-sourced `server_features()` silently drops factory-acquired delegate-agent servers (never in `McpRegistry`) | Primary API is `McpRuntime::server_features()` computed from `Arc<ConnectedServer>` handles; supervisor site sources from `ctx.tool_scope.mcp_runtime` (§4.4, D7); delegate-context fixture test |
| R4 | New prefixes wired into only one dispatch chain → silent fallthrough to invoke | Explicit wiring rule §4.3/§5.2; test asserting `mcp_read_x`/`mcp_prompt_x` never reach `invoke_mcp_tool` |
| R5 | `.prompt` shadows a user macro named `prompt` | Warning + docs (§5.1) |
| R6 | `peer_info() == None` strips a working server's tools | Fail-open for invoke only (D7) |
| R7 | fs-less contexts can't post-process spilled binaries | Accepted: inline base64 wouldn't help them either; self-describing spill metadata + `pattern`/paging cover text, which is the actionable case (D3/D4) |
| R8 | `list_all_*` on huge servers (GitHub-class) slows lazy listings | Listings remain lazy/per-call; only correctness change vs today; if latency bites, caching is a follow-up, not a v1 feature |
| R9 | `audience: ["user"]` annotated resources arguably don't belong in model context | Pass through + surface the annotation in rendered read metadata AND `mcp_search` results (OQ1 ruling, §12); revisit on field evidence of misuse |
| R10 | Background-jobs design (plans/background-jobs-design.md:549-550) classifies backgroundability by `mcp_*` prefix lists | New prefixes classified **not backgroundable** in v1 (single bounded RPC); the bg-jobs prefix tables must be updated when both land — follow-up F2 |
| R11 | A missed prefix-triple site silently drops or misroutes the new meta-functions (12 sites today) | Centralized `is_mcp_meta_function` / `mcp_meta_function_names` helpers replace ALL hand-rolled triples (§4.6); grep-audit acceptance criterion: no `starts_with(MCP_..._PREFIX)` triple remains outside mcp/mod.rs and the two dispatch chains |
| R12 | Invoke-name sentinel drops resources-only servers entirely under D7 gating | Sentinel moves to search name (§4.4, §4.6) + dedicated test: resources-only fixture keeps search/describe/read through `select_enabled_mcp_servers` |
| R13 | `.prompt` completion RPC hangs/blocks the line editor on a slow or wedged server | 2s `tokio::time::timeout` per completion RPC, silent empty-suggestion degrade, only already-running servers queried (§5.4); `block_in_place` needs the multi-thread runtime — use the cli/completer.rs:55-59 `Handle::try_current()` fallback template |

## 9. Testing strategy

- **render.rs**: unit tests for boundary-safe slicing (ASCII, multibyte, offset
  past EOF), pattern filtering, sniff (valid UTF-8 blob → text; binary → spill),
  decode ceiling, spill naming/dedup (same sha → same path), eviction
  (best-effort, NotFound-tolerant), **ext sanitization** (crafted mimes with
  `/`, `..`, unicode → `.bin`; allowlisted mimes → expected ext) (B3),
  multi-item overall response ceiling.
- **Gating**: fixture servers advertising each capability combination; assert
  exact meta-function sets incl. the `peer_info None` fail-open case (rewrites
  function/mod.rs:2211-2295), **plus a delegate context with a
  factory-acquired agent-only server** (registry-vs-runtime sourcing, R15).
- **Dispatch**: `mcp_read_*`/`mcp_prompt_*` route correctly on BOTH chains; a
  bogus `mcp_bogus_x` still falls through to invoke (current behavior preserved).
- **Partition**: `mcp_read_*`/`mcp_prompt_*` calls take the concurrent
  `eval_mcp` path (function/mod.rs:287), never the sequential external-tool path.
- **Selection** (request_context.rs): role with `enabled_mcp_servers: [srv]`
  keeps all emitted meta-functions incl. read/prompt; `enabled_tools` filters
  never strip them; resources-only server survives the sentinel (R12);
  mapping_mcp_servers expansion covers all 5 names; `.list tools` continues to
  exclude all `mcp_*` names (regression pin on `concrete_tool_names`).
- **Catalog**: `{kind}:{id}` collision test (tool and resource named alike);
  per-kind degradation (resources listing errors → tools still returned).
- **REPL**: `.prompt` arg parsing (named, quoted, missing-required → inquire),
  `.list prompts`, macro-shadow warning, **submission-path safety: a prompt
  result whose flattened text begins with `!` or `.` is submitted as chat
  input, never executed** (R14). Existing test conventions apply
  (pid+counter temp dirs, `#[serial]` for env-touching tests).
- **Completion** (§5.4): stage-1 filters to running+prompts-capability servers
  without any RPC; stage-2/3 suggestions from a fixture server (names +
  descriptions, `key=` args, required markers, already-typed keys excluded);
  timeout/error → empty suggestions (no panic, no error text); no-runtime
  fallback path exercised; unauthenticated/non-running server absent from
  stage 1 and yields empty (not error) at stages 2/3; auth-expired RPC error
  swallowed without triggering re-auth.
- **Template expansion**: Level 1 substitution + percent-encoding; rejection of
  Level 2+ operators.

## 10. Task breakdown sketch (for materialization after gates)

1. **T1**: `list_all_tools` pagination fix (3 sites) + prefix-constant module
   prep, incl. the §4.6 helpers (`is_mcp_meta_function`,
   `mcp_meta_function_names`) and the mechanical replacement of ALL existing
   hand-rolled triples (partition + both dispatch chains + the 8
   request_context.rs sites) — behavior-neutral at this point, so it lands
   before any new prefix exists.
2. **T2**: Unified catalog (`CatalogItem` kind/uri/mime/size, keyed maps,
   per-kind lazy listing, search/describe integration). Registry-side
   `ServerCatalog` (mcp/mod.rs:163) is write-only today — treat
   `catalog_items()` as the only live consumer and simplify accordingly.
3. **T3**: `render.rs` (text paging, pattern filter, sniff, spill, hygiene) — pure
   module + tests, no wiring.
4. **T4**: `mcp_read_<server>` (declaration, both dispatch chains, template
   expansion) wired to render.rs.
5. **T5**: Capability gating retrofit (`server_features()`, signature change,
   3 call sites, test rewrite), incl. the invoke→search sentinel fix in
   `select_enabled_mcp_servers` (§4.6, R12). Depends on T2.
6. **T6**: Prompts — `.prompt`, `.list prompts`, completer, shadow warning.
   Includes the full §5.4 staged live completion (repl_complete arms, async
   bridge, timeout guardrails) and the `REPL_COMMANDS` registrations for
   `.prompt` / `.list prompts` (name, description, `is_valid(state)`) — the
   stage-0 command completion and `.help` derive from that table. Depends on T2.
7. **T7**: `mcp_prompt_<server>` meta-tool. Depends on T5, T6 (flattening shared).
8. **T8**: Phase 2.5 — route `CallToolResult.content` through render.rs. Depends on T3.
9. **T9**: Docs — wiki + README + config examples; CHANGELOG is cz-generated
   (never hand-edit). The GitHub wiki (`Dark-Alex-17/coyote.wiki`) MUST be
   updated to document ALL the enhanced functionality, not just mention it:
   - **MCP page — resources**: the `mcp_read_<server>` meta-tool (uri,
     `arguments` template expansion, `pattern` line-filtering, `offset`/
     `max_bytes` paging); blob handling — UTF-8 sniff, spill location
     (`cache_dir()/mcp-resources/`), the self-describing spill metadata object,
     size ceilings and eviction; catalog/search/describe now spanning tools +
     resources + prompts.
   - **MCP page — capability gating**: which meta-functions appear per server
     capability set (and why a resources-only server still shows
     search/describe/read).
   - **REPL/commands page — `.prompt`**: full usage (`.prompt <server> <name>
     [key=value ...]`), quoting, interactive inquire for missing required args,
     result-as-user-input semantics, `.list prompts`, and the macro-shadow
     warning (a user macro named `prompt` is shadowed by the builtin).
   - **REPL/commands page — tab completion**: the §5.4 staged behavior
     (servers → prompts → `key=`), that it queries live per TAB, and the
     silent-empty semantics — explicitly document that an enabled-but-
     unauthenticated server shows nothing at `<TAB>` and that auth recovery
     happens on invocation (the `.prompt` call surfaces the auth-required
     error), so users aren't confused by an "empty" completion list.
   - **Config page**: any new/changed config examples (enabled_mcp_servers
     interaction with the new meta-functions).
   Acceptance criterion: every user-visible surface added by T1–T8 has a wiki
   section; PR description links the updated wiki pages.

Sequencing: T1 → T2 → {T3, T5} → T4 → {T6 → T7, T8} → T9.

## 11. Follow-ups (recorded, NOT in this run)

- **F1**: Macro named-variable support (`k=v` invocation with positional
  fallback) — standalone macro-system enhancement, adjudicated as valuable but
  orthogonal.
- **F2**: Update background-jobs prefix classification tables when both designs
  are merged (R10).
- **F3**: Catalog caching if `list_all_*` latency on large servers proves
  painful (R8).
- **F4**: Micro-TTL cache for `.prompt` completion RPCs if live-per-TAB latency
  proves painful in practice (§5.4 keeps v1 cache-free by design).

## 12. Open questions — RESOLVED at gate review (Oracle, 2026-08-24)

- **OQ1 — RESOLVED: pass + surface.** `audience` is advisory metadata in the
  MCP spec, not access control; a server hiding secrets behind
  `audience:["user"]` is misusing it, and refusing reads would create a
  confusing search-shows-it/read-refuses-it gap with no user recourse. Surface
  the annotation in **both** the rendered read metadata AND `mcp_search`
  results so the model can self-select. Revisit only on field evidence of
  misuse.
- **OQ2 — RESOLVED: keep 50 MiB decode / 512 MiB eviction as hardcoded, named
  `render.rs` constants; NO config keys in v1.** Both are generous for real
  use cases (logs, schemas, documents); config surface has permanent
  maintenance cost; constants→config is a trivial later change. Cite the
  constants in error messages so limits are self-explaining (§4.5).
