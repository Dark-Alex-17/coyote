# Phase 2 Implementation Plan: Engine + Emitter

## Overview

Phase 1 splits `Config` into `AppState` + `RequestContext`. Phase 2 takes the unified state and introduces the **Engine** — a single core function that replaces CLI's `start_directive()` and REPL's `ask()` — plus an **Emitter trait** that abstracts output away from direct stdout writes. After this phase, CLI and REPL both call `Engine::run()` with different `Emitter` implementations and behave identically to today. The API server in Phase 4 will plug in without touching core logic.

**Estimated effort:** ~1 week
**Risk:** Low-medium. The work is refactoring existing well-tested code paths into a shared shape. Most of the risk is in preserving exact terminal rendering behavior.
**Depends on:** Phase 1 Steps 0–10 complete (`GlobalConfig` eliminated, `RequestContext` wired through all entry points).

---

## Why Phase 2 Exists

Today's CLI and REPL have two near-identical pipelines that diverge in five specific places. The divergences are accidents of history, not intentional design:

1. **Streaming flag handling.** `start_directive` forces non-streaming when extracting code; `ask` never extracts code.
2. **Auto-continuation loop.** `ask` has complex logic for `auto_continue_count`, todo inspection, and continuation prompt injection. `start_directive` has none.
3. **Session compression.** `ask` triggers `maybe_compress_session` and awaits completion; `start_directive` never compresses.
4. **Session autoname.** `ask` calls `maybe_autoname_session` after each turn; `start_directive` doesn't.
5. **Cleanup on exit.** `start_directive` calls `exit_session()` at the end; `ask` lets the REPL loop handle it.

Four of these five divergences are bugs waiting to happen — they mean agents behave differently in CLI vs REPL mode, sessions don't get compressed in CLI even when they should, and auto-continuation is silently unavailable from the CLI. Phase 2 collapses both pipelines into one `Engine::run()` that handles all five behaviors uniformly, with per-request flags to control what's active (e.g., `auto_continue: bool` on `RunRequest`).

The Emitter trait exists to decouple the rendering pipeline from its destination. Today, streaming output is hardcoded to write to the terminal via `crossterm`. An `Emitter` implementation can also feed an axum SSE stream, collect events for a JSON response, or capture everything for a test. The Engine sends semantic events; Emitters decide how to present them.

---

## The Architecture After Phase 2

```
┌─────────┐  ┌─────────┐                 ┌─────────┐
│   CLI   │  │  REPL   │                 │   API   │ (Phase 4)
└────┬────┘  └────┬────┘                 └────┬────┘
     │            │                           │
     ▼            ▼                           ▼
┌──────────────────────────────────────────────────┐
│            Engine::run(ctx, req, emitter)        │
│  ┌────────────────────────────────────────────┐  │
│  │ 1. Apply CoreCommand (if any)              │  │
│  │ 2. Build Input from req                    │  │
│  │ 3. apply_prelude (first turn only)         │  │
│  │ 4. before_chat_completion                  │  │
│  │ 5. Stream or buffered LLM call             │  │
│  │    ├─ emit Started                         │  │
│  │    ├─ emit AssistantDelta (per chunk)      │  │
│  │    ├─ emit ToolCall                        │  │
│  │    ├─ execute tool                         │  │
│  │    ├─ emit ToolResult                      │  │
│  │    └─ loop on tool results                 │  │
│  │ 6. after_chat_completion                   │  │
│  │ 7. maybe_compress_session                  │  │
│  │ 8. maybe_autoname_session                  │  │
│  │ 9. Auto-continuation (if applicable)       │  │
│  │ 10. emit Finished                          │  │
│  └────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────┘
     │            │                           │
     ▼            ▼                           ▼
TerminalEmitter  TerminalEmitter          JsonEmitter / SseEmitter
```

---

## Core Types

### `Engine`

```rust
pub struct Engine {
    pub app: Arc<AppState>,
}

impl Engine {
    pub fn new(app: Arc<AppState>) -> Self { Self { app } }

    pub async fn run(
        &self,
        ctx: &mut RequestContext,
        req: RunRequest,
        emitter: &dyn Emitter,
    ) -> Result<RunOutcome, CoreError>;
}
```

