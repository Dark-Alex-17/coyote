# Design: Background Tool Jobs & Completion Push Notifications

Status: DRAFT v1.6 — grounded in src/function/mod.rs (eval_tool_calls, escalation
injection), src/supervisor/mod.rs (Supervisor/AgentHandle), src/function/
supervisor.rs (agent__* handlers, turn-end guardrail), and Oracle rulings
(agent_oracle_10936145, persisted as memory `coyote-bg-jobs-notifications-rulings`).
User rulings R6/R7 added 2026-08-21; R7 amended + R8/R9 and §9 (regression
parity) added same day after injection-gating and test-coverage exploration.
§13 items ALL RESOLVED (final code audits 2026-08-21: JobCtx, JobEnvSnapshot,
sbx process model, truncation). Gate audit 2026-08-21 (LEAKY, 8 findings,
11/11 receipts verified) remediated here. Oracle plan review 2026-08-21
(agent_oracle_f0d3932c): architecture + T0–T7 decomposition APPROVED;
findings folded in: shared JobState cell w/ pgid-clear (pid-reuse guard),
use_agent reset-site correction (there is NO full-context /clear), explicit
shutdown mechanism, capacity-0 supervisor consumer audit, panic-notification
semantics, tail-side UTF-8 flooring, lock discipline, parallelization map.
Re-gate on v1.5 (all prior findings verified fixed, receipts 100%): 3
residual gaps fixed in v1.6 — lazy plain-session supervisor init (R9/§6),
T2∥T3 function/mod.rs caveat (§11), collect-side cap ownership (§3/§6).

## 0. Decision record

Rulings already made (Oracle 2026-08-21, user same day) — do not relitigate
during implementation:

- **R1 (registry)**: One unified `Supervisor` whose `handles` map holds
  `TaskHandle = enum { Agent(AgentHandle), Job(JobHandle) }`. REJECTED: a
  kind-flag field on a generalized struct (Option-soup; a Job with a
  `child_supervisor` becomes representable) and a parallel `JobRegistry`
  (guardrail, `cancel_recursive`, and all 5 turn-end sites already traverse
  `ctx.supervisor`; a second registry duplicates traversal and creates two
  cleanup paths that drift).
- **R2 (invocation shape)**: Meta-tool `job__start { tool, arguments }`,
  mirroring the `mcp_invoke_*` wrapper shape the model already sees in its own
  transcripts. REJECTED: injecting a `background: true` param into every tool
  schema — coyote doesn't own MCP passthrough or argc-generated schemas, the
  param must be stripped before dispatch, and strict parsers
  (`additionalProperties: false` MCP servers, argc bins) hard-fail on leaks.
- **R3 (push mechanism)**: Notifications are delivered by (a) merging a
  `system_notifications` key into the LAST real ToolResult of a batch (the
  proven `pending_escalations` channel) and (b) the turn-end guardrail user
  message. REJECTED: a mid-turn synthetic `[SYSTEM NOTIFICATION]` user
  message — attribution confusion (model reads it as user intent and breaks
  off its tool chain), no clean seam (`merge_tool_results` at 5 sites vs one
  function in `eval_tool_calls`), fake "user said" artifacts persisting in
  saved sessions, and ZERO coverage gain: the only moment with no ToolResult
  to merge into is an empty batch, which IS turn-end, which the guardrail
  owns. This is the same transcript-integrity principle as the
  `__escalation_notification` phantom-call fix (see memory
  `coyote-escalation-notification-bug`): never fabricate transcript content
  the model didn't produce.
- **R4 (agents too)**: Completion notifications are ALWAYS-ON for both jobs
  and spawned agents. REJECTED: per-spawn opt-in (asymmetry doubles prompt
  guidance; the model polls anyway; volume is one terse event per
  completion). A `notify: false` opt-out may be added later if fanout spam
  materializes — not in v1. NOTE: this is one of the four deliberate
  behavior deltas that apply even when jobs are disabled (§9.1).
- **R5 (persistence)**: Jobs die with the coyote process. No pid files, no
  output spooling, no reattach-after-restart in v1. Half-building persistence
  is worse than not having it.
- **R6 (check semantics — user-ruled 2026-08-21)**: `job__check` AND
  `agent__check` are pure status probes; they NEVER consume the handle.
  `collect` is the single retrieval+reclaim verb. REJECTED: today's
  agent__check finished→collect delegation (supervisor.rs:844) — it returns
  an unbounded payload the model didn't opt into (job results can be huge),
  makes check's return shape bimodal, breaks the documented check-then-collect
  pattern (follow-up collect errors on the consumed id), and silently defeats
  the H7 guardrail backstop (once consumed, an overlooked result is
  unrecoverable). With push notifications the round-trip saving is marginal:
  completion usually arrives as a notification naming the collect command,
  not via polling. Finished JOB checks return status + output_tail preview +
  the exact collect command; finished AGENT checks return status + the exact
  collect command only (agents have no ring buffer — nothing to preview).
  Deliberate delta, see §9.1.
- **R7 (concurrency knob — user-ruled 2026-08-21, AMENDED same day)**:
  `max_concurrent_jobs` is BOTH a global `Config` field (concrete, default
  **5**) AND an `Option<usize>` override on `AgentConfig`, resolved
  agent-override-first exactly like `max_tool_result_chars`
  (src/function/mod.rs:332-337 pattern:
  `agent.field().or(global)`). Rationale for the amendment: plain (non-agent)
  sessions have NO `AgentConfig` at all (§2), and jobs — unlike agent
  spawning — ARE available in plain sessions (R9), so the global field is
  the only knob there; a serde default alone is not user-tweakable.
  Role/session-runtime overrides remain DEFERRED (parity with
  `max_concurrent_agents`). Implementation gotcha: `AppConfig` duplicates
  `Config` fields across FOUR touch points (struct ~app_config.rs:66,
  Default ~:151, From<Config> ~:237, env override ~:573) — miss one and the
  field silently stays default.
- **R8 (feature-off gating — user-ruled 2026-08-21; amended 2026-08-24)**:
  the job feature is OFF for a context when EITHER (a) the effective
  `max_concurrent_jobs` is 0, OR (b) `function_calling_support` is false
  (config mod.rs:232 / AppConfig app_config.rs:36, env-overridable
  app_config.rs:523). When OFF, the `job__*` tool declarations are NOT
  injected, the job prompt instructions are NOT injected, and no job
  capacity exists — mirroring exactly how `can_spawn_agents: false` omits
  the agent__* family today (declarations gated at Agent::init
  agent.rs:224-226; prompt text at agent.rs:439-441; supervisor creation at
  rc.rs:4139-4143). A model that has never seen a job__ declaration cannot
  call one; `max_concurrent_jobs: 0` = the feature does not exist for that
  context. The function-calling gate mirrors how skill/memory/rag function
  injection already checks `app.function_calling_support` at Agent::init
  (agent.rs:231, 238, 252) — NOT how agent__* does it (agent__* injects
  unconditionally and only refuses at runtime, supervisor.rs:526/710;
  job__* must gate at INJECTION so neither declarations nor instructions
  ever reach the prompt). The model-level `supports_function_calling()`
  strip at input.rs:260 remains the generic per-request safety net but is
  NOT the gate — it strips declarations only, never injected instructions.
- **R9 (plain sessions)**: jobs ARE available outside agent contexts
  (plain REPL/role sessions), governed by the global config value alone.
  This requires creating the Supervisor outside `use_agent`: init condition
  becomes `can_spawn_agents || jobs_enabled` — where `jobs_enabled` is
  `function_calling_support && effective_max_concurrent_jobs > 0` (R8) — with
  agent capacity 0 when `can_spawn_agents` is false (Supervisor::register
  already rejects at capacity — tested behavior). `exit_agent`'s Functions
  rebuild (rc.rs:4177-4186) must retain job declarations when jobs are
  enabled. Creation sites (gate-ruled 2026-08-21): EAGER in `use_agent`
  with the amended condition (existing site rc.rs:4139-4143); LAZY in plain
  sessions — `job__start` get-or-inits `ctx.supervisor` (agent capacity 0)
  on first use (job handlers run on the sequential `&mut ctx` path, so they
  can set it); `exit_agent`'s unconditional `self.supervisor = None`
  (rc.rs:4192) STAYS as-is — `cancel_recursive` already ran (rc.rs:4190)
  and the next `job__start` lazily recreates. A plain session that never
  starts a job keeps `supervisor: None` — bit-identical to today (§9.2.3).
- **R10 (REPL surfaces — user-ruled 2026-08-24)**: `job__*` are BUILT-IN
  functions — always on when `jobs_enabled`, never individually
  toggleable:
  - `.info tools` MUST list them when enabled. This is FREE: `tools_info`
    (rc.rs:700) renders the `select_functions` output, so it falls out of
    §3 injection + carve-outs — no dedicated code.
  - `.list tools`, `.tool enable/disable` validation, AND the `.tool`
    tab completions MUST all exclude them. ONE change covers all three
    surfaces: add `job__` to the built-in prefix exclusion list in
    `concrete_tool_names()` (rc.rs:1283-1308; today user__/mcp_/todo__/
    agent__/memory__/skill__/rag__) — `.list tools` (rc.rs:2732),
    `toggle_tool` validation (rc.rs:1356), and `repl_complete`'s `.tool`
    arm (rc.rs:3458) all draw from that one pool. This also keeps `job__`
    out of `toggle_tool`'s disable-path pool materialization, so job
    tools can never leak into a persisted `enabled_tools` list.
  - Consequently `.tool enable job__start` errors with the existing
    "Unknown tool" teaching error — identical to agent__/todo__ today.
