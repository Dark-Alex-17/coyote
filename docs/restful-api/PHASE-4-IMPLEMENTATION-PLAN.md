# Phase 4 Implementation Plan: REST API Server

## Overview

Phase 4 introduces a `--serve` mode that starts an HTTP server exposing Loki's functionality as a RESTful API. The server is a thin axum layer on top of `Engine::run()` — most of the work is mapping HTTP requests into `RunRequest`s, mapping `Emitter` events into JSON or Server-Sent Events, and providing baseline auth, cancellation, and graceful shutdown. By the end of this phase, Loki can run as a backend service that multiple clients can talk to simultaneously, each with their own session.

**Estimated effort:** ~1–2 weeks
**Risk:** Low–medium. The core pipeline (Engine) is unchanged; the risk is in the HTTP layer's correctness around streaming, cancellation, and concurrent session handling.
**Depends on:** Phases 1–3 complete. `SessionStore` with UUID identity, `Engine::run()` as the pipeline entrypoint, `Emitter` trait with working `TerminalEmitter` + `CollectingEmitter`.

---

## Why Phase 4 Exists

After Phase 3, everything the API server needs is already in place:
- `AppState` is a clonable `Arc` holding global services, safe to share across concurrent HTTP handlers.
- `RequestContext` is per-request mutable state with no hidden global singletons.
- `Engine::run()` is the single pipeline entrypoint that works for any frontend.
- `SessionStore` serves sessions by UUID with per-session mutex serialization.
- `Emitter` trait decouples output from destination.

What's missing is the last mile: accepting HTTP requests, routing them to `Engine::run()`, and turning `Event`s into HTTP responses. This phase builds exactly that.

The mental model is "Loki as a backend service." A frontend developer should be able to `curl -X POST http://localhost:3400/v1/completions -d '{"prompt":"hello"}'` and get a sensible response. A JavaScript app should be able to open an EventSource to `/v1/sessions/:id/completions?stream=true` and get live token streaming. An automation script should be able to maintain session state across many requests by passing back the same session UUID.

---

## The Architecture After Phase 4

```
┌─────────────────────────────────────────────┐
│         loki --serve --port 3400            │
│  ┌───────────────────────────────────────┐  │
│  │              axum Router              │  │
│  │  ┌─────────────┐  ┌────────────────┐  │  │
│  │  │   Middleware│  │    Handlers    │  │  │
│  │  │  - Auth     │  │  /v1/*         │  │  │
│  │  │  - Trace    │  │                │  │  │
│  │  │  - CORS     │  │                │  │  │
│  │  │  - Limit    │  │                │  │  │
│  │  └──────┬──────┘  └────────┬───────┘  │  │
│  └─────────┼──────────────────┼──────────┘  │
│            ▼                  ▼             │
│  ┌───────────────────────────────────┐      │
│  │       Arc<AppState> (shared)      │      │
│  └────────────────┬──────────────────┘      │
│                   ▼                         │
│  ┌───────────────────────────────────┐      │
│  │  Per-request RequestContext +     │      │
│  │  JsonEmitter or SseEmitter        │      │
│  └────────────────┬──────────────────┘      │
│                   ▼                         │
│  ┌───────────────────────────────────┐      │
│  │        Engine::run()              │      │
│  └───────────────────────────────────┘      │
└─────────────────────────────────────────────┘
```

---

## API Surface

### Versioning

All endpoints live under `/v1/`. The version prefix lets us ship breaking changes later without breaking existing clients. `/v2/` endpoints can coexist with `/v1/` indefinitely.

### Endpoint summary

```
Authentication
POST   /v1/auth/check                        # validate API key, returns subject info

Metadata
GET    /v1/models                            # list available LLM models
GET    /v1/agents                            # list installed agents
GET    /v1/roles                             # list installed roles
GET    /v1/rags                              # list standalone RAGs
GET    /v1/info                              # server build info, health

One-shot completions
POST   /v1/completions                       # stateless completion (no session)

Sessions
POST   /v1/sessions                          # create a new session (returns UUID)
GET    /v1/sessions                          # list sessions visible to this caller
GET    /v1/sessions/:id                      # get session metadata + message history
DELETE /v1/sessions/:id                      # delete a session
POST   /v1/sessions/:id/completions          # send a prompt into a session
POST   /v1/sessions/:id/compress             # manually trigger compression
POST   /v1/sessions/:id/empty                # clear messages (keep session record)

Role attachment
POST   /v1/sessions/:id/role                 # activate role on session
DELETE /v1/sessions/:id/role                 # detach role

Agent attachment
POST   /v1/sessions/:id/agent                # activate agent on session
DELETE /v1/sessions/:id/agent                # deactivate agent

RAG attachment
POST   /v1/sessions/:id/rag                  # attach standalone RAG
DELETE /v1/sessions/:id/rag                  # detach RAG
POST   /v1/rags/:name/rebuild                # rebuild a RAG index
```