`Engine` is intentionally a thin wrapper around `Arc<AppState>`. All per-turn state lives on `RequestContext`, so the engine itself has no per-call fields. This makes it cheap to clone and makes `Engine::run` trivially testable.

### `RunRequest`

```rust
pub struct RunRequest {
    pub input: Option<UserInput>,
    pub command: Option<CoreCommand>,
    pub options: RunOptions,
}

pub struct UserInput {
    pub text: String,
    pub files: Vec<FileInput>,
    pub media: Vec<MediaInput>,
    pub continuation: Option<ContinuationKind>,
}

pub enum ContinuationKind {
    Continue,
    Regenerate,
}

pub struct RunOptions {
    pub stream: Option<bool>,
    pub extract_code: bool,
    pub auto_continue: bool,
    pub compress_session: bool,
    pub autoname_session: bool,
    pub apply_prelude: bool,
    pub with_embeddings: bool,
    pub cancel: CancellationToken,
}

impl RunOptions {
    pub fn cli() -> Self { /* today's start_directive defaults */ }
    pub fn repl_turn() -> Self { /* today's ask defaults */ }
    pub fn api_oneshot() -> Self { /* API one-shot defaults */ }
    pub fn api_session() -> Self { /* API session defaults */ }
}
```

Two things to notice:

1. **`input` is `Option`.** A `RunRequest` can carry just a `command` (e.g., `.role explain`) with no user text, just an input (a plain prompt), or both (the `.role <name> <text>` form that activates a role and immediately sends a prompt through it). The engine handles all three shapes with one code path.

2. **`RunOptions` is the knob panel that replaces the five divergences.** CLI today has `auto_continue: false, compress_session: false, autoname_session: false`; REPL has all three `true`. Phase 2 exposes these as explicit options with factory constructors for each frontend's conventional defaults. This also means you can now run a CLI one-shot with auto-continuation by constructing `RunOptions::cli()` and flipping `auto_continue = true` — a capability that doesn't exist today.

### `CoreCommand`

```rust
pub enum CoreCommand {
    // State setters
    SetModel(String),
    UsePrompt(String),
    UseRole { name: String, trailing_text: Option<String> },
    UseSession(Option<String>),
    UseAgent { name: String, session: Option<String>, variables: Vec<(String, String)> },
    UseRag(Option<String>),

    // Exit commands
    ExitRole,
    ExitSession,
    ExitRag,
    ExitAgent,

    // State queries
    Info(InfoScope),
    RagSources,

    // Config mutation
    Set { key: String, value: String },

    // Session actions
    CompressSession,
    EmptySession,
    SaveSession { name: Option<String> },
    EditSession,

    // Role actions
    SaveRole { name: Option<String> },
    EditRole,

    // RAG actions
    EditRagDocs,
    RebuildRag,

    // Agent actions
    EditAgentConfig,
    ClearTodo,
    StarterList,
    StarterRun(usize),

    // File input shortcut
    IncludeFiles { paths: Vec<String>, trailing_text: Option<String> },

    // Macro execution
    Macro { name: String, args: Vec<String> },

    // Vault
    VaultAdd(String),
    VaultGet(String),
    VaultUpdate(String),
    VaultDelete(String),
    VaultList,

    // Miscellaneous
    EditConfig,
    Authenticate,
    Delete(DeleteKind),
    Copy,
    Help,
}

pub enum InfoScope {
    System,
    Role,
    Session,
    Rag,
    Agent,
}

pub enum DeleteKind {
    Role(String),
    Session(String),
    Rag(String),
    Macro(String),
    AgentData(String),
}
```

This enum captures all 37 dot-commands identified in the explore. Three categories deserve special attention:

- **LLM-triggering commands** (`UsePrompt`, `UseRole` with trailing_text, `IncludeFiles` with trailing_text, `StarterRun`, `Macro` that contains LLM calls, and the continuation variants `Continue`/`Regenerate` expressed via `UserInput.continuation`) — these don't just mutate state; they produce a full run through the LLM pipeline. The engine treats them as `RunRequest { command: Some(_), input: Some(_), .. }` — command runs first, then input flows through.

