# Design: Macros as First-Class Custom Commands

Status: DRAFT v2 — grounded in code (src/config/macros.rs, src/repl/mod.rs,
src/config/install_remote.rs, src/config/paths.rs). Supersedes v1, which
proposed a new markdown "commands" artifact before discovering macros already
cover ~80% of the feature.

## 0. Decision record

- **Naming: keep "macros"** (user decision). No rename, no new artifact type,
  no new install filter — `--install-from <url> --filter macros` already
  exists in both the CLI enum (`InstallFilter`, config/mod.rs:403) and the
  REPL parser (`install_remote_from_repl_args`). (Note: the flag may be
  renamed to `--install` per plans/bundle-manifest-design.md §5; the filter
  is unaffected.) Docs will state plainly: *macros are
  coyote's custom commands* (README + config examples + `.help`).
  "Macro" is also the more accurate term: these are replayable scripts of
  REPL commands with variables, not just prompt templates.
- **Keep the `.macro` subcommand** alongside top-level invocation (user
  decision): it hosts the interactive creator (which top-level must never
  trigger), it is the escape hatch for macros shadowed by built-ins (incl.
  future built-ins landing on existing macro names), and removing it breaks
  muscle memory and macros whose steps invoke `.macro`. Cost of keeping: ~0.
- **Rejected**: separate markdown command files (redundant — a single-step
  macro with one `rest` variable IS a prompt command, and macros additionally
  do multi-step, role switching, and `` .file `cmd` `` shell embedding);
  rename to "commands" (churn, dir migration, and the less accurate word);
  an `alias` field (complexity without payoff — the file name is the command
  name, matching the existing naming structure); command-side agent/role
  binding (inverts contexts-curate-artifacts; lets installed bundles mutate
  existing contexts).
- **Bundle manifest / provenance tracking is NOT in this plan.** It is an
  orthogonal, separate design (see §10); this feature neither depends on nor
  blocks it.

## 1. What exists today (verified)

- `Macro { variables: Vec<MacroVariable{name, rest, default}>, steps:
  Vec<String> }`, YAML at `macros_dir()/<name>.yaml` (global only; no
  workspace dir, unlike skills).
- `.macro <name> [args]` executes; nonexistent name + no args opens the
  interactive macro creator (`ctx.new_macro`).