### Request/response shapes

**One-shot completion:**

```
POST /v1/completions
Content-Type: application/json
Authorization: Bearer <api-key>

{
  "prompt": "Explain TCP handshake",
  "model": "openai:gpt-4o",         // optional: overrides default
  "role": "explain",                 // optional: apply role for this one request
  "agent": "oracle",                 // optional: run through an agent (no session retention)
  "stream": false,                   // optional: SSE vs JSON
  "files": [                         // optional: file attachments
    {"path": "/abs/path/doc.pdf"},
    {"url": "https://example.com/x"}
  ],
  "temperature": 0.7,                // optional override
  "auto_continue": false             // optional: enable agent auto-continuation
}
```

**Non-streaming response (default):**

```json
{
  "request_id": "7a1b...",
  "session_id": null,
  "final_message": "The TCP handshake is a three-way protocol ...",
  "tool_calls": [
    {"id": "tc_1", "name": "web_search", "args": "...", "result": "...", "is_error": false}
  ],
  "turns": 2,
  "compressed": false,
  "auto_continued": 0,
  "usage": {
    "input_tokens": 120,
    "output_tokens": 458
  }
}
```

**Streaming response** (`Accept: text/event-stream` or `stream: true`):

```
event: started
data: {"request_id":"7a1b...","session_id":null}

event: assistant_delta
data: {"text":"The TCP "}

event: assistant_delta
data: {"text":"handshake is "}

event: tool_call
data: {"id":"tc_1","name":"web_search","args":"..."}

event: tool_result
data: {"id":"tc_1","name":"web_search","result":"...","is_error":false}

event: assistant_delta
data: {"text":" a three-way protocol..."}

event: finished
data: {"outcome":{"turns":2,"tool_calls":1,"compressed":false}}
```

**Create session:**

```
POST /v1/sessions

{
  "alias": "my-project",      // optional; UUID-only if omitted
  "role": "explain",          // optional: pre-attach a role
  "agent": "sisyphus",        // optional: pre-attach an agent
  "rag": "mydocs",            // optional: pre-attach a RAG
  "model": "openai:gpt-4o"    // optional: pre-set model
}
```

**Response:**

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "alias": "my-project",
  "agent": "sisyphus",
  "role": "explain",
  "rag": "mydocs",
  "model": "openai:gpt-4o",
  "created_at": "2026-04-10T15:32:11Z"
}
```

**Session completion:**

```
POST /v1/sessions/550e8400-.../completions

{
  "prompt": "what was the bug we found yesterday?",
  "stream": true,
  "auto_continue": true
}
```

Returns the same shape as `/v1/completions`, but with `session_id` populated and agent runtime state preserved across calls.

**Error responses** (standard across all endpoints):

```json
{
  "error": {
    "code": "session_not_found",
    "message": "No session with id 550e8400-...",
    "request_id": "7a1b..."
  }
}
```

HTTP status codes map from `CoreError::http_status()` (defined in Phase 2):
- `InvalidRequest` → 400
- `Unauthorized` → 401
- `NotFound` → 404
- `InvalidState` → 409 (expected state doesn't match)
- `Cancelled` → 499 (client-closed request, borrowed from nginx)
- `ProviderError` → 502 (upstream LLM failed)
- `ToolError` → 500
- `Other` → 500

---

## Core Types

### `ApiConfig`

```rust
#[derive(Clone, Deserialize)]
pub struct ApiConfig {
    pub enabled: bool,
    pub listen_addr: SocketAddr,
    pub auth: AuthConfig,
    pub cors: CorsConfig,
    pub limits: LimitsConfig,
    pub request_timeout_seconds: u64,
    pub shutdown_grace_seconds: u64,
}

#[derive(Clone, Deserialize)]
pub enum AuthConfig {
    Disabled,                                      // dev only
    StaticKeys { keys: Vec<AuthKeyEntry> },        // simple key list
    // future: JwtIssuer { ... }, OAuthIntrospect { ... }
}

#[derive(Clone, Deserialize)]
pub struct AuthKeyEntry {
    pub subject: String,                           // for logs
    pub key_hash: String,                          // bcrypt or argon2 hash
    pub scopes: Vec<String>,
}

#[derive(Clone, Deserialize)]
pub struct CorsConfig {
    pub allowed_origins: Vec<String>,              // empty = no CORS
    pub allow_credentials: bool,
}