- **Asynchronous commands that return immediately** (`EditConfig`, `EditRole`, `EditRagDocs`, `EditAgentConfig`, most `Vault*`, `Delete`) — these are side-effecting but don't produce an LLM interaction. The engine handles them, emits a `Result` event, and returns without invoking the LLM path.

- **Context-dependent commands** (`ClearTodo`, `StarterList`, `StarterRun`, `EditAgentConfig`, etc.) — these require a specific scope (e.g., active agent). The engine validates the precondition before executing and returns a `CoreError::InvalidState { expected: "active agent" }` if the precondition fails.

### `Emitter` trait and `Event` enum

```rust
#[async_trait]
pub trait Emitter: Send + Sync {
    async fn emit(&self, event: Event<'_>) -> Result<(), EmitError>;
}

pub enum Event<'a> {
    // Lifecycle
    Started { request_id: Uuid, session_id: Option<SessionId>, agent: Option<&'a str> },
    Finished { outcome: &'a RunOutcome },

    // Assistant output
    AssistantDelta(&'a str),
    AssistantMessageEnd { full_text: &'a str },

    // Tool calls
    ToolCall { id: &'a str, name: &'a str, args: &'a str },
    ToolResult { id: &'a str, name: &'a str, result: &'a str, is_error: bool },

    // Auto-continuation
    AutoContinueTriggered { count: usize, max: usize, remaining_todos: usize },

    // Session lifecycle signals
    SessionCompressing,
    SessionCompressed { tokens_saved: Option<usize> },
    SessionAutonamed(&'a str),

    // Informational
    Info(&'a str),
    Warning(&'a str),

    // Errors
    Error(&'a CoreError),
}

pub enum EmitError {
    ClientDisconnected,
    WriteFailed(std::io::Error),
}
```

Three implementations ship in Phase 2; two are stubs, one is real:

- **`TerminalEmitter`** (real) — wraps today's `SseHandler` → `markdown_stream`/`raw_stream` path. This is the bulk of Phase 2's work; see "Terminal rendering details" below.
- **`NullEmitter`** (stub, for tests) — drops all events on the floor.
- **`CollectingEmitter`** (stub, for tests and future JSON API) — appends events to a `Vec<OwnedEvent>` for later inspection.

The `JsonEmitter` and `SseEmitter` implementations land in **Phase 4** when the API server comes online.

### `RunOutcome`

```rust
pub struct RunOutcome {
    pub request_id: Uuid,
    pub session_id: Option<SessionId>,
    pub final_message: Option<String>,
    pub tool_call_count: usize,
    pub turns: usize,
    pub compressed: bool,
    pub autonamed: Option<String>,
    pub auto_continued: usize,
}
```

`RunOutcome` is what CLI/REPL ignore but the future API returns as JSON. It records everything the caller might want to know about what happened during the run.

### `CoreError`

```rust
pub enum CoreError {
    InvalidRequest { msg: String },
    InvalidState { expected: String, found: String },
    NotFound { what: String, name: String },
    Cancelled,
    ProviderError { provider: String, msg: String },
    ToolError { tool: String, msg: String },
    EmitterError(EmitError),
    Io(std::io::Error),
    Other(anyhow::Error),
}

impl CoreError {
    pub fn is_retryable(&self) -> bool { /* ... */ }
    pub fn http_status(&self) -> u16 { /* for future API use */ }
    pub fn terminal_message(&self) -> String { /* for TerminalEmitter */ }
}
```

---

## Terminal Rendering Details

The `TerminalEmitter` is the most delicate part of Phase 2 because it has to preserve every pixel of today's REPL/CLI behavior. Here's the mental model:

**Today's flow:**
```
LLM client → mpsc::Sender<SseEvent> → SseHandler → render_stream
                                                      ├─ markdown_stream (if highlight)
                                                      └─ raw_stream (else)
```