- **R11 (subagents & tool-filtered contexts — user-ruled 2026-08-24)**:
  - An explicit `enabled_tools` list (role, session, agent, or graph LLM
    node `tools:` — node lists arrive as role-level enabled_tools, see the
    comment at rc.rs:2043-2046) can neither GRANT nor REVOKE `job__*`;
    presence is governed solely by `jobs_enabled` (R8). Mechanism:
    `JOB_FUNCTION_PREFIX` joins BOTH infra carve-outs in
    `select_enabled_functions` (the §3 bullet: agent-path retain
    ~rc.rs:2139-2150, non-agent builtin re-add ~rc.rs:2109-2128); extend
    `select_functions_preserves_infra_tools_under_agent_filter`
    (rc.rs:5642).
  - A context WITH a tool list CAN use `job__*` (user-confirmed) — but
    `job__start` additionally validates that the requested tool is
    AVAILABLE in the calling context (§3), so a narrowed node can only
    background tools it could call in the foreground; `job__start` must
    not be an `enabled_tools` bypass.
  - A context with NO enabled tools still SEES `job__*` — consistent with
    how agent__/todo__/user__ infra prefixes survive an empty filter
    today. Harmless: every `job__start` there rejects with the
    context-availability teaching error, since nothing is startable.
    NAMED CONSEQUENCE (Oracle N2, accepted): this flips previously
    TOOLLESS contexts from `tools: None` to `Some([five job__* decls])`
    request shape — `select_functions` returns None only when the merged
    set is empty (rc.rs:2277-2287; pinned by
    `select_functions_returns_none_when_no_tools_enabled`, rc.rs:5493,
    which needs a jobs-off guard). E.g. a pure-text graph node with
    `tools: []` now sends a tools array. T7 pins both states.
  - Subagents: NO special machinery. Each spawned agent runs Agent::init
    → R8 gating with its own effective `max_concurrent_jobs` (agent
    override → global); its jobs register in its OWN supervisor; parent
    `cancel_recursive` already cascades through child supervisors (T1).

Explicit non-goals: job persistence across restarts (R5); backgrounding
internal state-mutating tools (§4 whitelist); a general mid-turn user-message
injection mechanism (R3); job-to-job messaging (jobs have no inbox — they are
processes/futures, not conversants).

## 1. Summary

Today every tool call blocks the turn: `eval_tool_calls` (src/function/
mod.rs:262) is awaited inline from `call_chat_completions` (src/client/
common.rs:526), so a 10-minute build or a slow MCP call pins the model until
it finishes. Coyote already solved this exact problem for *agents*
(`agent__spawn`/`check`/`collect`/`cancel` on tokio tasks) — this design
generalizes that machinery to *tool invocations*:

1. A **`job__*` tool family** — `job__start`, `job__check`, `job__collect`,
   `job__cancel`, `job__list` — that runs a whitelisted tool call as a
   background tokio task registered in the existing `Supervisor`.
2. A **per-context `NotificationQueue`** so job and agent completions are
   *pushed* to the model (merged into the next batch's last ToolResult, and
   enumerated at turn-end by the guardrail) instead of relying on the model
   remembering to poll.
3. A **guardrail predicate fix** so finished-but-uncollected handles block
   turn-end with exact collect commands — which also fixes a latent bug where
   a finished-but-uncollected *agent's* output is silently abandoned today.

## 2. What exists today (verified)

Turn loop & dispatch:
- `ask()` (src/repl/mod.rs:1416) recurses: `call_chat_completions[_streaming]`
  → `eval_tool_calls` → non-empty `tool_results` re-enter `ask()` via
  `input.merge_tool_results` (repl/mod.rs:1469-1476). Empty results = turn
  end. Same loop duplicated in src/main.rs:581, src/acp/server.rs:214,
  src/graph/llm.rs:258-302, and the child loop `run_child_agent`
  (src/function/supervisor.rs:435-504).
- `eval_tool_calls` (mod.rs:262) partitions a batch on `is_mcp_meta_function`
  (mod.rs:291-293): calls carrying any of the FIVE MCP meta-prefixes
  (`mcp_invoke_/mcp_search_/mcp_describe_/mcp_read_/mcp_prompt_` — the
  catalog grew read/prompt with the 2026-08-25 MCP resources/prompts merge,
  src/mcp/mod.rs:36-48) run CONCURRENTLY via
  `future::join_all` with `&RequestContext` (`eval_mcp` takes `&ctx`,
  mod.rs:296-300); everything else runs SEQUENTIALLY because `ToolCall::eval`
  (mod.rs:1420) takes `&mut RequestContext`. Results re-sorted by original
  index; per-call errors soft-fail as `{"tool_call_error": ...}`; nulls
  normalize to `"DONE"` (mod.rs:358-364) because empty results end the turn.
- Post-processing order matters: dedup/loop check (mod.rs:271-284) →
  `max_tool_result_chars` truncation (mod.rs:332-337) → escalation injection
  into the last result at depth 0 (mod.rs:343-347).

Supervisor & agents:
- `agent__spawn` (`handle_spawn`, supervisor.rs:649) builds a child ctx
  (`RequestContext::new_for_child`, request_context.rs:465 — note:
  `supervisor: None` fresh per child, but `escalation_queue` CLONE-INHERITED
  from parent at rc.rs:497) and `tokio::spawn`s `run_child_agent`
  (supervisor.rs:778-795). The result lives in the
  `JoinHandle<Result<AgentResult>>` inside `AgentHandle { id, agent_name,
  depth, inbox, abort_signal, join_handle, child_supervisor }`
  (src/supervisor/mod.rs:30-38), registered in `Supervisor { handles:
  HashMap }` (mod.rs:40-45).
- `agent__check` polls `is_finished()` (supervisor.rs:827); on a finished
  agent it silently DELEGATES to `handle_collect` (supervisor.rs:844), i.e.
  check can consume the handle. `agent__collect` = 200ms poll loop that
  breaks out early with `status: "pending"` if escalations are pending
  (supervisor.rs:894-937), then `Supervisor::take` + await, optional LLM
  summarization above a threshold (supervisor.rs:1401-1451). `agent__cancel`
  = take + `cancel_recursive()` on child supervisor + abort signal + ≤5s
  join wait (supervisor.rs:1044-1067).
- Turn-end guardrail `check_pending_agents_guardrail` (supervisor.rs:71-99):
  called at all 5 turn-end sites; injects a `[SYSTEM GUARDRAIL]` user message
  via `Input::from_str` and recurses; after `PENDING_AGENTS_GUARDRAIL_MAX = 3`
  reminders, `ForceTerminate` + `cancel_recursive()`. CRITICAL:
  `pending_agent_ids` (supervisor.rs:41-53) filters
  `is_finished == Some(false)` — finished-but-uncollected handles pass the
  guardrail silently and their results are dropped.
- `run_child_agent` ends with `supervisor.read().cancel_recursive()`
  (supervisor.rs:498-500) — child-owned handles get cleanup for free IFF job
  cancellation is wired into `cancel_recursive`.
- Context-reset sites (Oracle-corrected 2026-08-21): there is NO
  full-context `/clear` command — the REPL has only `.clear todo`
  (repl/mod.rs:1320-1335). The real supervisor-reset sites are `use_agent`
  (rc.rs:4155 — REPLACES `self.supervisor` WITHOUT calling
  `cancel_recursive` today) and `exit_agent` (rc.rs:4190 — calls it). Also
  relevant: `process::exit` bypasses destructors at main.rs:672 (shell
  execute), main.rs:273, logs.rs:18, config/mod.rs:783.

Tool-injection gating & config layering (verified 2026-08-21):
- agent__* declarations exist ONLY inside an agent's `Functions`: appended by
  `Functions::append_supervisor_functions()` (mod.rs:616-621), called at
  `Agent::init` iff `can_spawn_agents` (agent.rs:224-226). `spawnable_agents`
  and depth limits do NOT gate injection — enforced at spawn time
  (supervisor.rs:663-670, Supervisor depth check).
- Per-request tool list: `RequestContext::select_enabled_functions`
  (rc.rs:2039-2172); agent path copies agent declarations (:2098-2109) with
  a `SUPERVISOR_FUNCTION_PREFIX` carve-out surviving role `enabled_tools`
  filters (:2111-2121); non-agent path has a builtin-prefix filter
  (:2077-2096).
  A job__ prefix needs the same carve-outs.
- Prompt gating: `interpolated_instructions()` pushes
  `DEFAULT_SPAWN_INSTRUCTIONS` (prompts.rs:74) iff
  `can_spawn_agents && inject_spawn_instructions` (agent.rs:439-441).
- Supervisor exists ONLY when `use_agent` runs with `can_spawn_agents`
  (rc.rs:4139-4143); plain REPL/role sessions have `supervisor: None`, no
  agent__* declarations, and CANNOT spawn agents. `exit_agent`
  (rc.rs:4175-4192) rebuilds `Functions::init` + user-interaction functions
  only (:4113-4116).
- Global-vs-agent fallback precedent: `Option<T>` on AgentConfig + concrete
  global Config field + `agent.field().or(global)` at the resolution site
  (max_tool_result_chars: mod.rs:332-337; compression_threshold:
  session.rs:569-575). `max_concurrent_agents` does NOT follow it (agent-only,
  serde default 4) — R7 makes the jobs knob follow the Option-fallback
  pattern instead.