#[derive(Clone, Deserialize)]
pub struct LimitsConfig {
    pub max_body_bytes: usize,                     // request body limit
    pub max_concurrent_requests: usize,            // semaphore
    pub rate_limit_per_minute: Option<usize>,      // optional per-subject
}
```

`ApiConfig` loads from `config.yaml` under a new top-level `api:` block. It's NOT part of `AppConfig` because it only matters in `--serve` mode; in CLI/REPL mode it's ignored.

```yaml
# config.yaml
api:
  enabled: false                        # false = --serve refuses to start without explicit enable
  listen_addr: "127.0.0.1:3400"
  auth:
    mode: StaticKeys
    keys:
      - subject: "alice"
        key_hash: "$argon2id$..."
        scopes: ["read", "write"]
  cors:
    allowed_origins: []
    allow_credentials: false
  limits:
    max_body_bytes: 1048576             # 1 MiB
    max_concurrent_requests: 64
    rate_limit_per_minute: null
  request_timeout_seconds: 300          # 5 minutes default
  shutdown_grace_seconds: 30
```

### `ApiState`

```rust
#[derive(Clone)]
pub struct ApiState {
    pub app: Arc<AppState>,
    pub engine: Arc<Engine>,
    pub config: Arc<ApiConfig>,
    pub request_counter: Arc<AtomicU64>,
    pub active_requests: Arc<Semaphore>,
}
```

`ApiState` is the axum-friendly wrapper that every handler receives via the `State` extractor. It's clonable (cheap — all fields are `Arc` or atomic) and thread-safe. Handlers get a clone per request.

### `JsonEmitter`

Phase 2 promised `JsonEmitter` and `SseEmitter` as deferred deliverables. Phase 4 implements them.

```rust
pub struct JsonEmitter {
    events: Mutex<Vec<OwnedEvent>>,
    tool_calls: Mutex<Vec<ToolCallRecord>>,
    final_message: Mutex<Option<String>>,
    outcome: Mutex<Option<RunOutcome>>,
}

impl JsonEmitter {
    pub fn new() -> Self { /* ... */ }

    /// Consume the emitter and return the JSON response body.
    pub fn into_response(self) -> serde_json::Value { /* ... */ }
}

#[async_trait]
impl Emitter for JsonEmitter {
    async fn emit(&self, event: Event<'_>) -> Result<(), EmitError> {
        match event {
            Event::AssistantDelta(text) => { /* accumulate */ }
            Event::AssistantMessageEnd { full_text } => { /* set final_message */ }
            Event::ToolCall { .. } | Event::ToolResult { .. } => { /* record */ }
            Event::Finished { outcome } => { /* store */ }
            _ => { /* record as event */ }
        }
        Ok(())
    }
}
```

The non-streaming HTTP handler creates a `JsonEmitter`, calls `Engine::run`, and then calls `.into_response()` to get the final JSON body.

### `SseEmitter`

```rust
pub struct SseEmitter {
    sender: mpsc::Sender<Result<axum::response::sse::Event, axum::Error>>,
    client_disconnected: Arc<AtomicBool>,
}

#[async_trait]
impl Emitter for SseEmitter {
    async fn emit(&self, event: Event<'_>) -> Result<(), EmitError> {
        if self.client_disconnected.load(Ordering::Relaxed) {
            return Err(EmitError::ClientDisconnected);
        }
        let sse_event = to_sse_event(&event)?;
        self.sender
            .send(Ok(sse_event))
            .await
            .map_err(|_| {
                self.client_disconnected.store(true, Ordering::Relaxed);
                EmitError::ClientDisconnected
            })?;
        Ok(())
    }
}