- `macro_execute` forks a fresh `RequestContext` from the current role
  (inherits role's model/temperature/enabled_tools/enabled_mcp_servers),
  copies `last_message` in (discontinuous), sets `macro_flag`, runs each
  `{{var}}`-interpolated step via `run_repl_command`. **No state flows back**:
  the macro's exchanges are not recorded in the active session and do not
  update the caller's `last_message`.
- Auto-generated usage strings; positional vars with defaults + rest-capture.
- `.list macros`, `.delete macro`, built-ins embedded from `assets/macros/`.
- Unknown dot-command → `unknown_command()` at repl/mod.rs:1521:
  `Error: Unknown command. Type ".help" for additional help.`

## 2. Gaps this design closes

1. Top-level invocation: `.review-work args`, not just `.macro review-work args`.
2. Discoverability: `description` field → listings, completion, `.help`.
3. Context scoping: `enabled_macros` on role / agent (non-graph) / session /
   global config, mirroring `enabled_skills`.
4. Runtime toggles: `.macro enable|disable <name>`, shorthand for
   `.set enabled_macros` (see §6).
5. Conversation-integrated macros: `isolated: false`.
6. Workspace-local macros: `.coyote/macros/` shadowing global, mirroring
   workspace skills/MCP conventions (see §5).

## 3. New Macro fields

```yaml
description: Review WIP against a base branch   # optional; shown in listings/completion/.help
isolated: false                                 # optional; default TRUE (current behavior)
variables:
  - name: base
    default: main
  - name: instructions
    rest: true
    default: ""
steps:
  - "Review the diff against {{base}}. {{instructions}}"
```

Both new fields are `#[serde(default)]`-style optional → every existing
macro file remains valid; default `isolated: true` preserves current behavior
exactly.

### `isolated` semantics

- `true` (default, today's behavior): forked context as described in §1.
  Right for utility macros (e.g. generate-commit-message) whose chatter
  should not pollute the session.
- `false`: steps run via `run_repl_command(ctx, ...)` against the **live**
  context — exactly as if the user had typed each step at the prompt
  themselves. Prompts continue the actual conversation: recorded in the
  active session, `last_message` updated, agent context preserved.
  Consequence to document loudly: mutating steps (`.role x`, `.model y`)
  **persist after the macro ends** — that is the meaning of non-isolation,
  not a bug.
- **VERIFIED: the session-recording chain has no macro_flag check anywhere**
  (`after_chat_completion` request_context.rs:1528-1544 → `save_message`
  :1464-1474 → `Session::add_message` session.rs:724-763; disk save deferred
  to `Session::exit` :626-631). Today's "macros aren't recorded" behavior
  comes ENTIRELY from the fork (`session: None` in the fresh context), not
  from the flag — so non-isolated execution records normally with zero
  changes to the recording path.
- Footnotes to today's isolation (documented, not changed): (a) the forked
  ctx still appends exchanges to the flat `messages.md` file via the
  `save_message` fallthrough (request_context.rs:1518-1526) — only *session*
  recording is skipped; (b) Arc-backed state copied into the fork
  (supervisor, inbox, escalation_queue) is shared, so mutations through those
  handles already reach the parent.
- Both modes: `macro_flag` is set for the duration (temporarily on the live
  ctx for non-isolated, restored on exit **including the error path** — RAII
  guard) so the existing forbidden-op guards (repl/mod.rs:890, 979, 1310)
  apply, and nested `.macro`/top-level macro invocation inside a macro is
  rejected in non-isolated mode (isolated mode already recurses safely via
  `#[async_recursion]`; keep as-is).
  Precise predicate (oracle note): reject when `macro_flag` is set AND the
  CURRENT execution mode is non-isolated. An **isolated** macro's step
  invoking a **non-isolated** macro runs it inline on the FORKED ctx
  (harmless — the fork has no session); pin this behavior in the step-5
  test matrix.
  Implementation footguns (oracle): (a) the RAII guard cannot hold
  `&mut ctx.macro_flag` while `&mut ctx` is passed to `run_repl_command` —
  wrap the whole `&mut RequestContext` (Drop restores prior mode) or use
  save/restore around a closure; (b) prefer a COMPANION FIELD for the mode
  over changing `macro_flag` to `Option<MacroMode>` (the enum touches all 13
  sites incl. both fork-propagation sites; the companion field is the
  smaller diff); (c) a separate `macro_execute_inline` fn creates a new
  type-level recursion cycle with `run_repl_command` and needs its own
  `#[async_recursion]`/boxing — a branch inside the already-boxed
  `macro_execute` is free.
  Semantics caveat to document (§8): steps are FAIL-FAST — a mid-macro error
  aborts remaining steps while completed steps' mutations persist (slightly
  stronger than "as if typed", where a human would continue past errors);
  and a `.exit` step's exit signal is swallowed inside macros today (bool
  discarded at macros.rs:67) — unchanged, but say so.
- **VERIFIED guard inventory** (13 `macro_flag` sites: 4 decl/init, 2
  propagation — `fork_for_branch` request_context.rs:276, `new_for_child`
  :315 — and 7 behavioral). Under non-isolated execution:
  - Keep as-is (desirable in a macro even on the live ctx): `.update` bail
    (repl/mod.rs:890), `.edit` bail (:979 — no `$EDITOR` mid-macro),
    blank-line suppression (:1310, cosmetic), `new_role` prompt→bail
    (request_context.rs:2259), `new_macro` prompt→bail (:2360).
  - Non-issue: `apply_prelude` skip (:3966) — on a live REPL ctx the prelude
    already ran and `state()` is non-empty.
  - **RULED (user, 2026-08-20): condition on isolation.** `use_agent`
    (:3743-3749) suppresses the agent's default `agent_session` when
    `macro_flag` is set; under `isolated: false` the suppression is LIFTED —
    a non-isolated `.agent foo` step inherits foo's default session exactly
    as if typed. Mechanism: the RAII guard records the mode (companion field
    or `Option<MacroMode>` replacing the bare bool) so use_agent can
    distinguish isolated from non-isolated; isolated mode keeps today's
    suppression verbatim.
  - Nested-macro *execution* is currently unguarded (`new_macro` only blocks
    the interactive creator) — the non-isolated nesting rejection is NEW
    code, not a reuse of an existing check.