Safe injection channels (the only two):
- Key-merge into the last real ToolResult: `inject_escalation_notification`
  (mod.rs:366-381) adds `pending_escalations` + `escalation_instruction` keys
  to the output object, wrapping non-objects as `{"output": old, ...}`. Runs
  AFTER truncation. Gated `ctx.current_depth == 0` on the SHARED root
  escalation queue.
- Synthetic user message at turn-end only (guardrail, auto-continue at
  repl/mod.rs:1477-1524, `.recover`).

Loop detection: root `ToolCallTracker::default()` = `new(2,3)` (mod.rs:
2336-2353) — TWO identical consecutive calls trip it. Unmitigated, a model
calling `job__check {id}` twice in a row is flagged as a loop.

External argc tools: results are returned via the `LLM_OUTPUT` temp file, not
stdout (run_llm_function protocol; see memory
`coyote-execute-command-swallowed-output` for the hardened error semantics).
Stdout/stderr are human-facing side output.

Test coverage today (inventoried 2026-08-21; all inline `#[cfg(test)]`, no
tests/ dir): ~153 tests across the area. FULL coverage: escalation.rs (11),
mailbox.rs (11), taskqueue.rs (17), Supervisor register/take/capacity/depth
unit tests (11), single-call eval_tool_calls behaviors (soft-fail, DONE
normalization, escalation injection incl. non-object wrap), tracker + dedup
unit tests, declaration/registry tests (function/mod.rs ≈55,
function/supervisor.rs 48). ZERO coverage (ranked by regression risk — the
basis for T0 in §11): handle_collect (poll loop, escalation early-out,
take+await, summarize threshold); guardrail ForceTerminate arm + counter
reset; multi-call eval_tool_calls (MCP/sequential partition, index re-sort,
loop-alert-in-batch); handle_spawn (capacity/depth/allow-list through the
handler); run_child_agent; turn-loop call sites + merge_tool_results;
max_tool_result_chars truncation; handle_check finished→collect delegation;
handle_cancel of a RUNNING agent / cancel_recursive; ToolCall::eval prefix
routing.

## 3. The `job__*` tool family

New prefix `JOB_FUNCTION_PREFIX = "job__"` routed in `ToolCall::eval`'s
prefix chain (mod.rs:1420) before the external-command fallthrough, handled
in src/function/jobs.rs (new), mirroring src/function/supervisor.rs.

### Availability gating (R8, R9)

Effective value: `agent.max_concurrent_jobs().or(global).unwrap_or(5)`.
`jobs_enabled = function_calling_support && effective_max_concurrent_jobs > 0`
(R8); when `jobs_enabled` is false the feature is OFF for that context:
- No `job__*` declarations appended (`append_job_functions()` called
  conditionally at Agent::init, mirroring agent.rs:224-226, AND at the
  non-agent `Functions::init` sites — rc.rs:3836, rc.rs:4177,
  app_state.rs:71 — so plain sessions get jobs per R9). Every one of these
  call sites checks the full `jobs_enabled` predicate, including
  `function_calling_support` (pattern: agent.rs:231/238/252, where
  skill/memory/rag functions already gate on it).
- No job prompt instructions in `interpolated_instructions()` (mirror
  agent.rs:439-441, but on the full `jobs_enabled` predicate — with
  function calling off, NO job instructions appear anywhere, since a model
  that cannot emit tool calls must not be taught tool syntax). Plain
  sessions get NO injected job prompt text at
  all — guidance there comes solely from the job__* tool descriptions
  (no plain-session analog of interpolated_instructions exists; this is
  the simplest R8-consistent ruling).
- Prefix carve-outs added at rc.rs:2139-2150 (agent path role filter) and
  rc.rs:2109-2128 (non-agent builtin filter) apply only when enabled.
  These carve-outs are what make `.info tools` show job__* and what makes
  job__* survive role/session/agent/graph-node `enabled_tools` filters
  (R11); extend `select_functions_preserves_infra_tools_under_agent_filter`
  (rc.rs:5642) accordingly.
- REPL surfaces (R10): add `job__` to the built-in prefix exclusion list
  in `concrete_tool_names()` (rc.rs:1283-1308) — that ONE pool feeds
  `.list tools` (rc.rs:2732), `.tool enable/disable` validation
  (toggle_tool rc.rs:1356, incl. its disable-path pool materialization),
  and the `.tool` tab completions (repl_complete rc.rs:3458). `.info
  tools` (tools_info rc.rs:700) needs NO dedicated code — it renders
  `select_functions` output.
- Supervisor init condition becomes
  `can_spawn_agents || jobs_enabled`; when only jobs
  are enabled, agent capacity is 0 (register-at-capacity rejection is
  existing tested behavior).

### `job__start { tool: string, arguments: object }`
- Description mirrors `mcp_invoke_*`: "`arguments` is the same object the
  tool takes when called directly." `arguments` schema is free-form object.
- Validates the tool against the backgroundable whitelist (§4). Rejection is
  a TEACHING error: names the tool, why it can't background (mutates agent
  state / interactive), and the whitelist categories — the model WILL try
  `job__start {tool: "memory__write"}`.
- Validates the tool is AVAILABLE in the calling context (R11): the
  requested tool must appear in the PER-REQUEST DECLARED-NAMES STASH —
  the set of function names actually sent to the model for the current
  request, captured in `before_chat_completion` (see Validation hardening
  rule 1 for the mechanism and why recomputing via `extract_role` is
  WRONG). This inherently applies role/session/agent/LLM-node
  `enabled_tools` filters and enabled-MCP-server filters, because the
  stash IS the filtered output. Whitelisted-but-filtered-out →
  TEACHING error: "'X' is not enabled in this context". Without this
  check, `job__start` would be an `enabled_tools` bypass for narrowed
  contexts (e.g. graph LLM nodes with a `tools:` list).
- Runs any pre-execution gate SYNCHRONOUSLY before returning — a detached
  task cannot touch the tty (H3). AUDITED (§13.2): NO gates exist on the
  external-tool path today (no approval prompts/denylists; the only
  pre-spawn checks are dedup/loop/unknown-tool inside eval_tool_calls), so
  this rule is future-proofing, not migration work.
- Returns immediately: `{ status: "ok", job_id: "job_<hex>", tool, message:
  "Running in background. Check with job__check, block with job__collect,
  cancel with job__cancel. You will receive a system_notifications entry on
  completion. Jobs do not survive coyote exiting." }` (The
  system_notifications sentence is transiently aspirational on a T3-only
  tree — notifications land in T4; both merge in the same PR, accepted
  window.)
- Capacity: separate per-context `max_concurrent_jobs` budget (R7: agent
  override → global config → default 5) — jobs must not consume the agent
  `max_concurrent` budget. Jobs skip the agent depth check (no depth
  semantics).

### `job__check { id }`
- Returns `{ status: running|completed|failed, id, tool, elapsed_secs,
  output_tail, output_bytes_captured, tail_truncated }`. `status` is read
  from the JobHandle's shared JobState cell (§6) — the JoinHandle result is
  never peeked (it can't be without consuming it). `cancelled` is
  deliberately absent from the enum: job__cancel removes the handle from
  the registry, so a later check on that id hits the unknown-id error
  below.
- Unknown id → teaching error: "No job 'X' is registered — it may have
  already been collected or cancelled. job__list shows active jobs."
- `output_tail` = tail of the live ring buffer (§5) — progress telemetry,
  NOT the result. Tail extraction uses `String::from_utf8_lossy` (the ring
  can split a multibyte char at the wrap point).
- NEVER consumes the handle (R6). `agent__check` is ALIGNED to the same
  pure-probe semantics in T2 (today it delegates finished→collect at
  supervisor.rs:844); a finished job check returns status + tail preview +
  the exact collect command; a finished agent check returns status + the
  exact collect command (no preview — no ring buffer).
- Per-handle consecutive-check counter: after ~5 checks with no state
  change — defined as the `(status, output_bytes_captured)` tuple
  unchanged — append hint: "still running; call job__collect to block, or
  do other work — a system notification will fire on completion" (H8's
  semantic rate-limit, replacing tracker special-casing).

### `job__collect { id, tail_lines?: number }`
- Mirrors `agent__collect`: poll loop with the same early-out that surfaces
  `pending_escalations` instead of deadlocking (supervisor.rs:904-914
  pattern), then `Supervisor::take` + await.
- Result assembly per tool class (§5): argc/external → read `LLM_OUTPUT`
  with the hardened missing/empty semantics, plus final `output_tail` and
  exit status; MCP → the future's return value. Ownership split (gate-ruled):
  the JOB TASK reads LLM_OUTPUT after `wait()` (protocol semantics §4) and
  returns the RAW output in `JobResult`; the COLLECT HANDLER applies the
  default tail cap / `tail_lines` and reports `result_truncated` in its own
  response.
- `JoinError` (panic) maps to `status: "failed"` with the panic message —
  never hangs — and still returns whatever ring-buffer content exists (H6).
- Output capping (RESOLVED §13.6): `max_tool_result_chars` CANNOT be the
  backstop — its global default is null/no-cap (config mod.rs:266,
  config.example.yaml:206) and `truncate_if_needed` keeps the HEAD
  (mod.rs:404-413), the wrong end for build logs (failures land at the
  tail). Job results get their own TAIL-biased cap: default keep the LAST
  50,000 chars with a `[truncated: kept last N of M chars]` header, plus
  the optional `tail_lines` param for explicit control (`tail_lines`
  applies to the RESULT text — LLM_OUTPUT contents / MCP return — never to
  the ring buffer). Both the default cap and `tail_lines` cuts floor to a
  char boundary (do not reintroduce the §9.1 UTF-8 bug on the tail side).
  No LLM summarization in v1 (supervisor.rs:1401-1451 ships the entire
  output to the summarizer as one message — a huge log would blow its
  context window anyway).