fn to_sse_event(event: &Event<'_>) -> Result<axum::response::sse::Event, serde_json::Error> {
    let (name, data) = match event {
        Event::Started { .. } => ("started", serde_json::to_string(event)?),
        Event::AssistantDelta(text) => ("assistant_delta", json!({ "text": text }).to_string()),
        Event::AssistantMessageEnd { .. } => ("assistant_message_end", serde_json::to_string(event)?),
        Event::ToolCall { .. } => ("tool_call", serde_json::to_string(event)?),
        Event::ToolResult { .. } => ("tool_result", serde_json::to_string(event)?),
        Event::AutoContinueTriggered { .. } => ("auto_continue_triggered", serde_json::to_string(event)?),
        Event::SessionCompressing => ("session_compressing", "{}".to_string()),
        Event::SessionCompressed { .. } => ("session_compressed", serde_json::to_string(event)?),
        Event::SessionAutonamed(_) => ("session_autonamed", serde_json::to_string(event)?),
        Event::Info(msg) => ("info", json!({ "message": msg }).to_string()),
        Event::Warning(msg) => ("warning", json!({ "message": msg }).to_string()),
        Event::Error(err) => ("error", serde_json::to_string(err)?),
        Event::Finished { outcome } => ("finished", serde_json::to_string(outcome)?),
    };
    Ok(axum::response::sse::Event::default().event(name).data(data))
}
```

The streaming handler creates an mpsc channel, hands the sender half to an `SseEmitter`, and returns an `axum::response::sse::Sse` wrapping the receiver half. axum streams each event as it's emitted, with automatic flushing. If the client disconnects, the send fails, `client_disconnected` is set, and subsequent emits return `ClientDisconnected` — which the engine respects by continuing to completion without emitting further (Phase 2 designed this behavior in).

---

## Middleware Stack

The axum router wraps handlers in a layered middleware stack. Order matters because middleware is applied outside-in on requests, inside-out on responses.

```rust
let router = Router::new()
    .route("/v1/auth/check", post(handlers::auth_check))
    .route("/v1/models", get(handlers::list_models))
    .route("/v1/agents", get(handlers::list_agents))
    .route("/v1/roles", get(handlers::list_roles))
    .route("/v1/rags", get(handlers::list_rags))
    .route("/v1/info", get(handlers::info))
    .route("/v1/completions", post(handlers::one_shot_completion))
    .route("/v1/sessions", post(handlers::create_session).get(handlers::list_sessions))
    .route("/v1/sessions/:id", get(handlers::get_session).delete(handlers::delete_session))
    .route("/v1/sessions/:id/completions", post(handlers::session_completion))
    .route("/v1/sessions/:id/compress", post(handlers::compress_session))
    .route("/v1/sessions/:id/empty", post(handlers::empty_session))
    .route("/v1/sessions/:id/role", post(handlers::set_role).delete(handlers::clear_role))
    .route("/v1/sessions/:id/agent", post(handlers::set_agent).delete(handlers::clear_agent))
    .route("/v1/sessions/:id/rag", post(handlers::set_rag).delete(handlers::clear_rag))
    .route("/v1/rags/:name/rebuild", post(handlers::rebuild_rag))
    .layer(middleware::from_fn_with_state(state.clone(), middleware::auth))
    .layer(middleware::from_fn(middleware::request_id))
    .layer(middleware::from_fn_with_state(state.clone(), middleware::concurrency_limit))
    .layer(middleware::from_fn(middleware::tracing))
    .layer(middleware::from_fn(middleware::error_handler))
    .layer(tower_http::timeout::TimeoutLayer::new(Duration::from_secs(
        state.config.request_timeout_seconds,
    )))
    .layer(tower_http::limit::RequestBodyLimitLayer::new(state.config.limits.max_body_bytes))
    .layer(cors_layer(&state.config.cors))
    .with_state(state);
