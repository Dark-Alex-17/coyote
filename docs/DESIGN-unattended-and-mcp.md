# Design: unattended mode (`--headless`) and ACP agent server (`--acp-server`)

**Status:** proposed · **Scope:** this repo only · **Motivation:** make Coyote drivable by machines, not just humans — CI pipelines, cron jobs, external orchestrators, and any [Agent Client Protocol](https://agentclientprotocol.com) client (Zed editors, agent fleet managers, test harnesses).

## Feature 1 — `--headless`: never block waiting for a human

### Problem

Coyote assumes a terminal in three places, and an unattended invocation (cron, CI, a supervisor process driving `coyote "<prompt>"`) hangs or fails at each:

1. **User-interaction tools** (`user__select` / `user__confirm` / `user__input` / `user__checkbox`) at `depth == 0` route to live `inquire` prompts (`src/function/user_interaction.rs:139-145`, handlers `:148-210`). No TTY → hang or error. (Depth >0 already escalates to the parent-agent queue — a top-level unattended run IS depth 0.)
2. **The shell execute guard** — already TTY-gated (`src/main.rs:596-657` checks `IS_STDOUT_TERMINAL`) but should be contractual, not incidental.
3. **Rendering** — `markdown_stream()` uses crossterm raw mode (`src/render/stream.rs:17`); unattended runs must take `raw_stream()`/silent deterministically.

### Why a flag instead of TTY detection

Non-TTY stdout does not mean no human: `coyote "explain" | tee out.md` pipes stdout with a fully present user, and `inquire` prompts render on stderr — prompting during piped output is correct today. Only the *launcher* knows nobody is at the keyboard; the flag states it.

### Change

- `src/cli/mod.rs`: `#[arg(long, help_heading = "Sandbox")] pub headless: bool`.
- `src/utils/mod.rs` (beside `IS_STDOUT_TERMINAL:45`): process-wide `HEADLESS: AtomicBool`.
- `src/main.rs` (~`:60-64`, the `--dangerously-skip-permissions` pattern): set `AUTO_CONFIRM=true`, set HEADLESS, force the non-interactive render path. `--headless` with no prompt (REPL mode) is a contradiction → `bail!`.
- `src/function/user_interaction.rs:139-145`: three-way route — headless ⇒ the tool immediately returns structured JSON to the model (`{"needs_human": true, "action": …, "question": …, "options": […], "guidance": …}`); agent configs own what to do with it (report upstream, apply a default, or fail the task). The binary only guarantees "never block."
- **Graph agents are covered for free**: graph user-interaction nodes call the same `handle_user_tool` (`src/graph/user_interaction.rs:4,24,59`) — one graph-path test locks it.

~50 LOC + tests. Composes three existing mechanisms (AUTO_CONFIRM env, IS_STDOUT_TERMINAL gating, the depth router).

## Feature 2 — `--acp-server`: Coyote as an ACP agent over stdio

### What ACP mode is

A JSON-RPC 2.0 stdio server implementing the ACP **agent** side, so any ACP client (an editor, an orchestrator, a test harness) can create sessions, send prompts, stream progress, relay user-interaction requests, cancel turns, and resume from transcripts. Ground truth for protocol shapes: the ACP spec and Zed's `agent-client-protocol` Rust crate (evaluate for adoption; fallback is ~300 LOC of hand-rolled framing — the protocol surface below is small).

### Method surface (initial)

Inbound: `initialize`, `session/new`, `session/prompt`, `session/load`, `session/cancel`. Outbound: `session/update` (streaming), `session/request_permission`. Anything else → `-32601`. Single session per server process initially (the process-per-agent model our launchers use); a second `session/new` errors.

### Mappings — each reuses machinery this repo already has

| ACP | Implementation |
|---|---|
| `session/new` | fork a `RequestContext` (child-fork pattern: `new_for_child`, `src/function/supervisor.rs:554`); `RenderMode::Silent` (`src/config/request_context.rs:163`) |
| `session/prompt` | one turn: `Input::from_str` → `call_chat_completions_streaming` (`src/client/common.rs:478`) with an `SseHandler` sink (`:485`) emitting `session/update` chunks and tool-call updates; reply `{stopReason: "end_turn"}` after the tool loop settles (mirror the minimal loop from `repl/mod.rs ask():1241` — do not call the REPL fn). `auto_continue` forced OFF in ACP mode: the client drives turns; internal self-continuation would desync stop reasons |
| `session/load` | transcript replay = the `--session` resume path (`src/config/session.rs:19-99`; load `:129-138`) — `messages` + `compressed_messages` are the transcript, tool results included; reconstruct history, never re-execute tools |
| `session/cancel` | `abort_signal.set_ctrlc()` (`src/utils/abort_signal.rs:53`) — already polled by streaming + graph executor; interrupted prompt still replies with the cancelled stop reason |
| `user__*` → `session/request_permission` | fourth arm in the interaction router: send the permission request, await with the EXISTING escalation-timeout discipline (300 s default, `user_interaction.rs:14,238-242`); secret-class prompts (device-code/login-shaped) carry a classification marker so clients can render them without a free-text return channel; headless JSON fallback if the permission call fails |

### stdout purity (the load-bearing constraint)

In ACP mode every byte on stdout must parse as JSON-RPC. Known contamination sources, by inspection: log4rs `ConsoleAppender` defaults to stdout (`src/main.rs:693-694`) — ACP mode parameterizes the existing logger builder to stderr/file; ~20 `print!/println!` sites in `src/main.rs`/`src/client/common.rs` — bypassed via `RenderMode::Silent`, with all ACP stdout writes funneled through one serializer. Locked by a test that drives a full session through a spawned child process and asserts line-by-line parseability. Spinners already write to stderr.

`--acp-server` implies `--headless` semantics.

600-900 LOC incl. tests. Testable end-to-end with a stubbed provider endpoint and an in-repo minimal ACP client harness — no network, no real keys.

## Feature 3 — kit: a headless profile (env-parameterized)

The sandbox kit (`assets/sbx-kit/spec.yaml`) gains a **headless profile** for sandboxes created by external supervisors rather than `coyote --sandbox`:

- Entry: `coyote --headless --agent "${COYOTE_HEADLESS_AGENT}" "${COYOTE_HEADLESS_PROMPT:-$(cat "${COYOTE_PROMPT_FILE}")}"` — the supervisor injects `COYOTE_HEADLESS_AGENT` and either a literal `COYOTE_HEADLESS_PROMPT` or a `COYOTE_PROMPT_FILE` path at sandbox-create time. (If the kit command schema can't express shell substitution, the fallback is `-f "${COYOTE_PROMPT_FILE}"` — file-only input still selects Cmd mode, `src/main.rs:77-82`.)
- Fresh-config semantics: no host-config projection (agents/config arrive via the image or the workspace — workspace discovery already covers `.coyote/mcp.json` etc., `src/config/paths.rs:207-220`).
- Credentials unchanged: the existing `proxy-managed` entries (`assets/sbx-kit/spec.yaml:211-239`).

Parameterizing by env (rather than hardcoding an agent name) means one profile serves any supervisor and this repo carries no supervisor-specific content.

## What already works (verified, no changes)

- Prompt intake: trailing positional + piped stdin (`src/cli/mod.rs:47-48`, `cli.text()` `:264-302`).
- Vault in sandboxes: disabled under `IS_SANDBOX`; credentials via the sbx proxy (`src/vault/mod.rs:97-123`; `src/sandbox/mod.rs:206-248,339-363`).
- Workspace MCP discovery: `.coyote/mcp.json` → `.coyote/.mcp.json` → `.mcp.json` (`src/config/paths.rs:207-220`) — supervisors can drop MCP wiring into a workspace with zero changes here.

## Sequencing & effort

| Step | What | Size |
|---|---|---|
| 1 | `--headless` | ~50 LOC, easy |
| 2 | kit headless profile | YAML, easy |
| 3 | ACP dep evaluation (crate vs hand-rolled) | ½ day |
| 4-7 | `--acp-server` in four increments (skeleton+purity → sessions/prompt → load/cancel → request_permission) | 600-900 LOC, medium |

Every hard sub-problem maps onto machinery built for another feature: session resume, sub-agent escalation, abort signal, SseHandler streaming, silent rendering. The new code is protocol framing plus glue.