### `job__cancel { id }`
- Take handle → kill process group per §6 platform strategy (only if the
  JobState pgid is still set) → abort JoinHandle → ≤5s join wait →
  `{ status: "cancelled", id, output_tail }` (partial output included).

### `job__list`
- All registered jobs for THIS agent's supervisor: id, tool, status (from
  the JobState cell), elapsed, bytes captured.

Cross-kind misuse errors teach: `agent__collect("job_x")` → "'job_x' is a
background job, not an agent — use job__collect"; `job__collect("agent_...")`
→ inverse. IDs are namespaced (`job_` prefix) to make this cheap.

### Validation hardening (loophole audit, 2026-08-24)

WHY this matters more than it looks: in the FOREGROUND, tool access is
enforced by request declaration — providers only let the model call tools
sent in the request payload, so `ToolCall::eval` merely checks POOL
membership (`extract_call_config_from_ctx` bails "Unexpected call" for
undeclared names, mod.rs:1814-1825; test mod.rs:2564) and then resolves
the name against bin dirs + PATH (`run_llm_function`). `job__start`'s
`tool` parameter is a FREE STRING — it bypasses the provider-level
constraint entirely, so the R11 declared-names check is THE enforcement
point (and is strictly tighter than foreground's pool check: enabled set,
not pool). Binding rules:

1. **Exact-match only, validated name drives everything.** The R11 check
   is exact string equality (post-trim) against the set of tool names
   ACTUALLY DECLARED to the model for the CURRENT request. It must NOT
   recompute `select_functions(&extract_role())` at eval time:
   `extract_role` layers session → agent → role (rc.rs:996-1019), and
   during a graph LLM-node run it returns the UN-narrowed
   `agent.to_role()` while the request was built from the node's
   swapped-in role (`ctx.role` — graph/llm.rs:174-188, 251-252 →
   input.rs:433 `functions: ctx.select_functions(role)`), so recomputing
   would validate against the FULL pool and reopen the exact bypass R11
   closes (Oracle B1). Mechanism (Oracle-ruled 2026-08-24):
   `before_chat_completion` (rc.rs:896) — already called at ALL SEVEN
   turn-loop sites (repl/mod.rs:1435/1444, main.rs:562/573/632,
   acp/server.rs:204, supervisor.rs:456 child loop, graph/llm.rs:257) —
   stashes the request's declared-function NAME SET (a cheap
   `HashSet<String>` from the Input's select_functions output, NOT a
   Role clone) into ctx; `job__start` validates against the stash.
   REFRESHED on every request — a stale stash from a prior turn must be
   impossible. Desirable side effect: a mid-batch `skill__load` widening
   the pool does NOT grant `job__start` access until the next request —
   the check is literally "declared to the model this turn".
   `mapping_tools` aliases are NOT expanded — concrete
   declaration names only. The raw model string is NEVER passed to
   `run_llm_function`/bin-dir/PATH resolution or to the agent-tool
   mapping (mod.rs:1785-1812): class dispatch (external vs `mcp_invoke_*`)
   and cmd resolution derive from the MATCHED declaration, not from the
   model's string. (Otherwise `job__start {tool: "bash"}` or a
   path-shaped name would execute an arbitrary PATH binary that was
   never a declared tool.)
2. **Both gates always run**: §4 whitelist AND R11 context availability;
   a name must pass both regardless of order (each has its own teaching
   error).
3. **Disabled state never falls through.** VERIFIED eval order (Oracle
   B2): `extract_call_config_from_ctx/from_agent` runs BEFORE the prefix
   chain (mod.rs:1420-1428) and bails "Unexpected call" for any name not
   in the declaration pool (mod.rs:1814-1825) — so with jobs disabled
   (job__* never declared, R8) a hallucinated `job__start` dies at that
   bail as a per-call soft-fail and can never reach the external-command
   fallthrough. RULING (B2 option (b)): this existing "Unexpected call"
   soft-fail IS the specified disabled-state behavior — exactly what
   agent__* produces in non-agent contexts today. Option (a)
   (pre-extract routing on the raw name to emit a nicer teaching error)
   REJECTED: reordering eval semantics for an error-message nicety, zero
   security gain; the mod.rs:2564 test must stay green as-is. The
   `job__` prefix arm in the chain consequently only ever executes when
   declarations exist (jobs enabled).
4. **MCP server derivation from the validated name.** `JobCtx`'s
   source, the full `McpRuntime` map, contains every STARTED server
   (src/config/tool_scope.rs:44-47), not just context-enabled ones. The
   server is derived solely from the validated `mcp_invoke_<server>`
   declaration name (the invoke_mcp_tool pattern, mod.rs:1614-1636,
   prefix-strip at :1619) — and to
   make the discipline STRUCTURAL (Oracle N3): `JobCtx` is constructed
   at `job__start` AFTER validation, so it snapshots a SINGLE-ENTRY
   `McpRuntime` holding only the validated server's
   `Arc<ConnectedServer>` — the job task cannot reach any other server
   even by bug. The
   inner `tool` argument stays foreground-parity (opaque server-side
   name — no new gate, none exists today).
5. **Handle isolation across contexts.** All five `job__*` handlers
   operate EXCLUSIVELY on `ctx.supervisor` — never `parent_supervisor`
   (which exists in child contexts for escalations) — so a subagent
   cannot check/collect/cancel its parent's jobs or vice versa. Forged
   or stale ids hit the cross-kind/unknown-id teaching errors.
6. **Residual, accepted + documented**: job OUTPUT is untrusted text —
   the same prompt-injection surface as foreground tool output. It cannot
   mint declarations, grant tools, or forge handles; worst case is
   persuading the model to call tools it already has. No v1 mitigation
   beyond the existing one (output is data, delivered inside a
   ToolResult).

## 4. Backgroundable-tool whitelist

Forced by the architecture: `ToolCall::eval` takes `&mut RequestContext`,
which cannot move into a detached task (H2). Backgroundable = tools whose
execution can run from an owned snapshot:

| Class | Backgroundable | Execution path in job task |
|---|---|---|
| `execute_command` + ALL external command tools — bash (argc), JavaScript/TypeScript, Python (user-ruled 2026-08-21; no per-tool opt-in flag) | YES (the whole point) | Extract the spawn logic out of the eval path into a free function taking `(Arc<AppState>, JobEnvSnapshot, args)` — do NOT route through `ToolCall::eval`. `JobEnvSnapshot` = the audited field set below. |
| `mcp_invoke_*` | YES | Already `&ctx` (`eval_mcp`); job task owns `JobCtx { mcp_runtime: McpRuntime, current_depth: usize }` — the runtime is a SINGLE-ENTRY snapshot holding only the validated server's `Arc<ConnectedServer>` (§3 hardening rule 4, Oracle N3). Job MCP path = `invoke` → `render_tool_result` (mod.rs:1635 foreground parity; FREE function, see §13.1). AUDITED (§13.1): that is eval_mcp's COMPLETE transitive ctx surface. McpRuntime is `#[derive(Clone)]`, shallow Arc map (src/config/tool_scope.rs:44-47); OAuth refresh is transport-embedded (auth_client.rs) and needs no AppState. NOTE: MCP jobs have NO timeout (`COYOTE_TOOL_TIMEOUT` is process-path only); a hung MCP job is recoverable via `job__cancel` (abort drops the future) — accepted v1, stated in the tool description. |
| `mcp_search_/mcp_describe_/mcp_read_/mcp_prompt_` | NO (pointless — fast) | Teaching error: "sub-second call; invoke directly." (All four non-invoke meta-families; meta-declarations are capability-gated per server — `gated_meta_function_prefixes`, function/mod.rs:416-426: invoke⇔tools, read⇔resources, prompt⇔prompts — which composes with the declared-names stash automatically: a capability the server never advertised is never declared, so `job__start` rejects it with the standard not-declared error.) |
| `agent__*`, `job__*` | NO | "Already asynchronous — use them directly." |
| `todo__*`, `memory__*`, `skill__*`, `user__*` and other internal `&mut ctx` tools | NO | "Mutates agent/session state; must run in-turn." |
| fs_* / ast_grep / grep-class builtins | NO in v1 | Fast; not worth the snapshot surface. Revisit only with evidence. |

Snapshot semantics (document in tool description): config/model/env changes
made after `job__start` do not affect a running job. The job must not mutate
shared session state — it only produces output.

Directionality ruling (user, 2026-08-24): the async systems compose ONE WAY
ONLY — agents (and graph LLM nodes) may start jobs, but a job can NEVER
start an agent, another job, or invoke any built-in — anything else would
duplicate/defeat the agents' own parallelization system. This is enforced
twice: (1) the whitelist rows above (policy, with teaching errors); (2)
architecturally — every built-in handler requires `&mut RequestContext`,
which cannot move into a detached task (H2); a job task owns only
`JobEnvSnapshot`/`JobCtx`, so there is no ctx, supervisor, or eval loop
inside a job for a built-in to run against, even if the whitelist check
were bypassed.

### JobEnvSnapshot & runner mechanics (§13.2 audit, 2026-08-21)