## 4. Top-level invocation

- Dispatch order in the REPL command match: **built-ins first**, then — where
  `unknown_command()` fires today — look up visible macros by name (file
  stem). Hit → execute exactly as `.macro <name> <rest>` would (honoring
  `isolated`). Miss → existing `unknown_command()` error, verbatim, unchanged.
  **VERIFIED insertion point**: the top-level catch-all `_ =>
  unknown_command()?` at repl/mod.rs:1297. The other `unknown_command()`
  call sites (:630, :740, :763, :1236, :1259) are sub-argument mismatches
  inside known commands and must NOT dispatch macros.
- `.macro` subcommand is kept verbatim for back-compat, including the
  interactive-creation flow (creation stays ONLY under `.macro`; a top-level
  typo like `.hi` must error, never open the creator).
- Both entry points (`.name` and `.macro name`) go through the same §5
  visibility check — otherwise `.macro` trivially bypasses `enabled_macros`.
- Collision rules:
  - Macro name colliding with a **built-in** command: built-in always
    wins; macro still invokable via `.macro <name>`; flagged
    `shadowed (built-in)` in `.list macros`. Built-in list sourced from the
    existing `ReplCommand` registry, not a hardcoded copy. **VERIFIED shape**:
    `static REPL_COMMANDS: LazyLock<[ReplCommand; 60]>` (repl/mod.rs:56-330),
    `ReplCommand { name: &'static str, description, state: AssertState }`
    (:554-571), with a test hardcoding the count (:1732). Macros must NOT be
    added to this array (static, `&'static str`, fixed count).
- Tab completion: on `.<TAB>`, visible macros appear alongside the usual
  built-ins, with their `description` shown when available — joining the
  built-in completer — as a SEPARATE dynamic source queried at
  completion time: the completer clones REPL_COMMANDS at construction
  (completer.rs:93-98) but already holds `Arc<RwLock<RequestContext>>`
  (completer.rs:84), so it can query visible macros live.
  **Shadowed macros are excluded from completion entirely** (RULED): a macro
  whose name collides with a built-in is neither dispatchable via `.<name>`
  (built-in always wins) nor listed in `.<TAB>` completions — it surfaces
  only in `.list macros` as `shadowed (built-in)` and stays invokable via
  `.macro <name>`.
- **`.macro <TAB>` argument completion (RULED)**: upgraded to match `.<TAB>`
  presentation — macro names WITH descriptions when available. VERIFIED
  today: request_context.rs:3004 completes `.macro` args via
  `map_completion_values(paths::list_macros())` (names only, no
  descriptions), while the plumbing already supports described suggestions
  (`repl_complete` returns `(String, Option<String>)`; `.model`/`.agent`
  arms use it, rendered at completer.rs:58-60). Change the `.macro` arm to
  the same resolver source as top-level completion. Differences from
  `.<TAB>`: shadowed macros ARE listed here (`.macro` is their escape
  hatch), and the `enable`/`disable` subcommands appear alongside macro
  names. Second-arg completion: `.macro enable <TAB>` / `.macro disable
  <TAB>` complete toggle-eligible macro names.
- Completion entries for macros carry the same `AssertState` stance as the
  `.macro` built-in (the completer filters on `cmd.is_valid(state)`,
  completer.rs:46).
- `enable`/`disable` are sub-args of the existing `.macro` entry, NOT new
  `REPL_COMMANDS` array entries (the count-asserting test at repl/mod.rs:1732
  stays untouched).
  `.help` gains a "custom commands (macros)" section and
  a line stating macros = custom commands.

## 5. Scoping: `enabled_macros`

New optional field, mirroring `enabled_skills` semantics **verbatim** — same
parser (`parse_string_or_array`: YAML list or comma-separated string), same
null/absent/empty behavior. **VERIFIED semantics** (resolver:
`SkillPolicy::effective_with`, skill_policy.rs:40-132; precedence :78-82;
regression test :388-404 pinning CHANGELOG:320):