Both `markdown_stream` and `raw_stream` write directly to stdout via `crossterm`, managing cursor positions, line clears, and incremental markdown parsing themselves.

**Target flow:**
```
LLM client → mpsc::Sender<SseEvent> → SseHandler → TerminalEmitter::emit(Event::AssistantDelta)
                                                      ├─ (internal) markdown_stream state machine
                                                      └─ (internal) raw_stream state machine
```

The `TerminalEmitter` owns a `RefCell<StreamRenderState>` (or `Mutex` if we need `Send`) that wraps the existing `markdown_stream`/`raw_stream` state. Each `emit(AssistantDelta)` call feeds the chunk into this state machine exactly as `SseHandler`'s receive loop does today. The result is that the exact same crossterm calls happen in the exact same order — we've just moved them behind a trait.

**Things that migrate 1:1 into `TerminalEmitter`:**
- Spinner start/stop on first delta
- Cursor positioning for line reprint during code block growth
- Syntax highlighting invocation via `MarkdownRender`
- Color/dim output for tool call banners
- Final newline + cursor reset on `AssistantMessageEnd`

**Things that the engine handles, not the emitter:**
- Tool call *execution* (still lives in the engine loop)
- Session state mutations (engine calls `before_chat_completion` / `after_chat_completion` on `RequestContext`)
- Auto-continuation decisions (engine inspects agent runtime)
- Compression and autoname decisions (engine)