| Field | Source (foreground receipt) |
|---|---|
| `cmd_name`, `cmd_args` | tool name / agent-tool mapping + JSON args pushed as last arg (mod.rs:1785-1812, 1269) |
| `envs` | `agent.variable_envs()` — `LLM_AGENT_VAR_*` with vault-secret interpolation (agent.rs:469-485); resolved INTO the snapshot at job__start (same plaintext-in-memory exposure as foreground) |
| `agent_name` | drives bin dir + AUTO_CONFIRM-for-graph (mod.rs:2136-2149) |
| `PATH` | functions/agent bin dirs prepended (mod.rs:2136-2149); env-derived dirs (paths.rs:285-287, 327-329) FROZEN at job__start per snapshot semantics |
| `LLM_OUTPUT` | fresh temp path, file not pre-created (mod.rs:2158-2159, utils/mod.rs:313-320) |
| `CLICOLOR_FORCE`/`FORCE_COLOR` | hardcoded =1 (mod.rs:2178-2179) |
| `COYOTE_TOOL_TIMEOUT` | default 1800s, 0 = unlimited (mod.rs:2247-2251); resolved at job__start; jobs honor it (expiry → kill process GROUP + `failed` status) |
| cwd | inherited (foreground sets none) |
| Windows | `LLM_TOOL_DATA_FILE` arg spill + PATHEXT polyfill (mod.rs:2161-2176) |

Runner-mechanics rulings from the audit:
- The external branch of `ToolCall::eval` never actually uses `&mut` —
  `run_llm_function(cmd_name, cmd_args, envs, agent_name)` takes zero ctx
  (mod.rs:1246-1252) — so the extraction is clean, not a refactor risk.
- The foreground runner is SYNC `std::process` polled at 100ms from async
  without spawn_blocking (mod.rs:2181-2316). v1 leaves the foreground path
  UNTOUCHED; the background runner uses `tokio::process` +
  `process_group(0)`. (Foreground sync-in-async is a pre-existing latent
  issue — note as follow-up, out of scope.)
- Foreground TEES stdout/stderr live to the terminal via threads
  (mod.rs:2193-2245); background jobs are CAPTURE-ONLY into the ring
  buffer — no live tee (it would interleave with the foreground UI).
- `stdin = Stdio::null()` is already foreground behavior (mod.rs:2184) — no
  delta for jobs.
- LLM_OUTPUT protocol semantics to replicate when the job task reads the
  file after `wait()` (mod.rs:2283-2316):
  nonzero exit → `tool_call_error` + stderr/stdout + partial output; zero
  exit + missing/empty file → null → "DONE"; unreadable existing file →
  hard error; timeout → kill + `tool_call_error`.

## 5. Output channels: ring buffer vs result (H1 — do not conflate)

For process-backed jobs there are TWO distinct channels:

1. **Live telemetry**: stdout+stderr streamed into a bounded ring buffer on
   the `JobHandle` (`Arc<Mutex<RingBuf>>`, default 64 KiB). Carries a
   monotonic total-bytes counter and a truncation marker so the model knows
   the tail is clipped. This is what `job__check` returns and what makes
   "did it produce any output yet?" answerable mid-run.
2. **The tool result**: for argc/external tools this is the `LLM_OUTPUT`
   file, read ONLY once — by the job task after `wait()` (§3 ownership
   split; the model sees it at collect) — with the same missing/empty-file
   error semantics as the foreground path. The ring buffer is never substituted
   for the result. For MCP jobs the result is simply the future's return
   value; the ring buffer is unused/empty.

Background processes get `stdin = /dev/null` — a script that prompts must
fail fast, not hang the job forever (H3 corollary; already true in the
foreground runner — `Stdio::null()` at mod.rs:2184 — so no behavior delta).

## 6. JobHandle, JobState, process lifecycle, and cleanup (H4)

```rust
// src/supervisor/mod.rs (alongside AgentHandle)
pub struct JobHandle {
    pub id: String,              // "job_<hex>"
    pub tool: String,
    pub started_at: Instant,
    pub join_handle: JoinHandle<Result<JobResult>>,
    pub abort_signal: AbortSignal,
    pub state: Arc<Mutex<JobState>>,     // shared with the job task
    pub output_buf: Arc<Mutex<RingBuf>>, // telemetry channel (§5)
    pub no_change_checks: u32,           // §3 job__check counter
}

pub struct JobState {
    pub status: JobStatus,  // Running | Completed | Failed
    pub pgid: Option<i32>,  // Unix pgid; None on Windows & MCP jobs;
                            // CLEARED by the job task right after wait()
                            // reaps the child (pid-reuse guard, see below)
}

pub enum JobStatus { Running, Completed, Failed }
// job__check/job__list read this cell. Cancelled is unrepresentable here —
// cancel removes the handle from the registry (§3). The job task writes the
// final status (and clears pgid) BEFORE it returns.

pub struct JobResult {
    pub output: Value,          // RAW tool result (job task reads LLM_OUTPUT after wait(); MCP return)
    pub exit_code: Option<i32>, // process jobs only
    pub output_bytes_captured: u64,
}
// The COLLECT HANDLER applies the tail cap / tail_lines (§3) and reports
// `result_truncated` in its own response — deliberately NOT a JobResult
// field (capping is a presentation concern owned by collect).

pub enum TaskHandle { Agent(AgentHandle), Job(JobHandle) }  // R1
```

- `Supervisor.handles` becomes `HashMap<String, TaskHandle>`; `register()`
  branches on variant for capacity; existing agent paths pattern-match.
- Supervisor creation per R9: `can_spawn_agents || jobs_enabled` (R8),
  agent capacity 0 in jobs-only contexts — EAGER in `use_agent`, LAZY via
  `job__start`
  get-or-init in plain sessions (see R9 for the full site ruling incl. the
  exit_agent rc.rs:4192 rule). Never-jobbing plain sessions keep
  `supervisor: None`.
- **Kill discipline (pid-reuse guard — Oracle finding, MANDATORY)**: every
  killer (`impl Drop for JobHandle`, `job__cancel`, `cancel_recursive`,
  timeout expiry) kills the process group ONLY IF `state.pgid` is still
  `Some`, and the job task sets `pgid = None` immediately after `wait()`
  returns (the child is reaped; with `process_group(0)`, pgid == child pid,
  so a stale killpg after reap can SIGTERM an innocent recycled pid).
  Without this guard, the normal collect path (take → await → handle drop)
  fires a killpg against a reaped group on EVERY successful job.
- Spawn processes with `process_group(0)`; cancellation = `killpg(SIGTERM,
  grace 5s, SIGKILL)` then `JoinHandle::abort()` — `abort()` alone does NOT
  kill OS processes, and `kill_on_drop` misses grandchildren.
- Context-reset sites (corrected — there is NO full-context `/clear`):
  `use_agent` (rc.rs:4155) currently REPLACES `self.supervisor` without
  cleanup — T1 adds `cancel_recursive()` on the old supervisor before the
  replacement (Arc-drop alone is unreliable: child ctxs hold
  `parent_supervisor` clones, rc.rs:494, deferring the Drop while the job
  runs on with no transcript). `exit_agent` already calls it (rc.rs:4190).
  A job surviving a context switch has no transcript to report into.
- **Shutdown mechanism**: explicit kill-all (cancel_recursive over the root
  supervisor) on the REPL quit path, where destructors run today. Hard-exit
  paths that bypass destructors (`process::exit` at main.rs:672/257,
  logs.rs:18, config/mod.rs:783) and panics MAY orphan a process group —
  documented, accepted v1 (§9.4.11 checks the normal path). Note:
  `process_group(0)` detaches jobs from coyote's group, so the shell will
  not reap them either.
- `cancel_recursive`/`cancel_all` must handle the Job variant (kill group
  per the discipline above, not just abort signal).
- R5: nothing survives process exit; `job__start`'s response and the system
  prompt say so.
- **Lock discipline** (copy the existing pattern, supervisor.rs:894-942):
  NEVER hold the parking_lot supervisor lock across an await point in any
  job handler; take what you need under a scoped read/write and drop the
  guard before awaiting. The ring-buffer mutex is locked only for the
  memcpy (never across awaits in the stdout pump task).

Module placement: `TaskHandle`/`JobHandle`/`JobState`/`JobStatus`/`JobResult`
live in src/supervisor/mod.rs alongside AgentHandle; the five job__*
handlers, `RingBuf`, `JobEnvSnapshot`, `JobCtx`, and the extracted runner
live in src/function/jobs.rs (new file, mirroring
src/function/supervisor.rs); `NotificationQueue` lives in
src/supervisor/notifications.rs (new file, sibling and structural template:
src/supervisor/escalation.rs).

### Platform strategy (gate finding — killpg is Unix-only)

- Unix: set the group with std's `CommandExt::process_group(0)`
  (std::os::unix::process — no crate needed); kill with
  `libc::killpg(pgid, SIGTERM)` → 5s grace → SIGKILL. NEW SANCTIONED
  DEPENDENCY: `[target.'cfg(unix)'.dependencies] libc = "0.2"` — Cargo.toml
  has no nix/libc direct dep today; this is the approved addition.
- Windows: jobs remain ENABLED; cancellation/timeout uses tokio's
  `Child::start_kill()` + `kill_on_drop(true)` — single-PID, grandchildren
  may leak, which is exactly the foreground timeout path's behavior today
  (mod.rs:2258-2266): parity, not regression. All group-kill code is
  `#[cfg(unix)]`-gated. Win32 Job objects are out of scope v1.
- MCP jobs have no OS process: cancellation = abort signal +
  `JoinHandle::abort()` on both platforms.

## 7. Push notifications: per-context `NotificationQueue`