**Scope of "mirror" (explicit):** the mirroring covers ONLY the allowlist
resolution semantics (None/empty/populated meanings, first-`Some`-wins
precedence) and the config plumbing (where the field lives, how each level
parses it). It does NOT copy any LLM-facing skills machinery: skills feed
instruction injection and tool-scope refresh into the model payload —
macros have no analog of any of that. The LLM never learns macros exist or
ran (an isolated macro's exchanges arrive as ordinary messages on a fork; a
non-isolated macro's steps are indistinguishable from typed input).
`enabled_macros` gets its own small resolver over the discovered-files set —
it is NOT wired into `SkillPolicy`, prompt building, or context startup
(see "Lazy resolution" below).

- `None`/absent = "no opinion" → fall through to the next level; all-`None`
  → everything visible.
- `Some([])` (empty list, incl. empty string via the parsers) = **explicit
  ZERO** — nothing enabled. Empty ≠ all; that was the regression.
- Populated = exactly those names. **Deliberate DIVERGENCE from skills on
  unknown names**: skills hard-bail the whole resolution
  (skill_policy.rs:94-103 — there is no warn path). For macros, hard
  validation happens ONLY at `.set enabled_macros` time (the
  request_context.rs:2709-2727 pattern: bail on a name that exists in
  neither workspace nor global macros dir); CONFIG-FILE lists are validated
  gracefully at resolution time — warn + `missing` row in `.list macros`,
  never a bail (a stale name in a role file must not brick that role).
  There is no `visible_macros` concept in v1.
- Precedence: `.or_else()` chain — session → agent → role → global, **first
  `Some` wins outright**, no merging. Matches the ruling below.
- Plumbing gotcha: the global level is TWO structs (`Config` mod.rs:222 AND
  `AppConfig` app_config.rs:44/:212) plus an env-override arm
  (app_config.rs:532); role parses via frontmatter `parse_string_or_array`
  (role.rs:131), session via plain serde — three parse paths to mirror.

- Global config (`config.example.yaml`, alongside `enabled_skills`): default
  when no role/agent/session is active. Absent/null = all macros visible.
- Role (`config.role.example.md` frontmatter), agent config
  (`config.agent.example.yaml`), session config: allowlist for that context.
- **Graph-based agents — CORRECTED BY VERIFICATION, then RULED (user,
  2026-08-20): silently ignored, option (a).**
  The prior ruling ("graph configs reject the field") is not implementable
  as specified: coyote uses `deny_unknown_fields` nowhere except
  mcp/mod.rs:79, so unknown fields in graph.yaml are silently ignored — and
  `enabled_skills` is in fact SUPPORTED at graph level (`AgentConfig::
  from_graph` copies it at agent.rs:811; graph/llm.rs:195-226 swaps per-node
  values with save/restore; validator enforces node⊆graph at
  graph/validator.rs:1156,:1235). Ruling: `enabled_macros` is simply omitted
  from the `Graph` struct — graph.yaml ignores it like any other unknown
  field (zero code, consistent behavior); the omission is documented in the
  wiki/agent docs. Full skills-style graph support was rejected as
  meaningless (graph nodes never dispatch REPL commands); a bespoke
  validator warning was considered and declined.
- Precedence: most-specific active context that defines the field wins —
  session > agent > role > global. No merging/intersection.
- Allowlist entries are macro **names** (the file stem — the same identifier
  used for invocation).

### Workspace-local macros (RULED: in scope)

Workspace macro definitions are allowed, mirroring the existing
workspace-artifact conventions (VERIFIED mechanics):

- Discovery: `.coyote/macros/*.yaml` under `workspace_config_dir()`
  (paths.rs:198-207 — CWD only, no ancestor walk-up, dir name overridable
  via `COYOTE_WORKSPACE_CONFIG_DIR`), exactly like workspace skills
  (`workspace_skills_dir()`, paths.rs:209-215).
- Collision rule: **workspace shadows global by name** — same as workspace
  skills (`list_skills` iterates [workspace, global] with shadowing,
  paths.rs:478-501; `has_skill` checks workspace first, :503-505) and
  workspace MCP (HashMap insert, workspace wins, mcp/mod.rs:239).
- Opt-out mirrors MCP: new `--no-workspace-macros` CLI flag +
  `no_workspace_macros` config key (default false), modeled on
  `--no-workspace-mcp` (cli/mod.rs:96-98 → main.rs:222-223,
  app_config.rs:98).
- Trust model: no confirmation prompt — consistent with workspace MCP and
  skills, which load with no gate (mcp/mod.rs:225-268 merely eprintlns);
  macros are strictly lower-risk since they run only on explicit user
  invocation, never automatically. `.list macros` gains a source column
  (`workspace` | `global`) so provenance is always visible.
- `enabled_macros` existence validation consults workspace-then-global (the
  `has_skill` pattern).
- The interactive creator (`.macro <new-name>`) continues to write to the
  GLOBAL macros dir; workspace macros are authored by hand / committed to
  the repo. Remote installs (`--filter macros`) also target global only —
  bundles never write into a workspace.
- Agent plumbing (VERIFIED): `AgentConfig` agent.rs:694-764 (getter/setter
  pattern :399-412), reached lazily from `RequestContext.agent`
  (request_context.rs:147, set in `use_agent` :3670+). Policy is resolved
  lazily at enforcement sites from `ctx.role/agent/session` — no
  activation-time snapshot exists, which confirms the lazy-resolution ruling
  fits the existing architecture exactly.
- Session-level note: `enabled_skills` has NO REPL setter today (session file
  serde only). Runtime toggles do NOT touch session state — `.macro
  enable|disable` edits the global-level in-memory list via
  `update_app_config` (see §6), reusing the `.set` machinery.
- Unknown names in a config-file list: warning + `missing` row in `.list
  macros` (per the divergence ruling above — resolution never bails).

### Lazy resolution

`enabled_macros` never touches the LLM payload (macros are invisible to the
model), so the visible set is computed on demand — at top-level dispatch
fallback, completion, `.list macros`, `.macro enable|disable` — from:
discovered files x active context's allowlist x runtime toggles. Zero
context-startup work; no new startup-ordering surface. Rescan both macro
dirs (workspace + global) on each resolution — two `read_dir`s; matches
`paths::list_macros()` cost today.

## 6. REPL management surface

- `.list macros` (enriched): name, description, isolated?, state:
  `enabled` | `disabled (runtime)` | `locked` | `missing` | `shadowed
  (built-in)` | `invalid`. `locked` names the restricting config (e.g.
  `agent:oracle enabled_macros`). Plus a source column
  (`workspace` | `global`).
- **`.macro enable <name>` / `.macro disable <name>`** (RULED — replaces the
  earlier `.command` proposal; user prefers no separate command at the cost
  of reserving two names): shorthand for editing `.set enabled_macros`.
  Consequently **`enable` and `disable` are reserved macro names**: the
  creator rejects them, discovery marks such a file `invalid` (with a
  warning), and remote installs warn.
- **`.set enabled_macros <csv|null>`** — new key in the `.set` match
  (`RequestContext::update`, request_context.rs:2644-2929), mirroring
  `enabled_skills` (:2706-2730) verbatim: `csv_to_vec` parsing (space-free,
  comma-separated), `null` clears back to None, per-name existence
  validation (workspace-then-global), then
  `update_app_config(|app| app.enabled_macros = ...)`.
- **Mechanism makes the no-override rule structural** (VERIFIED): `.set`
  writes the in-memory GLOBAL AppConfig level — the LOWEST-precedence rung
  of the session > agent > role > global chain — in-memory only, never
  persisted (`update_app_config` clone-and-swaps the Arc,
  request_context.rs:347-354). So `.macro enable|disable` physically cannot
  override a role/agent/session `enabled_macros` allowlist. When such an
  allowlist is active, the toggle ERRORS (naming the owning config) instead
  of silently writing a shadowed value.
- Toggle semantics over the global-level list: `disable X` with list=None
  (all visible) materializes the list as all-discovered-minus-X; `enable X`
  appends if absent (None → already visible → no-op notice); `disable X`
  removes (absent → no-op notice). Lifetime: process runtime, like every
  other `.set` key.
- Drive-by fix bundled here: `enabled_skills` is missing from the `.set`
  key completion list (request_context.rs:3018-3047) although its setter
  works — add both `enabled_skills` and `enabled_macros` to completion.

States per macro per context:
1. permitted + enabled (default within allowlist)
2. permitted + disabled (runtime toggle)
3. locked (outside a ROLE/AGENT/SESSION allowlist) — not enableable from the
   REPL; the error says to edit `enabled_macros` in the owning config. No
   REPL override, ever: those configs stay the single source of truth.

Boundary rule: `locked` applies ONLY to role/agent/session allowlists. A
global-level exclusion (whether from config.yaml or a prior toggle — they
occupy the same in-memory list and are indistinguishable) is always
toggleable via `.macro enable` and displays as `disabled (runtime)`.

### Error handling matrix

| Action | Condition | Behavior |
|---|---|---|
| `.name` | no built-in, no macro | existing error verbatim: `Error: Unknown command. Type ".help" for additional help.` |
| `.name` / `.macro name` | locked | error naming the restricting context/config |
| `.name` / `.macro name` | runtime-disabled | error: re-enable with `.macro enable <name>` |
| `.macro enable X` | locked (more-specific allowlist active) | error: "restricted by <config>; edit `enabled_macros` there" |
| `.macro enable X` | unknown | existing unknown-style error |
| `.macro enable X` | already enabled | no-op notice |
| `.macro create/creator` | name is `enable` or `disable` | error: reserved name |
| discovery | file named `enable`/`disable`.yaml | `invalid` in `.list macros` + warning |
| config `enabled_macros` | unknown name | warning + `missing` in `.list macros` |
| discovery | name shadows built-in | works via `.macro` only; `shadowed` in `.list macros` |
| discovery | workspace + global same name | workspace shadows global; both visible in `.list macros` source column |
| non-isolated macro | step invokes another macro | error: nested macros not allowed in non-isolated mode |

## 7. Install & distribution

Unchanged: `--install-from --filter macros` (CLI + REPL; flag rename to
`--install` tracked in plans/bundle-manifest-design.md §5). Installing macros
never modifies any `enabled_macros` list — a context with an allowlist is
unaffected by new installs until the user edits it (the security property
motivating context-side scoping). Existing overwrite/skip behavior applies;
install-time warning if an installed macro's name shadows a built-in.

## 8. Docs

- README + `.help`: "macros are coyote's custom commands" framing; top-level
  invocation; `isolated` semantics with the role-switch-persists warning.
- `config.example.yaml`, `config.role.example.md`,
  `config.agent.example.yaml`: `enabled_macros` entries mirroring the
  existing `enabled_skills` doc comments; `no_workspace_macros` entry
  alongside the existing `no_workspace_mcp` one (config.example.yaml:140-145).
- New `macro.example.yaml` (or extend existing docs) showing all fields incl.
  description/isolated.
- Workspace macros: document `.coyote/macros/` alongside the existing
  workspace skills/MCP conventions.

## 9. Implementation sketch

1. **Macro struct**: add `description`, `isolated` (serde defaults);
   deser tests for back-compat with field-less YAML.
2. **`enabled_macros` field**: global AppConfig + role + agent (non-graph) +
   session structs (both `Config` AND `AppConfig` at the global level + env
   arm), via `parse_string_or_array` like role.rs:131; graph.yaml silently
   ignores it (field omitted from the Graph struct, per §5 ruling);
   precedence resolver + tests (incl. the empty-list-means-zero regression
   case).
3. **Resolver**: discovered x allowlist x runtime toggles (global-level
   in-memory) →
   visible set + per-macro state; table-driven tests over the §6 matrix.
   OWNS the two-dir discovery (`workspace_macros_dir()` + shadowing, the
   has_skill/list_skills pattern) so steps 4/5/6 are genuinely independent.
4. **REPL dispatch**: macro fallback immediately before `unknown_command()`;
   `.macro enable|disable` + `.set enabled_macros` key (+ completion
   drive-by); enriched `.list macros`; dynamic macro completion with
   shadowed-name exclusion; `.macro <TAB>` arg completion upgraded to
   descriptions + subcommands (request_context.rs:3004); `.help`. Enforce
   visibility on the `.macro` path too.
5. **Non-isolated execution**: `macro_execute_inline(ctx, ...)` variant (or
   branch) running steps on the live ctx with RAII macro_flag guard +
   nested-macro rejection; tests for flag restore on error, plus two
   mock-free session tests (oracle note — no mock-client harness needed):
   (i) the non-isolated path passes the LIVE ctx (session `Some`) into
   `run_repl_command`, not a fork; (ii) a mutating step (`.model x`)
   persists on the live ctx after the macro returns. Also pin
   isolated→non-isolated nesting (§3).
6. **Workspace macros (flag + docs only; discovery lives in step 3)**:
   `--no-workspace-macros` flag + `no_workspace_macros` config key, source
   column in `.list macros`.
7. **Docs** (§8).

Order: 1 → 2 → 3 → {4, 5, 6 in parallel} → 7.

## 10. Follow-ups (out of scope)

- Persisted toggles (surviving restart) — `.set` state is process-lifetime
  by design; persistence would be new machinery for all `.set` keys, not
  just macros.
- Bundle manifest & provenance layer — **separate design doc**, now written:
  `plans/bundle-manifest-design.md` (per-file provenance recording at install
  time, `--list-bundles` / `--update-bundle` / `--uninstall`, optional
  author-shipped `coyote-bundle.yaml` manifest). Once it exists, `.list
  macros` gains a "source bundle" column for free. This plan neither depends
  on nor blocks it.
- "Did you mean" suggestions on unknown command (only if built-ins get it too).

## 11. VERIFY before task materialization

All items resolved 2026-08-20 (findings folded into §3/§4/§5 above):

- [x] `enabled_skills` semantics: None=fall-through, empty=ZERO, populated=
      exact+hard-bail validation; first-`Some`-wins precedence
      (`SkillPolicy::effective_with`). See §5.
- [x] Agent config plumbing: lazy resolution from `ctx.agent`, no snapshot;
      new field = AgentConfig field + accessor + resolver read. See §5.
- [x] `ReplCommand` registry: static fixed array, count-asserting test;
      macros go in a separate dynamic completer source. See §4.
- [x] `macro_flag` guards: 13 sites inventoried; none suppress session
      recording; one OPEN DECISION (`use_agent` agent_session suppression)
      + nested-rejection is new code. See §3.
- [x] Session recording: `after_chat_completion` → `save_message` →
      `Session::add_message`, no macro conditions; isolation lives in the
      fork's `session: None`. See §3.

Both open decisions ruled by the user 2026-08-20: §3 `use_agent` suppression
is conditioned on isolation (lifted for isolated:false); §5 graph.yaml
silently ignores `enabled_macros`. No open questions remain.

Tooling warning for implementers: `fs_grep`/plain grep tools silently skip
src/config/request_context.rs (file-size exclusion) — audit that file with
ast_grep or targeted reads only.

### Second verification round (2026-08-20, after user review)

- [x] `.set` mechanics: fixed hand-written key match in
      `RequestContext::update` (request_context.rs:2644-2929);
      `enabled_skills` IS settable (:2706-2730) and always writes the
      in-memory global AppConfig via `update_app_config` (:347-354) —
      lowest precedence, never persisted, no role/agent/session setter.
      Values: `csv_to_vec` comma lists (space-free; whitespace rejected for
      all but two keys, :2654-2662), `null` clears. `enabled_skills` missing
      from `.set` completion (:3018-3047) — oversight, fixed as drive-by.
- [x] Workspace discovery precedents: MCP = CWD-only probe
      `.coyote/mcp.json` → `.coyote/.mcp.json` → `.mcp.json`
      (paths.rs:217-230), workspace wins collisions via HashMap insert
      (mcp/mod.rs:239), gated by `no_workspace_mcp` only, no trust prompt;
      skills = `.coyote/skills/` shadowing global by name
      (paths.rs:478-505); memory walks ancestors but MCP/skills do NOT.
      Workspace macros copy the skills model + an MCP-style opt-out flag.