```

### Middleware responsibilities

**auth** — Validates `Authorization: Bearer <key>` header against the configured auth provider. Compares against stored hashes (bcrypt/argon2), never plaintext. On success, attaches an `AuthContext { subject, scopes }` to request extensions. On failure, returns 401 immediately without calling the handler. If `AuthConfig::Disabled`, synthesizes an `AuthContext { subject: "anonymous", scopes: vec!["*"] }` for local dev.

**request_id** — Generates a UUID request ID, attaches it to request extensions for downstream correlation, emits it as `X-Request-Id` in the response headers. Used by tracing and error handlers.

**concurrency_limit** — Acquires a permit from `state.active_requests` semaphore with a short timeout. If the server is saturated, returns 503 Service Unavailable immediately. This protects against runaway connection counts exhausting resources.

**tracing** — Wraps the request in a `tracing::Span` carrying the request ID, subject, method, path, and session ID if present. Every log line and every tool call emitted during the request carries this span context. Essential for debugging production issues.

**error_handler** — Catches `CoreError` from handler results and maps to proper HTTP responses using `CoreError::http_status()` and a JSON error body. Ensures no handler leaks an `anyhow::Error` or raw `?` into an axum 500.

**timeout** — Overall request deadline. After N seconds (default 300), the request is aborted. This is a backstop — the engine's per-request cancellation token is the primary cancellation mechanism.

**body limit** — Rejects requests larger than the configured max. Default 1 MiB is enough for prompts with several files attached; adjustable in config.

**cors** — Attaches `Access-Control-Allow-Origin` headers for cross-origin browsers. Empty allowed origins = no CORS headers emitted (safe default). `allow_credentials: true` enables cookie/auth forwarding.

### What's NOT in middleware

- **Rate limiting per subject** — deferred. The `rate_limit_per_minute` config option is wired through but the middleware is a stub in Phase 4. Real rate limiting with sliding windows lands in a follow-up.
- **Request/response logging** — use the tracing middleware's output; don't add a separate HTTP log layer.
- **Metrics** — deferred to Phase 4.5 (Prometheus endpoint). Phase 4 just exposes counters in `ApiState`.
- **Content negotiation** — Phase 4 assumes JSON requests. `Accept: text/event-stream` is the only alternate content type we handle, and only on completion endpoints.

---

## Handler Pattern

Every handler follows the same shape:

```rust
pub async fn session_completion(
    State(state): State<ApiState>,
    Extension(auth): Extension<AuthContext>,
    Extension(request_id): Extension<Uuid>,
    Path(session_id): Path<String>,
    Json(req): Json<CompletionRequest>,
) -> Result<Response, ApiError> {
    // 1. Parse domain types
    let session_id = SessionId::parse(&session_id)
        .map_err(|_| ApiError::bad_request("invalid session id"))?;

    // 2. Open the session handle
    let handle = state.app.sessions.open(None, session_id).await
        .map_err(|e| match e {
            StoreError::NotFound { .. } => ApiError::not_found("session", &session_id.to_string()),
            other => ApiError::from(other),
        })?;

    // 3. Build RequestContext from AppState + session
    let mut ctx = RequestContext::new(state.app.clone(), WorkingMode::Api);
    ctx.session = Some(handle);
    ctx.auth = Some(auth);

    // 4. Build cancellation token that fires on client disconnect
    let cancel = CancellationToken::new();

    // 5. Convert the HTTP request to a RunRequest
    let run_req = RunRequest {
        input: Some(UserInput::from_api(req.prompt, req.files)?),
        command: None,
        options: {
            let mut o = if req.session_active {
                RunOptions::api_session()
            } else {
                RunOptions::api_oneshot()
            };
            o.stream = req.stream;
            o.auto_continue = req.auto_continue.unwrap_or(false);
            o.cancel = cancel.clone();
            o
        },
    };

    // 6. Branch on streaming vs JSON
    if req.stream {
        // Create SseEmitter + channel, spawn engine task, return Sse response
        let (tx, rx) = mpsc::channel(32);
        let emitter = SseEmitter::new(tx);
        let engine = state.engine.clone();

        tokio::spawn(async move {
            let _ = engine.run(&mut ctx, run_req, &emitter).await;
            // Emitter Drop closes the channel; Sse stream ends naturally
        });

        Ok(Sse::new(ReceiverStream::new(rx))
            .keep_alive(KeepAlive::default())
            .into_response())
    } else {
        // Use JsonEmitter synchronously, return JSON body
        let emitter = JsonEmitter::new();
        state.engine.run(&mut ctx, run_req, &emitter).await
            .map_err(ApiError::from)?;
        Ok(Json(emitter.into_response()).into_response())
    }
}
```

The streaming path spawns a background task because axum needs to return the `Response` (with the SSE stream) before the engine finishes its work. The task owns the `ctx` and `emitter`, runs to completion, and naturally terminates when the engine returns. The channel closing signals the end of the stream to axum.

The non-streaming path runs synchronously in the handler task because we need the full result before returning the response body.

---

## Cancellation and Client Disconnect

Two cancellation sources, one unified mechanism:

1. **Client disconnect during streaming.** axum signals this by dropping the SSE receiver. The next `SseEmitter::emit` call fails with `ClientDisconnected`, which the engine handles by stopping further emits but continuing to completion so session state is persisted correctly.

2. **Request timeout.** The outer tower timeout layer fires after N seconds, dropping the handler's future. This cancels any pending awaits in the engine, which propagates through tokio cancellation. Active tool calls (especially bash/python/typescript subprocesses) need to be killed cleanly — this is the same concern as Phase 2's Ctrl-C handling.

The engine's `CancellationToken` handles both cases uniformly. For streaming, the handler watches the SSE sender's `closed()` signal and triggers `cancel.cancel()` when the client goes away. For timeout, tower's dropped future causes the handler task to be aborted, which drops `cancel` and fires any `cancelled()` waiters in the engine.

```rust
// Inside the streaming handler:
let cancel_for_disconnect = cancel.clone();
let send_tx = tx.clone();
tokio::spawn(async move {
    send_tx.closed().await;  // resolves when receiver drops
    cancel_for_disconnect.cancel();
});
```

**Tool call cancellation** is the interesting case. A running bash/python/typescript subprocess must be killed when `cancel` fires. The existing tool execution code uses `AbortSignal` from the `abort_on_ctrlc` crate; Phase 2's shim layer adapts it to `CancellationToken`. Phase 4 doesn't need to change this — it just needs to verify that the adapter is still firing correctly when cancellation comes from HTTP disconnect instead of Ctrl-C.

---

## Per-Request State Isolation

The critical correctness property: **two concurrent requests must not share mutable state.** The architecture from Phases 1–3 makes this structural rather than something we have to police:

- `AppState` is `Arc`-wrapped and contains only immutable config and shared services (vault, RAG cache, MCP factory, session store).
- `RequestContext` is constructed fresh in each handler — two requests get two independent contexts.
- `SessionHandle` uses per-session `Mutex` serialization — two concurrent requests on the *same* session wait their turn (by design).
- `McpFactory` acquires handles via per-key sharing — two requests using the same MCP server share one process; two using different servers get independent processes.
- `RagCache` shares `Arc<Rag>` via weak refs — same sharing property.

The one place where the architecture can't help us is **agent runtime isolation**. Two concurrent API requests on two different sessions, both running agents, must get two fully independent `AgentRuntime`s with their own supervisors, inboxes, todo lists, and escalation queues. Phase 1 Step 6.5 made this work by putting `AgentRuntime` on `RequestContext`, which is already per-request. Phase 4 just needs to verify nothing regresses.

**Integration test for this:** spin up 10 concurrent requests, each running a different agent with tools, and assert that each one gets its own tool call history, its own todo list, and its own eventual response. Use a mock LLM so the test is deterministic.

---

## Migration Strategy

### Step 1: Add dependencies and scaffolding

Add to `Cargo.toml`:

```toml
axum = { version = "0.8", features = ["macros"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["cors", "limit", "timeout", "trace"] }
argon2 = "0.5"
```

`hyper` is already present. `tokio-stream` for SSE.

Create module structure:

- `src/api/mod.rs` — module root, `serve()` entrypoint
- `src/api/config.rs` — `ApiConfig`, `AuthConfig`, etc.
- `src/api/state.rs` — `ApiState`
- `src/api/auth.rs` — middleware + `AuthContext`
- `src/api/middleware.rs` — other middlewares (request_id, tracing, concurrency_limit, error_handler)
- `src/api/error.rs` — `ApiError` + conversion from `CoreError`
- `src/api/emitters/json.rs` — `JsonEmitter`
- `src/api/emitters/sse.rs` — `SseEmitter`
- `src/api/handlers/mod.rs` — handler module root
- `src/api/handlers/completions.rs` — one-shot and session completions
- `src/api/handlers/sessions.rs` — session CRUD
- `src/api/handlers/metadata.rs` — list models/agents/roles/rags
- `src/api/handlers/scope.rs` — role/agent/rag attachment endpoints
- `src/api/handlers/rag.rs` — rebuild endpoint

Register `pub mod api;` in `src/main.rs`. Add a `--serve` CLI flag that calls `api::serve(app_state).await`.

**Verification:** `cargo check` clean with empty handler stubs returning 501 Not Implemented.

### Step 2: Implement auth middleware and error handling

Build the auth middleware against `AuthConfig::StaticKeys` using argon2 for verification. Implement `ApiError` with `IntoResponse` that produces the JSON error body. Implement `From<CoreError>` for `ApiError` using `CoreError::http_status()` and `CoreError::message()` (add those methods to `CoreError` in Phase 2 if they don't exist yet; otherwise add here).

Write unit tests:
- Valid key → handler runs, `AuthContext` is attached
- Invalid key → 401
- Missing key → 401
- `AuthConfig::Disabled` → anonymous context synthesized

**Verification:** Auth tests pass. `curl -H "Authorization: Bearer <valid-key>" http://localhost:3400/v1/info` returns info; without the header returns 401.

### Step 3: Implement `JsonEmitter` and `SseEmitter`

Both are relatively mechanical. `JsonEmitter` accumulates events into a buffer and exposes `into_response()`. `SseEmitter` converts each event to an axum SSE frame and pushes into an mpsc channel.

Write unit tests using `NullEmitter` → feed a scripted sequence of events → assert the resulting JSON or SSE frames.

**Verification:** Both emitters have unit tests that drive a scripted `Event` sequence and compare to golden outputs.

### Step 4: Implement metadata handlers

Start with the easy endpoints: `GET /v1/models`, `/v1/agents`, `/v1/roles`, `/v1/rags`, `/v1/info`. These don't call the engine — they just read from `AppState` and return JSON.

**Verification:** `curl` each endpoint and inspect output. Write integration tests that spin up the router and hit each endpoint.

### Step 5: Implement session CRUD handlers

`POST /v1/sessions` creates via `SessionStore::create`. `GET /v1/sessions` lists via `SessionStore::list`. `GET /v1/sessions/:id` reads metadata + message history via `SessionStore::open` + handle lock. `DELETE /v1/sessions/:id` calls `SessionStore::delete`.

These handlers don't call the engine either. They're thin wrappers around `SessionStore`.

**Verification:** Create a session via POST, list it, read it, delete it, confirm 404 after delete. All through `curl`.

### Step 6: Implement one-shot completion handler

`POST /v1/completions` is the first engine-calling handler. It constructs a fresh `RequestContext` with no session, builds a `RunRequest` from the HTTP body, and calls `Engine::run` with either `JsonEmitter` or `SseEmitter` based on the `stream` flag.

This is where the streaming infrastructure first gets exercised end-to-end. Test both modes:

```bash
# Non-streaming
curl -X POST http://localhost:3400/v1/completions \
  -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  -d '{"prompt":"hello"}'

# Streaming
curl -N -X POST http://localhost:3400/v1/completions \
  -H "Authorization: Bearer <key>" \
  -H "Content-Type: application/json" \
  -H "Accept: text/event-stream" \
  -d '{"prompt":"hello","stream":true}'
```

**Verification:** Both modes work with a real LLM. Disconnect the streaming client mid-response (Ctrl-C on curl) and verify the engine task gets cancelled cleanly — no orphaned MCP subprocesses, no hung tool executions.

### Step 7: Implement session completion handler

`POST /v1/sessions/:id/completions` is the same as one-shot but with a session attached. The handler calls `store.open(id)`, builds a context with `ctx.session = Some(handle)`, and proceeds as before. Session state is automatically persisted by the engine at the end of the turn.

Concurrent request test: spin up 10 concurrent `curl` commands all hitting the same session. Assert:
- All 10 complete successfully
- The session has 10 message pairs appended in some order (serialized by the per-session mutex)
- No lost updates, no corrupted YAML

**Verification:** Concurrent test passes reliably. Run it 100 times in a loop to catch races.

### Step 8: Implement scope attachment handlers

`POST /v1/sessions/:id/role`, `/agent`, `/rag` and their `DELETE` counterparts. Each one opens the session handle, constructs a `RunRequest` with a `CoreCommand` variant (`UseRole`, `UseAgent`, `UseRag`), and calls the engine with no input — just the command. The engine dispatches the command, mutates state, and the session is persisted.

**Verification:** `POST /v1/sessions/<id>/role {"name":"explain"}` activates the role. Subsequent completion on the session uses the role. `DELETE /v1/sessions/<id>/role` clears it.

### Step 9: Implement miscellaneous handlers

`POST /v1/sessions/:id/compress`, `/empty`, `POST /v1/rags/:name/rebuild`. Same pattern: translate to `CoreCommand` and dispatch.

**Verification:** All endpoints respond correctly.

### Step 10: Graceful shutdown

axum's graceful shutdown requires a signal future. Wire it up:

```rust
pub async fn serve(app: Arc<AppState>, config: ApiConfig) -> Result<()> {
    let state = ApiState::new(app, config);
    let router = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(state.config.listen_addr).await?;

    let shutdown_signal = async {
        tokio::signal::ctrl_c().await.ok();
        info!("Received shutdown signal, draining requests...");
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal)
        .await?;

    info!("Draining active sessions...");
    tokio::time::timeout(
        Duration::from_secs(state.config.shutdown_grace_seconds),
        drain_active_requests(&state),
    ).await.ok();

    info!("Shutdown complete.");
    Ok(())
}
```

`drain_active_requests` waits for the semaphore to return to full capacity, bounded by `shutdown_grace_seconds`. After the grace period, any remaining requests are force-cancelled.

**Verification:** Start server, send a long streaming request, hit Ctrl-C. The server should finish the in-flight request (up to the grace period) before exiting, not cut it off mid-stream.

### Step 11: Configuration loading and docs

Wire `ApiConfig` through `config.yaml` parsing. Add a default `api.enabled: false` so the server refuses to start without explicit opt-in. Document the config shape, endpoint schemas, and auth setup in `docs/REST-API-SERVER.md`.

**Verification:** Start with `api.enabled: false` → fatal error with helpful message. Start with `api.enabled: true` + no auth keys → fatal error demanding at least one key (unless `AuthConfig::Disabled` is explicit).

### Step 12: Integration test suite

Write a comprehensive integration test suite in `tests/api/` that exercises the full HTTP surface with a mock LLM:

- Auth: valid, invalid, missing, disabled
- Metadata: list each resource type
- Session lifecycle: create → list → read → delete
- One-shot completion: JSON + SSE
- Session completion: single + concurrent
- Scope attachment: role, agent, rag (set + clear)
- Cancellation: client disconnect mid-stream, timeout expiry
- Graceful shutdown: in-flight requests complete within grace period
- Concurrent sessions: 20 sessions, each with a few turns, all running at once

Use `reqwest` as the test client. Spin up the server on a random port per test. The mock LLM lives as a fake `Client` implementation that returns scripted responses.

**Verification:** All tests pass. CI runs them on every PR.

---

## Risks and Watch Items

| Risk | Severity | Mitigation |
|---|---|---|
| **SSE client disconnect detection lag** | High | The mpsc channel's `closed()` signal is the primary disconnect detector. Verify it fires within <1s of a real client disconnect. Add integration test with `reqwest` that opens a stream, sends a few events, drops the connection, and asserts the engine's cancellation token fires within 2s. |
| **Concurrent session writes losing data** | High | Phase 3's per-session mutex handles this structurally. Verify with the 100-concurrent-writers integration test from Phase 3 adapted to hit the HTTP layer. |
| **Orphaned tool subprocesses on timeout** | High | Tool execution must respect the cancellation token. Test: start a completion that triggers a bash tool running `sleep 60`, timeout at 5s, verify the `sleep` process is killed (not reparented to init). |
| **Auth key storage** | High | Store argon2 hashes, never plaintext. Rotate via config reload (future). Log subject (not key) on every request. Audit: no `println!` of any part of the key anywhere. |
| **Streaming body size growth** | Medium | A long session with many tool calls produces a lot of SSE frames. Verify the mpsc channel size (32) is enough; if not, backpressure causes the engine task to block on emit. Document in the emitter: `emit()` can await. |
| **CORS misconfiguration** | Medium | Default to no CORS. Require explicit origin allowlist. Log warnings on wildcard usage. Browser-accessible deployments should use a reverse proxy to terminate CORS. |
| **Auth bypass via malformed header** | Medium | Use axum's `Authorization` typed header extractor, not raw string parsing. Reject unknown schemes (only Bearer accepted). |
| **Rate limit stub** | Low | Document that `rate_limit_per_minute` is not yet implemented. Add an issue for follow-up. Protect against DoS with `max_concurrent_requests` in the meantime. |
| **Session metadata leak across users** | Low | `GET /v1/sessions` lists all sessions regardless of caller identity in Phase 4. Document this limitation: Phase 4's auth is coarse-grained (anyone with a valid key sees all sessions). Per-subject session ownership lands in a follow-up phase. Treat Phase 4 as single-tenant-per-key for now. |
| **Body size abuse** | Low | `max_body_bytes` caps payload. File uploads (not yet supported) would need separate multipart handling. |
| **Port binding failure** | Low | Fail fast with clear error if the configured port is in use or unreachable. Don't silently retry. |

---

## What Phase 4 Does NOT Do

- **No WebSocket support.** SSE is sufficient for server-to-client streaming; WebSockets would add bidirectional complexity we don't need. Client-to-server commands use regular HTTP POST.
- **No multi-tenancy.** All sessions are visible to any authenticated caller. Per-subject session ownership is a follow-up.
- **No rate limiting.** `rate_limit_per_minute` config exists but is a stub.
- **No metrics endpoint.** Counters are in memory; Prometheus scraping lands later.
- **No API versioning beyond `/v1/`.** Breaking changes would introduce `/v2/`.
- **No JWT or OAuth.** Static API keys only. JWT introspection can extend `AuthConfig` later.
- **No request signing.** Bearer tokens over HTTPS (users provide their own TLS termination via reverse proxy).
- **No admin endpoints.** Server management (reload config, view metrics, kill sessions) is not exposed.
- **No file upload.** File references in requests use absolute paths or URLs that the server fetches; no multipart uploads in Phase 4.
- **No MCP tool exposure over API.** The API calls the engine, which runs tools internally. Direct "execute this tool" API endpoints don't exist and are not planned.

---

## Entry Criteria (from Phase 3)

- [ ] `SessionStore` trait is the only path to session persistence
- [ ] `FileSessionStore` is wired into `AppState.sessions`
- [ ] Concurrent-write integration test from Phase 3 passes
- [ ] All session-touching callsites go through the store
- [ ] `Engine::run` handles `RunOptions::api_oneshot()` and `RunOptions::api_session()` modes
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean

## Exit Criteria (Phase 4 complete)

- [ ] `--serve` flag starts an HTTP server on the configured port
- [ ] `src/api/` module exists with all handlers, middleware, emitters
- [ ] `JsonEmitter` and `SseEmitter` implemented and tested
- [ ] Auth middleware validates argon2-hashed API keys
- [ ] All 19 endpoints listed in the API surface are implemented and return sensible responses
- [ ] Concurrent-session integration test passes (20 sessions, multiple turns, parallel)
- [ ] Client disconnect during streaming triggers engine cancellation within 2s
- [ ] Request timeout fires at the configured deadline
- [ ] Graceful shutdown drains in-flight requests within the grace period
- [ ] Tool subprocesses are killed on cancellation, not orphaned
- [ ] `docs/REST-API-SERVER.md` documents config, endpoints, and auth setup
- [ ] Full integration test suite in `tests/api/` passes
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean
- [ ] Phase 5 (Tool Scope Pooling) can optimize the hot path without changing the API surface