### Ownership model (critical — opposite of escalations)
`NotificationQueue` follows the `supervisor` pattern in `new_for_child`
(fresh per child, rc.rs:493), NOT the `escalation_queue` pattern
(clone-inherited, rc.rs:497). Each agent gets notifications for ITS OWN
spawned jobs/agents. A shared inherited queue = first-drainer-wins race
delivering the root's notifications into a child's transcript.
Consequently: escalations stay `depth == 0` on the shared root queue;
notifications drain the ctx's OWN queue at ANY depth.

### Event shape (terse — one line per event)
```json
{ "event": "job_completed" | "job_failed" | "agent_completed" | "agent_failed",
  "id": "job_a1b2", "tool_or_agent": "execute_command", "status": "success",
  "next_action": "job__collect --id job_a1b2 for output" }
```

### Producers
- Job task: pushes on completion/failure before returning.
- Agent task: the `tokio::spawn` wrapper in `handle_spawn`
  (supervisor.rs:778-795) pushes into the SPAWNING ctx's queue before
  returning. Always-on (R4).
- NO event on explicit `job__cancel`/`agent__cancel` (the cancel's own
  ToolResult confirms it). YES on failure/panic — with one caveat: a PANIC
  in the job task skips the push (the producer never runs). This is
  INTENTIONAL and covered: the turn-end guardrail's
  finished-but-uncollected predicate still surfaces the handle, and
  collect's JoinError→failed mapping (H6) reports the panic. Do NOT "fix"
  this with a Drop-guard push — it reopens double-delivery.

### Delivery point 1 — mid-turn key-merge (the "push")
Generalize `inject_escalation_notification` (mod.rs:366-381) into ONE
`merge_system_channel(last: &mut ToolResult, escalations, notifications)`
applied at the end of `eval_tool_calls`, keeping the existing
AFTER-truncation ordering. Single pass is MANDATORY: two independent mergers
each applying the non-object wrap double-nest the output
(`{"output": {"output": ...}}`) (H9). Key order: `pending_escalations` first
(children are BLOCKED; completions are not urgent), then
`system_notifications` + a one-line instruction.

Drain-time stale suppression: filter events against current supervisor
registration — if the handle was already `take()`n (model collected before
the drain), DROP the event. Otherwise the model chases a dead id into an
error loop.

### Delivery point 2 — turn-end guardrail (H7 predicate fix)
`pending_agent_ids` → `pending_task_ids` returning `(id, kind, finished)`,
INCLUDING finished-but-uncollected handles (today's `Some(false)` filter
excludes them — supervisor.rs:48). Guardrail prompt renders two sections:
- still running: existing reclaim language, kind-specific commands;
- completed — collect NOW: exact `job__collect`/`agent__collect` commands
  (instant on a finished handle, so this is cheap for the model).
On the `ForceTerminate` strike (3 reminders), discard finished results with
a logged warning — no infinite loop. NOTE: this predicate change also fixes
the latent agent bug where finished-but-uncollected output is silently
abandoned at turn end. All 5 guardrail call sites get this for free.
Deliberate delta, see §9.1.

### Delivery point 3 — none
No other injection point exists or is needed (R3). If the model has nothing
else to do, blocking `job__collect`/`agent__collect` remains the correct
primitive; notifications improve the working-meanwhile case only.

### Race sweep (Oracle-reviewed 2026-08-21 — all benign, no further holes)
- Job completes between drain and merge → event delivers on the next batch
  drain or the turn-end guardrail catches the finished handle. Covered.
- Model collects before drain → stale suppression drops the event. Covered.
- Guardrail-prompted collect, then next batch drains the old event → stale
  suppression. Covered.
- Duplicate mention (event + guardrail, same handle) → benign redundancy.
- check-then-collect on the JobState cell → no consuming race under R6;
  collect's take-under-write-lock is atomic.

## 8. Loop-detection & polling ergonomics (H8)

- Exempt `job__check`, `job__list`, `agent__check`, `agent__list_running`
  from `tool_tracker.check_loop` AND from `record_call` — recording them
  would let `[check, X, check, X]` mask a real X-loop; they must be invisible
  to the tracker. (Root tracker `new(2,3)` trips on just 2 identical calls.)
- Unbounded-polling backstop is the per-handle no-change counter hint (§3),
  a semantic limit where it belongs — not a tracker special case.

## 9. Regression parity: guarantee when jobs are disabled (user requirement)

Hard requirement (user, 2026-08-21): with effective `max_concurrent_jobs: 0`,
ALL existing function-calling and agent__* behavior works IDENTICALLY to
today — as if the feature does not exist.

### 9.1 The ONLY intentional behavior deltas (jobs-independent)

Four approved changes apply even when jobs are disabled. Everything else is
bit-identical. Each ships as its own commit with its own dedicated tests so
it can be reviewed/reverted in isolation:

| Delta | Ruling | What changes |
|---|---|---|
| Agent completion push notifications | R4 | `system_notifications` key can appear on the last ToolResult of a batch after an agent finishes; guardrail prompt gains a "completed — collect now" section. Own commit within T4. |
| `agent__check` never consumes | R6 | Finished-agent check returns status + the exact collect command (no preview — agents have no ring buffer) instead of delegating to collect (supervisor.rs:844). T2 commit (b). |
| Guardrail counts finished-but-uncollected | H7 | Turn-end with an uncollected finished agent now Injects instead of silently dropping the result (fixes latent output-abandonment bug). T2 commit (a). |
| `truncate_if_needed` UTF-8 boundary fix | §13.6 audit | Edge-case BUG today: a cap landing mid-UTF-8-char makes `s.get(..max_chars)` return None → falls back to the FULL untruncated string while still prepending the truncation marker (mod.rs:404-413). Fix: floor the cut to a char boundary. Foreground-visible only in the broken edge case. T2 commit (c). |

If any of these must ALSO be gated off, say so before task
materialization — they are separable.

### 9.2 Zero-diff invariants when jobs are OFF (encode as tests)

1. Tool list byte-identical: no `job__*` declarations in agent or plain
   sessions (`select_enabled_functions` output compared with effective
   `max_concurrent_jobs` 0 vs >0, AND with `function_calling_support:
   false` at any `max_concurrent_jobs` value — both legs of `jobs_enabled`
   independently force the OFF state).
2. Prompt byte-identical: no job instructions in
   `interpolated_instructions()` (plain sessions never get job prompt text
   at any setting — §3); also byte-identical with
   `function_calling_support: false` regardless of `max_concurrent_jobs`.
3. Supervisor creation condition unchanged for agent-only contexts:
   `can_spawn_agents: false` + jobs 0 → `supervisor: None`, exactly today.
4. `eval_tool_calls` behavior on any batch without job__ calls: identical
   partition/order/soft-fail/truncation/injection (pinned by T0 tests).
5. `Supervisor` agent paths (register/capacity/depth/take/cancel_recursive)
   behave identically with the `TaskHandle::Agent` variant — the enum
   refactor is mechanical; T0 tests written BEFORE T1 must pass unmodified
   after it (except type-name churn).
6. `merge_system_channel` with zero notifications + pending escalations
   produces byte-identical output to today's
   `inject_escalation_notification` (both object and non-object wrap cases).
7. Loop-tracker behavior unchanged for all non-exempt tools; exemption list
   is exactly {job__check, job__list, agent__check, agent__list_running}
   (agent-check exemption is part of delta R6's ergonomics; verify it
   cannot mask real loops via the interleave test).
8. No `NotificationQueue` allocation side effects in sessions that never
   spawn/background anything (drain of an empty queue = no-op, no key
   added).

### 9.3 T0 characterization tests (write BEFORE T1; the refactor safety net)

Pin current behavior for every §2 coverage gap (ranked by risk):
1. `handle_collect`: finished-agent take+await happy path; escalation
   early-out returns `status: "pending"` without consuming; summarization
   under/over threshold.
2. Guardrail: `ForceTerminate` after 3 strikes + `cancel_recursive` called;
   counter reset on collect/cancel/no-pending; Inject prompt content (both
   with and without escalations — exists, keep).
3. `eval_tool_calls` multi-call: MCP/sequential partition, result re-sort by
   original index, per-call soft-fail isolation (one failing call doesn't
   poison the batch), loop-alert result shape inside a batch, bail on
   empty-after-dedup.
4. `handle_spawn`: capacity rejection, depth rejection, spawnable_agents
   allow-list rejection — through the HANDLER, not just Supervisor units.
5. `max_tool_result_chars` truncation through eval_tool_calls (agent
   override + global fallback + 0-disables).
6. `merge_tool_results` message-shape test.
7. `handle_check`: pin CURRENT finished→collect delegation, marked as
   deliberately rewritten by T2 commit (b) so the T2 diff is explicit.
8. `handle_cancel` of a RUNNING (not pre-finished) agent: abort + wait path;
   direct `cancel_recursive` unit test.
9. `ToolCall::eval` prefix-routing table test (each prefix → expected
   handler family, unknown → catalog-hint error).
10. `run_child_agent`: BEST-EFFORT — needs a mock client; if infeasible
    without large scaffolding, document as manual case (§9.4) instead of
    faking it.

T0 merges before any refactor commit; T1+ must keep T0 green (allowing only
mechanical type renames), except tests explicitly marked for T2's deliberate
deltas.

### 9.4 Manual verification checklist (user-runnable, post-implementation)

Run once with `max_concurrent_jobs: 0` (global), once unset (default 5),
and once with `function_calling_support: false` (any `max_concurrent_jobs`)
— the last leg must show NO `job__*` declarations and NO job instructions
anywhere in the assembled prompt (R8):
1. Plain REPL: chat + `execute_command` + an fs_* call + an MCP call — works
   as today; `job__*` absent from the tool list (0-case) / present
   (default case).