**Things the emitter decides, not the engine:**
- Whether to suppress ToolCall rendering (sub-agents in today's code suppress their own output; TerminalEmitter respects a `verbose: bool` flag)
- How to format errors (TerminalEmitter uses colored stderr; JsonEmitter will use structured JSON)
- Whether to show a spinner at all (disabled for non-TTY output)

**One gotcha:** today's `SseHandler` itself produces the `mpsc` channel that LLM clients push into. In the new model, `SseHandler` becomes an internal helper inside the engine's streaming path that converts `mpsc::Receiver<SseEvent>` into `Emitter::emit(Event::AssistantDelta(...))` calls. No LLM client code changes — they still push into the same channel type. Only the consumer side of the channel changes.

---

## The Engine::run Pipeline

Here's the full pipeline in pseudocode, annotated with which frontend controls each behavior via `RunOptions`:

```rust
impl Engine {
    pub async fn run(
        &self,
        ctx: &mut RequestContext,
        req: RunRequest,
        emitter: &dyn Emitter,
    ) -> Result<RunOutcome, CoreError> {
        let request_id = Uuid::new_v4();
        let mut outcome = RunOutcome::new(request_id);

        emitter.emit(Event::Started { request_id, session_id: ctx.session_id(), agent: ctx.agent_name() }).await?;

        // 1. Execute command (if any). Commands may be LLM-triggering, mutating, or informational.
        if let Some(command) = req.command {
            self.dispatch_command(ctx, command, emitter, &req.options).await?;
        }

        // 2. Early return if there's no user input (pure command)
        let Some(user_input) = req.input else {
            emitter.emit(Event::Finished { outcome: &outcome }).await?;
            return Ok(outcome);
        };

        // 3. Apply prelude on first turn of a fresh context (CLI/REPL only)
        if req.options.apply_prelude && !ctx.prelude_applied {
            apply_prelude(ctx, &req.options.cancel).await?;
            ctx.prelude_applied = true;
        }

        // 4. Build Input from user_input + ctx
        let input = build_input(ctx, user_input, &req.options).await?;

        // 5. Wait for any in-progress compression to finish (REPL-style block)
        while ctx.is_compressing_session() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // 6. Enter the turn loop
        self.run_turn(ctx, input, &req.options, emitter, &mut outcome).await?;

        // 7. Maybe compress session
        if req.options.compress_session && ctx.session_needs_compression() {
            emitter.emit(Event::SessionCompressing).await?;
            compress_session(ctx).await?;
            outcome.compressed = true;
            emitter.emit(Event::SessionCompressed { tokens_saved: None }).await?;
        }

        // 8. Maybe autoname session
        if req.options.autoname_session {
            if let Some(name) = maybe_autoname_session(ctx).await? {
                outcome.autonamed = Some(name.clone());
                emitter.emit(Event::SessionAutonamed(&name)).await?;
            }
        }

        // 9. Auto-continuation (agents only)
        if req.options.auto_continue {
            if let Some(continuation) = self.check_auto_continue(ctx) {
                emitter.emit(Event::AutoContinueTriggered { .. }).await?;
                outcome.auto_continued += 1;
                // Recursive call with continuation prompt
                let next_req = RunRequest {
                    input: Some(UserInput::from_continuation(continuation)),
                    command: None,
                    options: req.options.clone(),
                };
                return Box::pin(self.run(ctx, next_req, emitter)).await;
            }
        }

        emitter.emit(Event::Finished { outcome: &outcome }).await?;
        Ok(outcome)
    }

    async fn run_turn(
        &self,
        ctx: &mut RequestContext,
        mut input: Input,
        options: &RunOptions,
        emitter: &dyn Emitter,
        outcome: &mut RunOutcome,
    ) -> Result<(), CoreError> {
        loop {
            outcome.turns += 1;

            before_chat_completion(ctx, &input);

            let client = input.create_client(ctx)?;
            let (output, tool_results) = if should_stream(&input, options) {
                stream_chat_completion(ctx, &input, client, emitter, &options.cancel).await?
            } else {
                buffered_chat_completion(ctx, &input, client, options.extract_code, &options.cancel).await?
            };

            after_chat_completion(ctx, &input, &output, &tool_results);
            outcome.tool_call_count += tool_results.len();

            if tool_results.is_empty() {
                outcome.final_message = Some(output);
                return Ok(());
            }

            // Emit each tool call and result
            for result in &tool_results {
                emitter.emit(Event::ToolCall { .. }).await?;
                emitter.emit(Event::ToolResult { .. }).await?;
            }

            // Loop: feed tool results back in
            input = input.merge_tool_results(output, tool_results);
        }
    }
}
```

**Key design decisions in this pipeline:**

1. **Command dispatch happens first.** A `RunRequest` that carries both a command and input runs the command first (mutating `ctx`), then the input flows through the now-updated context. This lets `.role explain "tell me about X"` work as a single atomic operation — the role is activated, then the prompt is sent under the new role.

2. **Tool loop is iterative, not recursive.** Today both `start_directive` and `ask` recursively call themselves after tool results. The new `run_turn` uses a `loop` instead, which is cleaner, avoids stack growth on long tool chains, and makes cancellation handling simpler. Auto-continuation remains recursive because it's a full new turn with a new prompt, not just a tool-result continuation.

3. **Cancellation is checked at every await point.** `options.cancel: CancellationToken` is threaded into every async call. On cancellation, the engine emits `Event::Error(CoreError::Cancelled)` and returns. Today's `AbortSignal` pattern gets wrapped in a `CancellationToken` adapter during the migration.

4. **Session state hooks fire at the same points as today.** `before_chat_completion` and `after_chat_completion` continue to exist on `RequestContext`, called from the same places in the same order. The refactor doesn't change their semantics.

5. **Emitter errors don't abort the run.** If the emitter's output destination disconnects (client closes browser tab), the engine keeps running to completion so session state is correctly persisted, but it stops emitting events. The `EmitError::ClientDisconnected` case is special-cased to swallow subsequent emits. Session save + tool execution still happen.

---

## Migration Strategy

This phase is structured as **extract, unify, rewrite frontends** — similar to Phase 1's facade pattern. The old functions stay in place until the new Engine is proven by tests and manual verification.

### Step 1: Create the core types

Add the new files without wiring them into anything:

- `src/engine/mod.rs` — module root
- `src/engine/engine.rs` — `Engine` struct + `run` method (initially `unimplemented!()`)
- `src/engine/request.rs` — `RunRequest`, `UserInput`, `RunOptions`, `ContinuationKind`, `RunOutcome`
- `src/engine/command.rs` — `CoreCommand` enum + sub-enums
- `src/engine/error.rs` — `CoreError` enum
- `src/engine/emitter.rs` — `Emitter` trait + `Event` enum + `EmitError`
- `src/engine/emitters/mod.rs` — emitter module
- `src/engine/emitters/null.rs` — `NullEmitter` (test stub)
- `src/engine/emitters/collecting.rs` — `CollectingEmitter` (test stub)
- `src/engine/emitters/terminal.rs` — `TerminalEmitter` (initially `unimplemented!()`)

Register `pub mod engine;` in `src/main.rs`. Code compiles but nothing calls it yet.

**Verification:** `cargo check` clean, `cargo test` passes.

### Step 2: Implement `TerminalEmitter` against existing render code

Before wiring the engine, build the `TerminalEmitter` by wrapping today's `SseHandler` + `markdown_stream` + `raw_stream` + `MarkdownRender` + `Spinner` code. Don't change any of those modules — just construct a `TerminalEmitter` that holds the state they need and forwards `emit(Event::AssistantDelta(...))` into them.

```rust
pub struct TerminalEmitter {
    render_state: Mutex<StreamRenderState>,
    options: TerminalEmitterOptions,
}

pub struct TerminalEmitterOptions {
    pub highlight: bool,
    pub theme: Option<String>,
    pub verbose_tool_calls: bool,
    pub show_spinner: bool,
}

impl TerminalEmitter {
    pub fn new_from_app(app: &AppState, working_mode: WorkingMode) -> Self { /* ... */ }
}
```

Implement `Emitter` for it, mapping each `Event` variant to the appropriate crossterm operation:

| Event | TerminalEmitter action |
|---|---|
| `Started` | Start spinner |
| `AssistantDelta(chunk)` | Stop spinner (if first), feed chunk into render state |
| `AssistantMessageEnd { full_text }` | Flush render state, emit trailing newline |
| `ToolCall { name, args }` | Print dimmed `⚙ Using <name>` banner if verbose |
| `ToolResult { .. }` | Print dimmed result summary if verbose |
| `AutoContinueTriggered` | Print yellow `⟳ Continuing (N/M, R todos remaining)` to stderr |
| `SessionCompressing` | Print `Compressing session...` to stderr |
| `SessionCompressed` | Print `Session compressed.` to stderr |
| `SessionAutonamed` | Print `Session auto-named: <name>` to stderr |
| `Info(msg)` | Print to stdout |
| `Warning(msg)` | Print yellow to stderr |
| `Error(e)` | Print red to stderr |
| `Finished` | No-op (ensures trailing newline is flushed) |

**Verification:** write integration tests that construct a `TerminalEmitter`, feed it a sequence of events manually, and compare captured stdout/stderr to golden outputs. Use `assert_cmd` or similar to snapshot the rendered output of each event variant.

### Step 3: Implement `Engine::run` without wiring it

Implement `Engine::run` and `Engine::run_turn` following the pseudocode above. Use the existing helper functions (`before_chat_completion`, `after_chat_completion`, `apply_prelude`, `create_client`, `call_chat_completions`, `call_chat_completions_streaming`, `maybe_compress_session`, `maybe_autoname_session`) unchanged, just called through `ctx` instead of `&GlobalConfig`.

**Implementing `dispatch_command`** is the largest sub-task here because it needs to match all 37 `CoreCommand` variants and invoke the right `ctx` methods. Most variants are straightforward one-liners that call a corresponding method on `RequestContext`. A few need special handling:

- `CoreCommand::UseRole { name, trailing_text }` — activate role, then if `trailing_text` is `Some`, the outer `run` will flow through with the trailing text as `UserInput.text`.
- `CoreCommand::IncludeFiles` — reads files, converts to `FileInput` list, attaches to `ctx`'s next input (or fails if no input is provided).
- `CoreCommand::StarterRun(id)` — looks up the starter text on the active agent, fails if no agent.
- `CoreCommand::Macro` — delegates to `macro_execute`, which may itself call `Engine::run` internally for LLM-triggering macros.

**Verification:** write unit tests for `dispatch_command` using `NullEmitter`. Each test activates a command and asserts the expected state mutation on `ctx`. This is ~37 tests, one per variant, and they catch the bulk of regressions early.

Then write a handful of integration tests for `Engine::run` with `CollectingEmitter`, asserting the expected event sequence for:
- Plain prompt, no tools, streaming
- Plain prompt, no tools, non-streaming
- Prompt that triggers 2 tool calls
- Prompt that triggers auto-continuation (mock the LLM response)
- Prompt on a session that crosses the compression threshold
- Command-only request (`.info`)
- Command + prompt request (`.role explain "..."`)

### Step 4: Wire CLI to `Engine::run`

Replace `main.rs::start_directive` with a thin wrapper:

```rust
async fn start_directive(
    app: Arc<AppState>,
    ctx: &mut RequestContext,
    input_text: String,
    files: Vec<String>,
    code_mode: bool,
) -> Result<()> {
    let engine = Engine::new(app.clone());
    let emitter = TerminalEmitter::new_from_app(&app, WorkingMode::Cmd);

    let req = RunRequest {
        input: Some(UserInput::from_text_and_files(input_text, files)),
        command: None,
        options: {
            let mut o = RunOptions::cli();
            o.extract_code = code_mode && !*IS_STDOUT_TERMINAL;
            o
        },
    };

    match engine.run(ctx, req, &emitter).await {
        Ok(_outcome) => Ok(()),
        Err(CoreError::Cancelled) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
```

**Verification:** manual smoke test. Run `loki "hello"`, `loki --code "write a rust hello world"`, `loki --role explain "what is TCP"`. All should produce identical output to before the change.

### Step 5: Wire REPL to `Engine::run`

Replace `repl/mod.rs::ask` with a wrapper that calls the engine. The REPL's outer loop that reads lines and calls `run_repl_command` stays. `run_repl_command` for non-dot-command lines constructs a `RunRequest { input: Some(...), .. }` and calls `Engine::run`. Dot-commands get parsed into `CoreCommand` and called as `RunRequest { command: Some(...), input: None, .. }` (or with input if they carry trailing text).

```rust
// In Repl:
async fn handle_line(&mut self, line: &str) -> Result<()> {
    let req = if let Some(rest) = line.strip_prefix('.') {
        parse_dot_command_to_run_request(rest, &self.ctx)?
    } else {
        RunRequest {
            input: Some(UserInput::from_text(line.to_string())),
            command: None,
            options: RunOptions::repl_turn(),
        }
    };

    match self.engine.run(&mut self.ctx, req, &self.emitter).await {
        Ok(_) => Ok(()),
        Err(CoreError::Cancelled) => Ok(()),
        Err(e) => {
            self.emitter.emit(Event::Error(&e)).await.ok();
            Ok(())
        }
    }
}
```

**Verification:** manual smoke test of the REPL. Run through a typical session:
1. `loki` → REPL starts
2. `hello` → plain prompt works
3. `.role explain` → role activates
4. `what is TCP` → responds under the role
5. `.session` → session starts
6. Several messages → conversation continues
7. `.info session` → info prints
8. `.compress session` → compression runs
9. `.agent sisyphus` → agent activates with sub-agents
10. `write a hello world in rust` → tool calls + output
11. `.exit agent` → agent exits, previous session still active
12. `.exit` → REPL exits

Every interaction should behave identically to pre-Phase-2. Any visual difference is a bug.

### Step 6: Delete the old `start_directive` and `ask`

Once CLI and REPL both route through `Engine::run` and all tests/smoke tests pass, delete the old function bodies. Remove any now-unused imports. Run `cargo check` and `cargo test`.

**Verification:** full test suite green, no dead code warnings.

### Step 7: Tidy and document

- Add rustdoc comments on `Engine`, `RunRequest`, `RunOptions`, `Emitter`, `Event`, `CoreCommand`, `CoreError`.
- Add an `examples/` subdirectory under `src/engine/` showing how to call the engine with each emitter.
- Update `docs/AGENTS.md` with a note that CLI now supports auto-continuation (since it's no longer a REPL-only feature).
- Update `docs/REST-API-ARCHITECTURE.md` to remove any "in Phase 2" placeholders.

---

## Risks and Watch Items

| Risk | Severity | Mitigation |
|---|---|---|
| **Terminal rendering regressions** | High | Golden-file snapshot tests for every `Event` variant. Manual smoke tests across all common REPL flows. Keep `TerminalEmitter` as a thin wrapper — no logic changes in the render code itself. |
| **Auto-continuation recursion limits** | Medium | The new `Engine::run` uses `Box::pin` for the auto-continuation recursive call. Verify with a mock LLM that `max_auto_continues = 100` doesn't blow the stack. |
| **Cancellation during tool execution** | Medium | Tool execution currently uses `AbortSignal`; the new path uses `CancellationToken`. Write a shim that translates. Write a test that cancels mid-tool-call and verifies graceful cleanup (no orphaned subprocesses, no leaked file descriptors). |
| **Command parsing fidelity** | Medium | The dot-command parser in today's REPL is hand-written and has edge cases. Port the parsing code verbatim into a dedicated `parse_dot_command_to_run_request` function with unit tests for every edge case found in today's code. |
| **Macro execution recursion** | Medium | `.macro` can invoke LLM calls, which now go through `Engine::run`, which can invoke more macros. Verify there's a recursion depth limit or cycle detection; add one if missing. |
| **Emitter error propagation** | Low | Emitter errors (ClientDisconnected) should NOT abort session save logic. Engine must continue executing after the first `EmitError::ClientDisconnected` — just stop emitting. Write a test that simulates a disconnected emitter mid-response and asserts the session is still correctly persisted. |
| **Spinner interleaving with tool output** | Low | Today's spinner is tightly coupled to the stream handler. If the new order of operations fires a tool call before the spinner is stopped, you'll get garbled output. Test this specifically. |
| **Feature flag: `auto_continue` in CLI** | Low | After Phase 2, CLI *could* support auto-continuation but it's not exposed. Decision: leave it off by default in `RunOptions::cli()`, add a `--auto-continue` flag in a separate follow-up if desired. Don't sneak behavior changes into this refactor. |

---

## What Phase 2 Does NOT Do

- **No new features.** Everything that worked before works the same way after.
- **No API server.** `JsonEmitter` and `SseEmitter` are placeholders — Phase 4 implements them.
- **No `SessionStore` abstraction.** That's Phase 3.
- **No `ToolScope` unification.** That landed in Phase 1 Step 6.5.
- **No changes to LLM client code.** `call_chat_completions` and `call_chat_completions_streaming` keep their existing signatures.
- **No MCP factory pooling.** That's Phase 5.
- **No dot-command syntax changes.** The REPL still accepts exactly the same dot-commands; they just parse into `CoreCommand` instead of being hand-dispatched in `run_repl_command`.

The sole goal of Phase 2 is: **extract the pipeline into Engine::run, route CLI and REPL through it, and prove via tests and smoke tests that nothing regressed.**

---

## Entry Criteria (from Phase 1)

Before starting Phase 2, Phase 1 must be complete:

- [ ] `GlobalConfig` type alias is removed
- [ ] `AppState` and `RequestContext` are the only state holders
- [ ] All 91 callsites in the original migration table have been updated
- [ ] `cargo test` passes with no `Config`-based tests remaining
- [ ] CLI and REPL manual smoke tests pass identically to pre-Phase-1

## Exit Criteria (Phase 2 complete)

- [ ] `src/engine/` module exists with Engine, Emitter, Event, CoreCommand, RunRequest, RunOutcome, CoreError
- [ ] `TerminalEmitter` implemented and wrapping all existing render paths
- [ ] `NullEmitter` and `CollectingEmitter` implemented
- [ ] `start_directive` in main.rs is a thin wrapper around `Engine::run`
- [ ] REPL's per-line handler routes through `Engine::run`
- [ ] All 37 `CoreCommand` variants implemented with unit tests
- [ ] Integration tests for the 7 engine scenarios listed in Step 3
- [ ] Manual smoke tests for CLI and REPL match pre-Phase-2 behavior
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean
- [ ] Phase 3 (SessionStore abstraction) can begin