2. Agent session (e.g. a spawning-capable agent): fan out 2 explores →
   check → collect both; verify outputs and summarization.
3. Escalation round-trip: child asks a user__* question → parent sees
   `pending_escalations` on last tool result → `agent__reply_escalation`
   unblocks child.
4. Guardrail: force a turn-end with a running agent → `[SYSTEM GUARDRAIL]`
   message; let it strike 3 times → force-cancel.
5. Task queue: `task_create` with deps + auto-dispatch agent on
   `task_complete`.
6. Mailbox: `send_message` → child `check_inbox`.
7. Ctrl-c mid-stream (partial text kept, turn ends), then `.recover`.
8. Auto-continue with todo list pending.
9. Load a pre-existing saved session (incl. one containing the old phantom
   `__escalation_notification` if available) — replays benignly.
10. `.agent` enter/exit — tool list correct on both sides of `exit_agent`;
    with a job running, entering/exiting an agent KILLS the job (§6
    reset-site rule).
11. (jobs enabled) `job__start` a 30s `execute_command` → `job__check`
    twice (no loop alert; tail visible) → do other tool work → observe
    `system_notifications` on completion → `job__collect` → result correct;
    then a cancel case; then quit coyote (normal REPL quit) mid-job →
    verify no orphan process (`pgrep`).
12. REPL surfaces (R10) + filtered contexts (R11), jobs enabled:
    `.info tools` lists the five `job__*` entries; `.list tools` omits
    them; `.tool enable`/`.tool disable` tab completion never offers them;
    `.tool enable job__start` → "Unknown tool". An agent/role with an
    `enabled_tools` filter still sees `job__*`; `job__start` of a
    filtered-out (but whitelisted) tool → context-availability error;
    `job__start` of an in-filter tool works.

## 10. Prompt & docs updates

- Roles/system prompts (src/config/prompts.rs:118-119,180 region + agent
  definitions in assets/): document the `job__*` family alongside `agent__*`;
  update the wait-protocol guidance: "for long-running commands, `job__start`
  and keep working; completion arrives as a `system_notifications` entry on
  your next tool result; collect blocks only when you have nothing else to
  do." Note jobs die with the process (R5) and snapshot semantics (§4). Job
  instructions injected only when enabled (R8). Graph-node line (§12
  iteration-burn hazard): "in graph LLM nodes, collect or cancel your jobs
  before ending your final node turn — an uncollected job at node turn-end
  burns node iterations via the guardrail and can fail the node."
- Repo assets are canonical. Sync mechanism: T6 produces the list of every
  modified file under assets/; after merge, each is copied (plain `cp`) to
  its mirrored path under ~/.config/coyote/ (e.g. assets/agents/<name>/
  index.yaml → ~/.config/coyote/agents/<name>/index.yaml). That list is
  recorded as a follow-up item in the PR body. Built-in prompt text in
  src/config/prompts.rs is compiled into the binary and needs no sync.
- config.example.yaml: `max_concurrent_jobs` next to
  compression_threshold/max_tool_result_chars (~lines 200-206) with the
  `0 = disabled` semantics documented.
- CHANGELOG + README/docs section for the new tools.
- **Wiki (user-required 2026-08-24)** — users must be able to discover and
  understand the subsystem. Target: the GitHub wiki
  (github.com/Dark-Alex-17/coyote/wiki — a SEPARATE git repo,
  `coyote.wiki`, OUTSIDE this run's write boundary). Required content, new
  page `Background-Jobs.md`:
  1. What background jobs are (whitelisted tool calls running as detached
     tasks) and when to use them vs. agents (§4 directionality ruling:
     agents can start jobs, never the reverse — jobs run single tool
     calls; agents think);
  2. The five `job__*` tools with a worked example (start a long build →
     keep working → notification arrives → collect);
  3. The backgroundable whitelist + the teaching errors users will see;
  4. Push notifications: the `system_notifications` key and the turn-end
     guardrail, in user-visible terms;
  5. Configuration: global `max_concurrent_jobs`, per-agent override,
     `0` disables, and the function-calling requirement (R8) —
     jobs simply don't appear otherwise;
  6. Behavioral fine print: snapshot semantics, jobs die with the coyote
     process (R5, no persistence), output tail cap + `tail_lines`,
     `job__*` visible in `.info tools` but deliberately not toggleable
     via `.tool` (R10), graph LLM-node guidance (collect before the
     final node turn).
  Cross-links: Home.md feature blurb + README features list entry
  pointing at the new page, and a "jobs vs. subagents" note on the
  existing agents wiki page. Process (macros-run precedent, coyote.wiki
  198f82c): T6 DRAFTS the full page content and cross-link diffs as a
  task artifact (task log.md); actual publication to coyote.wiki is
  recorded in the PR body's Follow-up section and executed AFTER merge —
  the wiki must only ever describe merged behavior, and the wiki repo is
  outside the run branch's write target.

## 11. Implementation sketch (task-shaped)

0. **T0 — characterization tests (§9.3).** Pin current behavior for every
   coverage gap. Merges FIRST; no production code changes.
1. **T1 — TaskHandle enum + Supervisor generalization.** `TaskHandle`,
   `HashMap<String, TaskHandle>`, per-kind capacity (R7 resolution:
   agent-override → global → 5; 0 disables), namespaced ids, JobState cell
   + kill discipline (§6), `cancel_recursive`/`cancel_all` Job-variant
   handling, Drop-kill (pgid-guarded), cross-kind teaching errors,
   supervisor init condition (R9), `use_agent` gains `cancel_recursive()`
   on the old supervisor before replacement (rc.rs:4155 — §6 reset-site
   fix), explicit kill-all on the REPL quit path (§6 shutdown). Config
   plumbing: global Config field + AgentConfig Option + all four AppConfig
   touch points. T0 stays green.
2. **T2 — Deliberate deltas (THREE separate commits — §9.1 isolation).**
   Commit (a): guardrail predicate fix — `pending_task_ids` incl.
   finished-but-uncollected, kind-aware prompt, ForceTerminate
   discard-with-warning (H7). Commit (b): align `agent__check` to
   pure-probe semantics (R6, supervisor.rs:844) with tool-description/
   prompt updates. Commit (c): `truncate_if_needed` UTF-8-boundary fix.
   Each commit rewrites its own T0-marked tests — explicitly, in that
   commit. Independently shippable; fixes the agent output-abandonment
   bug even without jobs.
3. **T3 — job runner + `job__*` handlers + conditional injection (R8/R9).**
   Extracted process runner (`JobEnvSnapshot`, process_group(0) per §6
   platform strategy, stdin=null, ring buffer, LLM_OUTPUT read post-wait()),
   `JobCtx` for MCP invokes, the five handlers (lock discipline per §6),
   whitelist + teaching errors, sync pre-execution gates in `job__start`,
   context-availability validation (R11) against the per-request
   declared-names stash captured in `before_chat_completion` (rc.rs:896;
   all seven turn-loop call sites — §3 hardening rule 1, Oracle B1),
   single-entry JobCtx McpRuntime (§3 rule 4, Oracle N3), graph-LLM-node
   lifecycle semantics (§12, Oracle B3), REPL-surface wiring (R10:
   `job__` in `concrete_tool_names()` exclusion; `.info tools` verified
   free),
   declaration/prompt gating at all injection sites (agent init,
   plain-session Functions::init sites, exit_agent rebuild,
   select_enabled_functions carve-outs). Per the §13.2 audit: background
   runner on `tokio::process` (foreground stays sync-std, untouched);
   capture-only, no live tee; `COYOTE_TOOL_TIMEOUT` resolved at start and
   enforced with a pgid-guarded group kill; env-derived bin dirs +
   vault-interpolated agent envs frozen into the snapshot at start.
   Includes the tail-biased result cap + `tail_lines` param (§3, char-
   boundary floored). PLUS (Oracle finding 8): an explicit audit item —
   enumerate every `ctx.supervisor`/`parent_supervisor` consumer
   (guardrail, taskqueue/mailbox handlers, REPL displays, session
   save/load) and verify each behaves correctly with an agent-capacity-0
   supervisor (the novel R9 default-on state in plain sessions).
4. **T4 — NotificationQueue + merge_system_channel.** Per-ctx queue,
   producers (jobs, then the agent spawn wrapper), single-pass merger
   refactor replacing `inject_escalation_notification` (byte-identical
   output when notifications are empty — §9.2.6), stale suppression,
   no-notify-on-cancel, panic-skip semantics per §7. Wire drain into
   `eval_tool_calls` at any depth. The agent-producer (R4 — a §9.1 delta)
   is its own commit within T4.
5. **T5 — loop-tracker exemptions + no-change check hint.**
6. **T6 — prompts/docs/CHANGELOG/config.example.yaml + config sync (§10),
   including the README features-list entry and the DRAFT of the
   `Background-Jobs.md` wiki page + cross-link diffs (§10 wiki bullet;
   publication to the separate coyote.wiki repo is a post-merge follow-up
   recorded in the PR body, never done on the run branch).**
7. **T7 — feature tests**: §9.2 invariants (0 vs >0 states), double-wrap
   regression (escalation+notification same batch, non-object output),
   stale-notification suppression, orphan process-group kill (incl.
   grandchild, Unix), pgid-guard (no kill after normal collect),
   LLM_OUTPUT-vs-ring-buffer split, JoinError→failed mapping, guardrail
   enumeration of finished handles, tracker exemption masking test
   (`[check, X, check, X]` still detects X), cross-kind id errors,
   use_agent/exit_agent kills jobs, jobs-only supervisor (agent capacity 0)
   rejects agent__spawn with today's capacity error, tail-cap char-boundary
   tests. PLUS R10/R11 surface tests: `concrete_tool_names()` excludes
   `job__`; extended infra-preservation test (job__* survive agent/role
   `enabled_tools` filters, incl. the empty-list case); `job__start`
   rejects a context-filtered tool and accepts an in-filter one; toggle
   of `job__start` errors as unknown. PLUS §3 validation-hardening tests:
   `job__start {tool: "bash"}` / a path-shaped name / a PATH-resolvable
   binary that is not a declared tool → rejected with NO process spawned
   (assert the runner is never invoked); jobs-disabled context +
   hallucinated `job__start` → eval's existing "Unexpected call"
   soft-fail (B2 option (b) ruling; mod.rs:2564 stays green);
   `job__start {tool: "mcp_invoke_X"}` for
   a started-but-not-context-enabled server X → R11 rejection; subagent
   `job__check/collect/cancel` cannot reach a parent-supervisor job id
   (isolation → unknown-id error); declared+enabled but NOT whitelisted
   (e.g. `memory__write`, `fs_read`, and the directionality cases
   `agent__spawn`/`user__select` — §4 ruling) → whitelist teaching error,
   no spawn
   (Oracle N1 — rule 2's owning test); `mapping_tools` alias name →
   rejected (concrete names only, rule 1); stash freshness — stash
   refreshed per request, mid-batch `skill__load` does not grant until
   next request (B1); None→Some flip pinned both states + jobs-off guard
   for the rc.rs:5493 None-test (N2); `.info tools` positive assertion
   automated via the tools_info unit tests (rc.rs:6025-6079, N5);
   graph-node job lifecycle: node-started job registers in the shared
   ctx.supervisor, survives node completion, notification drains on a
   later turn of the same ctx; guardrail iteration-burn characterized
   (B3).

Dependency order & worktree parallelization (Oracle-confirmed):
T0 → T1 (strictly sequential; T0 is the safety net) → **T2 ∥ T3** (disjoint
file sets EXCEPT function/mod.rs — T2 commit (c) touches :404-413 while T3
touches the :1420 routing region: non-overlapping hunks, rebase-safe, not
conflict-free; otherwise T2 = function/supervisor.rs, T3 = function/jobs.rs
+ agent.rs/rc.rs gating + config) → T4 (after BOTH — touches
mod.rs's merger and supervisor.rs's spawn wrapper) → **T5 ∥ T6** → T7 last
(may overlap T6 only).

## 12. Risks

- **Prompt-budget creep**: `system_notifications` + guardrail enumeration +
  `pending_escalations` can stack; keep entries one-line terse.
- **Snapshot drift**: JobEnvSnapshot must capture exactly what the foreground
  runner reads, or background runs behave differently — enumerate the field
  set during T3 with a test comparing foreground/background env of the same
  tool.
- **AppConfig four-touch-point trap** (R7): struct, Default, From<Config>,
  env override — missing one silently pins the default.
- **Sandbox mode**: RESOLVED (§13.5) — no risk. Sandboxed coyote runs INSIDE
  the container (kit entrypoint IS the coyote binary; `sbx run` attach at
  sandbox/mod.rs:1093-1100); tools are always local children of coyote;
  src/function/ has ZERO sandbox awareness. killpg works identically in and
  out of sbx. (Pre-existing caveat, unchanged by this design: the foreground
  timeout path uses single-PID `child.kill()`, so grandchildren can survive
  a foreground timeout — background jobs fix this for themselves via
  process groups on Unix.)
- **MCP server lifecycle vs running jobs**: a job's `Arc<ConnectedServer>`
  keeps the old service instance alive across a registry restart/shutdown
  mid-job — the job finishes against the OLD server. Acceptable v1;
  document in the tool description.
- **Hard-exit orphans**: `process::exit`/panic paths skip destructors and
  jobs are in their own process group — accepted v1 (§6 shutdown
  mechanism); normal REPL quit kills all.
- **ACP/graph surfaces** — CORRECTED + RULED (Oracle B3, 2026-08-24): the
  old "graph mode out of scope" claim conflated two node kinds. The
  supervisor bypass (`run_agent_for_graph`, supervisor.rs:510-596) means
  the jobs/notifications exclusion applies ONLY
  to graph AGENT nodes (the `run_agent_for_graph` path). Graph **LLM
  nodes** are IN SCOPE — consistent with R11's user-confirmed text: they
  are one of the §2 turn-loop sites, run on the parent session's
  `&mut RequestContext`, and already call the guardrail (graph/llm.rs:271).
  Semantics (owned by T3, tested in T7): a node-started job registers in
  the SHARED ctx.supervisor and OUTLIVES the node run — mechanically
  coherent: notifications drain into later turns on the same ctx, and the
  turn-end guardrail surfaces still-running handles. Iteration-burn
  hazard: the guardrail Inject arm inside the node's run_chat_loop
  consumes node `max_iterations` and bails the WHOLE node at the limit
  (graph/llm.rs:281-291) — a node that backgrounds a long job with
  nothing else to do converts success into an error. Mitigation is
  prompt guidance (§10: collect/cancel before the final node turn), NOT
  a mechanical carve-out in v1.

## 13. Open questions / VERIFY — ALL RESOLVED

1. **JobCtx vs full RequestContext clone** — RESOLVED 2026-08-21 (audit):
   purpose-built `JobCtx { mcp_runtime: McpRuntime, current_depth: usize }`
   WINS decisively. eval_mcp's complete transitive ctx surface is exactly
   `ctx.tool_scope.mcp_runtime` (+ `current_depth` for print gating) —
   the single partition + concurrent join in `eval_tool_calls`
   (mod.rs:291-326) and the five `eval_mcp` arms (mod.rs:1367-1418);
   `McpRuntime::invoke` is self-contained and returns the raw
   `CallToolResult` (tool_scope.rs:289-304). Post-invoke, the foreground
   routes that result through `render_tool_result` (mod.rs:1989, called at
   :1635) — a FREE function (spill under `paths::cache_dir()`/mcp-resources,
   `TEXT_MAX_BYTES_CLAMP` paging; zero ctx) — and the job path calls the
   SAME function for parity, so the audit conclusion is UNCHANGED after the
   2026-08-25 MCP resources/prompts merge. RequestContext has NO Clone impl
   at all; the nearest equivalent `fork_for_branch` (rc.rs:433-463)
   deep-copies the ENTIRE session transcript (Vec<Message> ×2) + Functions
   and would drag parking_lot supervisor locks into the detached task.
   OAuth needs nothing from ctx: per-request bearer injection + 401
   force-refresh-retry-once live INSIDE the transport (`McpOAuthClient`,
   auth_client.rs:42-99) with a process-global token store and per-server
   single-flight locks — mid-job refresh works through JobCtx unchanged.
2. **JobEnvSnapshot field set** — RESOLVED 2026-08-21 (audit): exact field
   table + runner-mechanics rulings in §4. Notable: NO pre-execution gates
   exist (H3 is future-proofing); the external eval branch never uses
   `&mut ctx` (clean extraction); foreground runner is sync-std polled from
   async (background uses tokio::process; foreground untouched v1);
   live-tee threads must not run for jobs; vault-interpolated agent envs
   resolve into the snapshot at start (same exposure as foreground).
3. **`agent__check` consume-on-finished quirk** — RESOLVED 2026-08-21
   (user, R6): align — check never consumes, for agents AND jobs;
   deliberate behavior change, tool descriptions/prompts updated in T2/T6.
4. **Defaults & knob placement** — RESOLVED 2026-08-21 (user, R7 amended):
   `max_concurrent_jobs` = global Config field (default 5, `0 = disabled`)
   + Option<usize> AgentConfig override, resolved agent-first
   (max_tool_result_chars pattern); ring buffer 64 KiB, no-change hint
   threshold 5, SIGTERM grace 5s stand. Role/session-runtime overrides
   deferred for both concurrency knobs together.
5. **Sandbox (sbx) execute_command** — RESOLVED 2026-08-21 (audit): clean
   answer — sandbox mode does NOT touch tool execution. Coyote itself runs
   inside the container (kit entrypoint = coyote; the only `sbx exec` calls
   are launch-time setup, sandbox/mod.rs:1051-1128); tools are plain local
   `std::process` children everywhere; `grep sandbox src/function/` = zero
   matches. killpg is fully viable in sbx; no whitelist carve-out needed.
6. **Huge job outputs at collect** — RESOLVED 2026-08-21 (audit): plain
   `max_tool_result_chars` truncation is INADEQUATE (default null = no cap;
   head-keeping = wrong end for logs; UTF-8 boundary bug — §9.1). Ruling:
   tail-biased job-result cap (default keep last 50,000 chars, explicit
   header, char-boundary floored) + optional `tail_lines` param on
   `job__collect` (§3); no LLM summarization v1.
7. **Whitelist boundary for custom tools** — RESOLVED 2026-08-21 (user):
   ALL external command tools are backgroundable — bash (argc),
   JavaScript/TypeScript, and Python — no per-tool opt-in flag; they are
   process-isolated by construction.
8. **Feature-off gating** — RESOLVED 2026-08-21 (user, R8/R9): effective
   `max_concurrent_jobs == 0` → job__* declarations and prompt text not
   injected (can_spawn_agents-style gating); jobs available in plain
   sessions via the global value (R9).
