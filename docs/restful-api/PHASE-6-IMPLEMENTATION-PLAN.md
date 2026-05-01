# Phase 6 Implementation Plan: Production Hardening

## Overview

Phase 6 closes out the refactor by picking up every "deferred to production hardening" item from Phases 1–5 and delivering a Loki build that's safe to run as a multi-tenant service. The preceding phases made Loki *functionally* a server — Phase 6 makes it *operationally* a server. That means real rate limiting instead of a stub, per-subject session ownership instead of flat visibility, Prometheus metrics instead of in-memory counters, structured JSON logging, deployment manifests, security headers, config validation, and operational runbooks.

This is the final phase. After it lands, Loki v1 is production-ready: you can run `loki --serve` in a container behind a reverse proxy, scrape its metrics from Prometheus, route requests through a rate limiter, and have multiple tenants share the same instance without seeing each other's data.

**Estimated effort:** ~1 week
**Risk:** Low. Most of the work is applying well-known patterns (sliding-window rate limiting, row-level authz, Prometheus, structured logging) on top of the architecture the previous phases already built. No new core types, no new pipelines.
**Depends on:** Phases 1–5 complete. The API server runs, MCP pool works, sessions are UUID-keyed.

---

## Why Phase 6 Exists

Phases 4 and 5 got the API server running with correct semantics, but several explicit gaps were called out as "stubs" or "follow-ups." A Phase 4 deployment is usable for a trusted single-tenant context (an internal tool, a personal server) but unsafe for anything else:

- **Anyone with a valid API key can see every session.** Phase 4 flagged this as "single-tenant-per-key." In a multi-tenant deployment where Alice and Bob both have keys, Alice can list Bob's sessions and read their messages. This is a security issue, not a feature gap.
- **No real rate limiting.** Phase 4's `max_concurrent_requests` semaphore caps parallelism but doesn't throttle per-subject request rates. A single runaway client can exhaust the whole concurrency budget.
- **No metrics for external observability.** Phase 5 added in-memory counters, but they're only reachable via the `.info mcp` dot-command or a one-shot JSON endpoint. Production needs Prometheus scraping so alerting and dashboards work.
- **Logs aren't structured.** The `tracing` spans from Phase 4 middleware emit human-readable text. Aggregators like Loki (the other one), Datadog, or CloudWatch want JSON with correlation IDs.
- **No deployment story.** There's no Dockerfile, no systemd unit, no documented way to actually run the thing in production. Every deploying team has to reinvent this.
- **Security headers missing.** Phase 4's CORS handles cross-origin; it doesn't set `X-Content-Type-Options`, `X-Frame-Options`, or similar defaults that a browser-facing endpoint should have.
- **No config validation at startup.** Mistyped config values produce runtime errors hours after deployment instead of failing fast at startup.
- **Operational procedures are undocumented.** How do you rotate auth keys? How do you reload MCP credentials? What's the runbook when the MCP hit rate drops? None of this is written down.

Phase 6 delivers answers to all of the above. It's the "you can actually deploy this" phase.

---

## What Phase 6 Delivers

Grouped by theme rather than by dependency order. Each item is independently valuable and can be worked in parallel.

### Security and isolation

1. **Per-subject session ownership** — every session records the authenticated subject that created it; reads/writes are authz-checked against the caller's subject.
2. **Scope-based authorization** — `AuthContext.scopes` are enforced per endpoint (e.g., `read:sessions`, `write:sessions`, `admin:mcp`). Phase 4's middleware already populates scopes; Phase 6 adds the enforcement.
3. **JWT support** — extends `AuthConfig` with a `Jwt { issuer, audience, jwks_url }` variant that validates tokens against a JWKS endpoint and extracts subject + scopes from claims.
4. **Security headers middleware** — `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin`, optional HSTS when behind HTTPS.
5. **Audit logging** — structured audit events for every authenticated request (subject, action, target, result), written to a dedicated sink so they survive log rotation.

### Throughput and fairness

6. **Per-subject rate limiting** — sliding-window limiter keyed by subject. Enforces `rate_limit_per_minute` and related config. Returns `429 Too Many Requests` with a `Retry-After` header.
7. **Per-subject concurrency limit** — subject-scoped semaphore so one noisy neighbor can't exhaust the global concurrency budget.
8. **Backpressure signal** — expose a `/healthz/ready` endpoint that returns 503 when the server is saturated, so upstream load balancers can drain traffic.

### Observability

9. **Structured JSON logging** — every log line is JSON with `timestamp`, `level`, `target`, `request_id`, `subject`, `session_id`, and `fields`. Routes through `tracing_subscriber` with `fmt::layer().json()`.
10. **Prometheus metrics endpoint** — `/metrics` exposing the existing Phase 5 counters plus new HTTP metrics (`http_requests_total`, `http_request_duration_seconds`, `http_requests_in_flight`), MCP metrics (`mcp_pool_size`, `mcp_acquire_latency_seconds` histogram), and session metrics (`sessions_active_total`, `sessions_created_total`).
11. **Liveness and readiness probes** — `/healthz/live` for process liveness (always 200 unless shutting down), `/healthz/ready` for dependency readiness (config loaded, MCP pool initialized, storage writable).

### Operability

12. **Config validation at startup** — a dedicated `ApiConfig::validate()` that checks every field against a schema and fails fast with a readable error message listing *all* problems, not just the first one.
13. **SIGHUP config reload** — reloads auth keys, log level, and rate limit settings without restarting the server. Does NOT reload MCP pool config (requires restart because the pool holds live subprocesses).
14. **Dockerfile + multi-stage build** — minimal runtime image based on `debian:bookworm-slim` with the compiled binary, config directory, and non-root user.
15. **systemd service unit** — with `Type=notify`, sandboxing directives, and resource limits.
16. **docker-compose example** — for local development with nginx-as-TLS-terminator in front.
17. **Kubernetes manifests** — Deployment, Service, ConfigMap, Secret, HorizontalPodAutoscaler.

### Documentation

18. **Operational runbook** (`docs/RUNBOOK.md`) — documented procedures for common scenarios.
19. **Deployment guide** (`docs/DEPLOYMENT.md`) — end-to-end instructions for each deployment target.
20. **Security guide** (`docs/SECURITY.md`) — threat model, hardening checklist, key rotation procedures.

---

## Core Type Additions

Most of Phase 6 hangs off existing types. A few new concepts need introducing.

### `AuthContext` enrichment

Phase 4 defined `AuthContext { subject: String, scopes: Vec<String> }`. Phase 6 extends it:

```rust
pub struct AuthContext {
    pub subject: String,
    pub scopes: Scopes,
    pub key_id: Option<String>,        // for audit log correlation
    pub claims: Option<JwtClaims>,     // present when auth mode is Jwt
}

pub struct Scopes(HashSet<String>);

impl Scopes {
    pub fn has(&self, scope: &str) -> bool;
    pub fn has_any(&self, required: &[&str]) -> bool;
    pub fn has_all(&self, required: &[&str]) -> bool;
}

pub enum Scope {
    ReadSessions,      // "read:sessions"
    WriteSessions,     // "write:sessions"
    ReadAgents,        // "read:agents"
    RunAgents,         // "run:agents"
    ReadModels,        // "read:models"
    AdminMcp,          // "admin:mcp"
    AdminSessions,     // "admin:sessions" — can see all users' sessions
}
```

The `Scope` enum provides typed constants for the well-known scope strings used in the handlers. Custom scopes (for callers to define their own access tiers) continue to work as raw strings.

### `SessionOwnership` in the session store

The session metadata needs to record who owns each session so reads/writes can be authorized:

```rust
pub struct SessionMeta {
    pub id: SessionId,
    pub alias: Option<SessionAlias>,
    pub owner: Option<String>,         // subject that created it; None = legacy
    pub last_modified: SystemTime,
    pub is_autoname: bool,
}
```

On disk, the ownership field goes into the session's YAML file under a reserved `_meta` block:

```yaml
_meta:
  owner: "alice"
  created_at: "2026-04-10T15:32:11Z"
  created_by_key_id: "key_3f2a..."
# ... rest of session fields unchanged
```

The `SessionStore` trait gets two new methods and an enriched `open` signature:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    // existing methods unchanged except:
    async fn open(
        &self,
        agent: Option<&str>,
        id: SessionId,
        caller: Option<&AuthContext>,  // NEW: for authz check
    ) -> Result<SessionHandle, StoreError>;

    async fn list(
        &self,
        agent: Option<&str>,
        caller: Option<&AuthContext>,  // NEW: for filtering
    ) -> Result<Vec<SessionMeta>, StoreError>;

    // NEW: transfer ownership (e.g., admin reassignment)
    async fn set_owner(
        &self,
        id: SessionId,
        new_owner: Option<String>,
    ) -> Result<(), StoreError>;
}
```

`caller: None` means internal or legacy access (CLI/REPL) — skip authz entirely. `caller: Some(...)` means an API call — enforce ownership.

**Authz rules:**
- Own session: full access.
- Other subject's session: denied unless caller has `admin:sessions` scope.
- Legacy sessions with `owner: None`: accessible to anyone (grandfathered); every mutation attempts to set the owner to the current caller so they get claimed forward.
- `list`: returns only sessions owned by the caller (or all if they have `admin:sessions`).

### `RateLimiter` and `ConcurrencyLimiter`

```rust
pub struct RateLimiter {
    windows: DashMap<String, SlidingWindow>,
    config: RateLimitConfig,
}

struct SlidingWindow {
    bucket_a: AtomicU64,
    bucket_b: AtomicU64,
    last_reset: AtomicU64,
}

pub struct RateLimitConfig {
    pub per_minute: u32,
    pub burst: u32,
}

impl RateLimiter {
    pub fn check(&self, subject: &str) -> Result<(), RateLimitError>;
}

pub struct RateLimitError {
    pub retry_after: Duration,
    pub limit: u32,
    pub remaining: u32,
}

pub struct SubjectConcurrencyLimiter {
    semaphores: DashMap<String, Arc<Semaphore>>,
    per_subject: usize,
}

impl SubjectConcurrencyLimiter {
    pub async fn acquire(&self, subject: &str) -> OwnedSemaphorePermit;
}
```

Both live in `ApiState` and are applied via middleware. Rate limiting runs first (cheap atomic operations), then concurrency acquisition (may block briefly).

### `MetricsRegistry`

```rust
pub struct MetricsRegistry {
    pub http_requests_total: IntCounterVec,
    pub http_request_duration: HistogramVec,
    pub http_requests_in_flight: IntGaugeVec,
    pub sessions_active: IntGauge,
    pub sessions_created_total: IntCounter,
    pub mcp_pool_size: IntGaugeVec,
    pub mcp_acquire_latency: HistogramVec,
    pub mcp_spawns_total: IntCounter,
    pub mcp_idle_evictions_total: IntCounter,
    pub auth_failures_total: IntCounterVec,
    pub rate_limit_rejections_total: IntCounterVec,
}
```

Built on top of the `prometheus` crate. Exposed via `GET /metrics` with the Prometheus text exposition format. The registry bridges Phase 5's atomic counters into the Prometheus types without requiring Phase 5's code to change — Phase 5 keeps its simple counters, and Phase 6 reads them on each scrape to populate the Prometheus gauges.

### `AuditLogger`

```rust
pub struct AuditLogger {
    sink: AuditSink,
}

pub enum AuditSink {
    Stderr,                                 // default
    File { path: PathBuf, rotation: Rotation },
    Syslog { facility: String },
}

pub struct AuditEvent<'a> {
    pub timestamp: OffsetDateTime,
    pub request_id: Uuid,
    pub subject: Option<&'a str>,
    pub action: AuditAction,
    pub target: Option<&'a str>,
    pub result: AuditResult,
    pub details: Option<serde_json::Value>,
}

pub enum AuditAction {
    SessionCreate,
    SessionRead,
    SessionUpdate,
    SessionDelete,
    AgentActivate,
    ToolExecute,
    McpReload,
    ConfigReload,
    AuthFailure,
    RateLimitRejection,
}

pub enum AuditResult {
    Success,
    Denied { reason: String },
    Error { message: String },
}

impl AuditLogger {
    pub fn log(&self, event: AuditEvent<'_>);
}
```

Audit events are emitted from handler middleware after request completion. The audit stream is deliberately separate from the regular tracing logs because audit logs have stricter retention/integrity requirements in regulated environments — you want to be able to pipe them to a WORM storage or SIEM without mixing in debug logs.

---

## Migration Strategy

### Step 1: Per-subject session ownership

The highest-impact security fix. No new deps, no new config — just enriching existing types.

1. Add `owner: Option<String>` and `created_by_key_id: Option<String>` to the session YAML `_meta` block. Serde skip if absent (backward compat for legacy files).
2. Update `SessionStore::create` to record the caller's subject.
3. Update `SessionStore::open` to take `caller: Option<&AuthContext>` and enforce ownership.
4. Update `SessionStore::list` to filter by caller subject (unless caller has `admin:sessions` scope).
5. Add `SessionStore::set_owner` for admin reassignment.
6. Implement the "claim on first mutation" behavior for legacy sessions.
7. Update all API handlers to pass the `AuthContext` through to store calls.
8. Add integration tests: Alice creates a session, Bob tries to read it (403), admin Claire can read it (200), Alice's `list` returns only her own, Claire's `list` with `admin:sessions` scope returns everything.

**Verification:** all new authz tests pass. CLI/REPL tests still pass because they pass `caller: None`.

### Step 2: Scope-based authorization for endpoints

Phase 4's middleware attaches `AuthContext` with a `scopes: Vec<String>` field but handlers don't check it. Phase 6 adds the enforcement.

1. Change `AuthContext.scopes` from `Vec<String>` to a `Scopes(HashSet<String>)` newtype with `has`/`has_any`/`has_all` methods.
2. Define the `Scope` enum with well-known constants.
3. Add a `require_scope` helper and a `#[require_scope("read:sessions")]` proc macro (or a handler-side check if proc macros add too much complexity).
4. Annotate every handler with the required scope(s):
   - `GET /v1/sessions` → `read:sessions`
   - `POST /v1/sessions` → `write:sessions`
   - `GET /v1/sessions/:id` → `read:sessions`
   - `DELETE /v1/sessions/:id` → `write:sessions`
   - `POST /v1/sessions/:id/completions` → `write:sessions` + `run:agents` (if the session has an agent)
   - `POST /v1/rags/:name/rebuild` → `admin:mcp`
   - `GET /v1/agents`, `/v1/roles`, `/v1/rags`, `/v1/models` → `read:agents`, `read:roles`, etc.
   - `/metrics` → `admin:metrics` (or unauthenticated if the endpoint is bound to a private network)
5. Document the scope model in `docs/SECURITY.md`.

**Verification:** per-endpoint authz tests. A key with only `read:sessions` can list and read but not write.

### Step 3: JWT support in `AuthConfig`

Extend the auth mode enum:

```rust
pub enum AuthConfig {
    Disabled,
    StaticKeys { keys: Vec<AuthKeyEntry> },
    Jwt(JwtConfig),
}

pub struct JwtConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_url: String,
    pub jwks_refresh_interval: Duration,
    pub subject_claim: String,        // e.g., "sub"
    pub scopes_claim: String,         // e.g., "scope" or "permissions"
    pub leeway_seconds: u64,
}
```

1. Add `jsonwebtoken` and `reqwest` (already present) to dependencies.
2. Implement a `JwksCache` that fetches `jwks_url` on startup and refreshes every `jwks_refresh_interval`. Uses `reqwest` with a short timeout. Refreshes in the background via `tokio::spawn`.
3. The auth middleware branches on `AuthConfig`: `StaticKeys` continues to work, `Jwt` calls `jsonwebtoken::decode` with the cached JWKS.
4. Extract subject from the configured claim name. Extract scopes from either a space-separated string (`scope` claim) or an array claim (`permissions`).
5. Handle key rotation gracefully: if decoding fails with "unknown key ID," trigger an immediate JWKS refresh (debounced to once per minute) and retry once.
6. Integration tests with a fake JWKS endpoint (use `mockito` or `wiremock`).

**Verification:** valid JWT authenticates; expired JWT rejected; invalid signature rejected; JWKS refresh handles key rotation.

### Step 4: Real rate limiting

Replace the Phase 4 stub with a working sliding-window implementation.

1. Add `dashmap` dependency for the per-subject map (lock-free reads/writes).
2. Implement `SlidingWindow` with two adjacent one-minute buckets; the effective rate is the weighted sum of the current bucket plus the tail of the previous bucket based on how far into the current window we are.
3. Add `RateLimiter::check(subject) -> Result<(), RateLimitError>`.
4. Write middleware that calls `check` before dispatching to handlers. On `Err`, return 429 with `Retry-After` header.
5. Add `rate_limit_per_minute` and `rate_limit_burst` config fields. Reasonable defaults: 60/min, burst 10.
6. Expose per-subject current rate as a gauge in the Prometheus registry.
7. Integration test: fire N+1 requests as the same subject within a minute, assert the N+1th gets 429.

**Verification:** rate limiting works correctly across subjects; non-limited subjects aren't affected; burst allowance works.

### Step 5: Per-subject concurrency limiter

Complements rate limiting — rate limits the *count* of requests over time, concurrency limits the *simultaneous* count.

1. Implement `SubjectConcurrencyLimiter` with a `DashMap<String, Arc<Semaphore>>`.
2. Lazy-init semaphores per subject with `per_subject_concurrency` slots (default 8).
3. Middleware acquires a permit per request. If the subject's semaphore is full, queue briefly (`try_acquire_owned` with a short timeout), then 503 if still full.
4. Garbage-collect unused semaphores periodically (entries with no waiters and full availability count haven't been used).
5. Integration test: fire 10 concurrent requests as one subject with `per_subject_concurrency: 5`, assert at least 5 serialize.

**Verification:** no subject can exceed its concurrency budget; other subjects unaffected.

### Step 6: Prometheus metrics endpoint

1. Add `prometheus` crate.
2. Implement `MetricsRegistry` with the metrics listed in the types section.
3. Wire metric updates into existing code:
   - HTTP middleware: `http_requests_total.inc()` on response, `http_request_duration.observe(elapsed)`, `http_requests_in_flight.inc()/dec()`
   - Session creation: `sessions_created_total.inc()`, `sessions_active.set(store.count())`
   - MCP factory: read the Phase 5 atomic counters on scrape and populate the Prometheus types
4. Add `GET /metrics` handler that writes the Prometheus text exposition format.
5. Auth policy for `/metrics`: configurable — either requires `admin:metrics` scope, or is opened to a private network via `metrics_listen_addr: "127.0.0.1:9090"` on a separate port (recommended).
6. Integration test: scrape `/metrics`, parse the response, assert expected metrics are present with sensible values.

**Verification:** Prometheus scraping works; metrics increment correctly.

### Step 7: Structured JSON logging

Replace the default `tracing_subscriber` format with JSON output.

1. Add a `log_format: Text | Json` config field, default `Text` for CLI/REPL, `Json` for `--serve` mode.
2. Configure `tracing_subscriber::fmt::layer().json()` conditionally.
3. Ensure every span has a `request_id` field (already present from Phase 4 middleware).
4. Add `subject` and `session_id` as span fields when present, so they get included in every child log line automatically.
5. Add a `log_level` config field that SIGHUP reloads at runtime (see Step 12).
6. Integration test: capture stdout during a request, parse as JSON, assert the fields are present and correctly scoped.

**Verification:** `loki --serve` produces one-line-per-event JSON output suitable for log aggregators.

### Step 8: Audit logging

Dedicated sink for security-relevant events.

1. Implement `AuditLogger` with `Stderr`, `File`, and `Syslog` sinks. Start with just `Stderr` and `File` — `Syslog` via `syslog` crate can follow.
2. Emit audit events from:
   - Auth middleware: `AuditAction::AuthFailure` on any auth rejection
   - Rate limiter: `AuditAction::RateLimitRejection` on 429
   - Session handlers: `AuditAction::SessionCreate/Read/Update/Delete`
   - Agent handlers: `AuditAction::AgentActivate`
   - MCP reload endpoint: `AuditAction::McpReload`
3. Audit events are JSON lines with a schema documented in `docs/SECURITY.md`.
4. Audit events don't interfere with the main tracing stream — they go to the configured audit sink independently.
5. File rotation via `tracing-appender` or manual rotation with size + date cap.

**Verification:** every security-relevant action produces an audit event; failures include a `reason`.

### Step 9: Security headers and misc middleware

1. Add a `security_headers` middleware layer that attaches:
   - `X-Content-Type-Options: nosniff`
   - `X-Frame-Options: DENY`
   - `Referrer-Policy: strict-origin-when-cross-origin`
   - `Strict-Transport-Security: max-age=31536000; includeSubDomains` (only when `api.force_https: true`)
   - Do NOT set CSP — this is an API, not a browser app; CSP would confuse clients.
2. Remove `Server: ...` and other fingerprinting headers.
3. Handle `OPTIONS` preflight correctly (Phase 4's CORS layer does this; verify).

**Verification:** `curl -I` inspects headers; automated test asserts each required header is present.

### Step 10: Config validation at startup

A single `ApiConfig::validate()` method that checks every field and aggregates ALL errors before failing.

1. Implement validation for:
   - `listen_addr` is parseable and bindable
   - `auth.mode` has a valid configuration (e.g., `StaticKeys` with non-empty key list, `Jwt` with reachable JWKS URL)
   - `auth.keys[].key_hash` starts with `$argon2id$` (catches plaintext keys)
   - `rate_limit_per_minute > 0` and `burst > 0`
   - `max_body_bytes > 0` and `< 100 MiB` (sanity)
   - `request_timeout_seconds > 0` and `< 3600`
   - `shutdown_grace_seconds >= 0`
   - `cors.allowed_origins` entries are valid URLs or `"*"`
2. Return a `ConfigValidationError` that lists every problem, not just the first.
3. Call `validate()` in `serve()` before binding the listener.
4. Test: a deliberately-broken config produces an error listing all problems.

**Verification:** startup validation catches common mistakes; error message is actionable.

### Step 11: Health check endpoints

1. `GET /healthz/live` — always returns 200 OK unless the process is in graceful shutdown. Body: `{"status":"ok"}`. No auth required.
2. `GET /healthz/ready` — returns 200 OK when fully initialized and not saturated, otherwise 503 Service Unavailable. Readiness criteria:
   - `AppState` fully initialized
   - Session store writable (attempt a probe write to a reserved path)
   - MCP pool initialized (at least the factory is alive)
   - Concurrency semaphore has at least 10% available (not saturated)
3. Both endpoints are unauthenticated and unmetered — load balancers hit them constantly.
4. Document in `docs/DEPLOYMENT.md` how Kubernetes, systemd, and other supervisors should use these.

**Verification:** endpoints return correct status under various load conditions.

### Step 12: SIGHUP config reload

Reload a subset of config without restarting.

1. Reloadable fields:
   - Auth keys (StaticKeys mode)
   - JWT config (including JWKS URL)
   - Log level
   - Rate limit config
   - Per-subject concurrency limits
   - Audit logger sink
2. NOT reloadable (requires full restart):
   - Listen address
   - MCP pool config (pool holds live subprocesses)
   - Session storage paths
   - TLS certs (use a reverse proxy)
3. Implementation: SIGHUP handler that re-reads `config.yaml`, validates it, and atomically swaps the affected fields in `ApiState`. Uses `arc-swap` crate for lock-free swaps.
4. Audit every reload: `AuditAction::ConfigReload` with before/after diff summary.
5. Document: rotation procedures for auth keys, logging level adjustments, etc.

**Verification:** start server, modify `config.yaml`, send SIGHUP, assert new config is in effect without dropped requests.

### Step 13: Deployment manifests

#### 13a. Dockerfile

Multi-stage build for a minimal runtime image:

```dockerfile
# Build stage
FROM rust:1.82-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY assets ./assets
RUN cargo build --release --bin loki

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    tini \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --home /loki --shell /bin/false loki
COPY --from=builder /build/target/release/loki /usr/local/bin/loki
COPY --from=builder /build/assets /opt/loki/assets
USER loki
WORKDIR /loki
ENV LOKI_CONFIG_DIR=/loki/config
EXPOSE 3400
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/loki", "--serve"]
```

Build args for targeting specific architectures. Result is a ~100 MB image.

#### 13b. systemd unit

```ini
[Unit]
Description=Loki AI Server
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
ExecStart=/usr/local/bin/loki --serve
Restart=on-failure
RestartSec=5
User=loki
Group=loki

# Sandboxing
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/loki
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true

# Resource limits
LimitNOFILE=65536
LimitNPROC=512
MemoryMax=4G

# Reload
ExecReload=/bin/kill -HUP $MAINPID

[Install]
WantedBy=multi-user.target
```

`Type=notify` requires Loki to call `sd_notify(READY=1)` after successful startup — add this with the `sd-notify` crate.

#### 13c. docker-compose example

For local development with TLS via nginx:

```yaml
version: "3.9"
services:
  loki:
    build: ..
    environment:
      LOKI_CONFIG_DIR: /loki/config
    volumes:
      - ./config:/loki/config:ro
      - loki_data:/loki/data
    ports:
      - "127.0.0.1:3400:3400"
    restart: unless-stopped
    healthcheck:
      test: [ "CMD", "curl", "-f", "http://localhost:3400/healthz/live" ]
      interval: 30s
      timeout: 5s
      retries: 3

  nginx:
    image: nginx:alpine
    volumes:
      - ./deploy/nginx.conf:/etc/nginx/nginx.conf:ro
      - ./deploy/certs:/etc/nginx/certs:ro
    ports:
      - "443:443"
    depends_on:
      - loki

volumes:
  loki_data:
```

Include a sample `nginx.conf` that terminates TLS and forwards to `loki:3400`.

#### 13d. Kubernetes manifests

Provide `deploy/k8s/` with:
- `namespace.yaml`
- `deployment.yaml` (3 replicas, resource requests/limits, liveness/readiness probes)
- `service.yaml` (ClusterIP)
- `configmap.yaml` (non-secret config)
- `secret.yaml` (API keys, JWT config)
- `hpa.yaml` (HorizontalPodAutoscaler based on CPU + custom metric for requests/sec)
- `ingress.yaml` (optional example using nginx-ingress)

Document storage strategy: sessions use a PVC mounted at `/loki/data`; RAG embeddings use a read-only ConfigMap or a separate PVC.

**Verification:** each deployment target produces a running Loki that passes health checks.

### Step 14: Operational runbook

Write `docs/RUNBOOK.md` with sections for:

- **Starting and stopping** the server
- **Rotating auth keys** (StaticKeys mode) — edit config, SIGHUP, verify in audit log
- **Rotating auth keys** (Jwt mode) — update JWKS at issuer, Loki auto-refreshes
- **Rotating MCP credentials** — update env vars, `POST /v1/mcp/reload` (new endpoint in this phase) or restart
- **Diagnosing high latency** — check MCP hit rate, check LLM provider latency, check concurrency saturation
- **Diagnosing auth failures** — audit log `AuthFailure` events, check key hash, check JWKS reachability
- **Diagnosing rate limit rejections** — check per-subject counter, adjust limit or identify runaway client
- **Diagnosing orphaned MCP subprocesses** — `ps aux | grep loki`, check logs for `McpFactory shutdown complete`
- **Diagnosing session corruption** — check `.yaml.tmp` files (should not exist when server is idle), inspect session YAML for validity
- **Backup and restore** — tar the `sessions/` and `agents/` directories
- **Scaling horizontally** — each replica has its own MCP pool and session store; share sessions via shared filesystem (NFS/EFS) or deferred to a database-backed SessionStore (not in this phase)
- **Incident response** — what logs to collect, what metrics to snapshot, how to reach a minimal reproducing state

**Verification:** walk through each procedure on a test deployment; fix any unclear steps.

### Step 15: Deployment and security guides

`docs/DEPLOYMENT.md` — step-by-step for Docker, systemd, docker-compose, Kubernetes. Pre-flight checklist, first-time setup, upgrade procedure.

`docs/SECURITY.md` — threat model, hardening checklist, scope model, audit event schema, key rotation, reverse proxy configuration, network security recommendations, CVE reporting contact.

Cross-reference from `README.md` and add a "Production Deployment" section to the README that points to both docs.

**Verification:** a developer unfamiliar with Loki can deploy it successfully using only the docs.

---

## Risks and Watch Items

| Risk | Severity | Mitigation |
|---|---|---|
| **Session ownership migration breaks legacy users** | Medium | Legacy sessions with `owner: None` stay readable by anyone; they get claimed forward on first mutation. Document this in `RUNBOOK.md`. Add a one-shot migration CLI command (`loki migrate sessions --claim-to <subject>`) that assigns ownership of all unowned sessions to a specific subject. |
| **JWT JWKS fetch failures block startup** | Medium | JWKS URL must be reachable at startup; if it's not, log an error and fall back to "reject all" mode until the fetch succeeds. A retry loop with exponential backoff runs in the background. Do NOT crash on JWKS failure. |
| **Rate limiter DashMap growth** | Low | Per-subject windows accumulate forever without cleanup. Add a background reaper that removes entries with zero recent activity every few minutes. Cap total entries at 100k as a safety valve. |
| **Prometheus metric cardinality explosion** | Low | `http_requests_total` with per-path labels could explode if routes have dynamic segments (`/v1/sessions/:id`). Use route templates as labels, not concrete paths. Validate label sets at registration. |
| **Audit log retention compliance** | Low | Audit logs might need to be retained for regulatory reasons. Phase 6 provides the emission; retention is the operator's responsibility. Document this in `SECURITY.md`. |
| **SIGHUP reload partial failure** | Medium | If the new config is invalid, don't swap it in — keep the old config running. Log the validation error. The operator can fix the file and SIGHUP again. Never leave the server in an inconsistent state. |
| **Docker image size** | Low | `debian:bookworm-slim` is ~80 MB; final image ~100 MB. If smaller is needed, use `distroless/cc-debian12` for a ~35 MB image at the cost of not having `tini` or debugging tools. Document both options. |
| **systemd Type=notify missing implementation** | Medium | Adding `sd_notify` requires the `sd-notify` crate AND calling it after listener bind. Missing this call makes systemd think the service failed. Add an integration test that fakes systemd and asserts the notification is sent. |
| **Kubernetes pod disruption** | Low | HPA scales down during low traffic, but in-flight requests on the terminating pod must complete gracefully. Set `terminationGracePeriodSeconds` to at least `shutdown_grace_seconds + 10`. Document in `DEPLOYMENT.md`. |
| **Running under a reverse proxy** | Low | CORS, `Host` header handling, `X-Forwarded-For` for rate limiter subject identification. Document the expected proxy config (trust `X-Forwarded-*` headers only from trusted proxies). |

---

## What Phase 6 Does NOT Do

- **No multi-region replication.** Loki is a single-instance service; scale out by running multiple instances behind a load balancer, each with its own pool. Cross-instance state sharing is not in scope.
- **No database-backed session store.** `FileSessionStore` is still the only implementation. A `PostgresSessionStore` is a clean extension point (`SessionStore` trait is already there) but belongs to a follow-up.
- **No cluster coordination.** Each Loki instance is independent. Running Loki in a "cluster" mode where instances share work is a separate project.
- **No advanced ML observability.** LLM call costs, token usage trends, provider error rates — these are tracked as counters but not aggregated into dashboards. Follow-up work.
- **No built-in TLS termination.** Use a reverse proxy (nginx, Caddy, Traefik, a cloud load balancer). Supporting TLS in-process adds complexity and key management concerns that reverse proxies solve better.
- **No SAML or LDAP.** Only StaticKeys and JWT. SAML/LDAP integration can extend `AuthConfig` later.
- **No plugin system.** Extensions to auth, storage, or middleware require forking and rebuilding. A dynamic plugin loader is explicitly out of scope.
- **No multi-tenancy beyond session ownership.** Tenants share the same process, same MCP pool, same RAG cache, same resources. Strict tenant isolation (separate processes per tenant) requires orchestration outside Loki.
- **No cost accounting per tenant.** LLM API calls are tracked per-subject in audit logs but not aggregated into billing-grade cost reports.

---

## Entry Criteria (from Phase 5)

- [ ] `McpFactory` pooling works and has metrics
- [ ] Graceful shutdown drains the MCP pool
- [ ] Phase 5 load test passes (hit rate >0.8, no orphaned subprocesses)
- [ ] Phase 4 API integration test suite passes
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean

## Exit Criteria (Phase 6 complete — v1 ready)

- [ ] Per-subject session ownership enforced; integration tests prove Alice can't read Bob's sessions
- [ ] Scope-based authorization enforced on every endpoint
- [ ] JWT authentication works with a real JWKS endpoint
- [ ] Real rate limiting replaces the Phase 4 stub; 429 responses include `Retry-After`
- [ ] Per-subject concurrency limiter prevents noisy-neighbor saturation
- [ ] Prometheus `/metrics` endpoint scrapes cleanly
- [ ] Structured JSON logs emitted in `--serve` mode
- [ ] Audit events written for all security-relevant actions
- [ ] Security headers set on all responses
- [ ] Config validation fails fast at startup with readable errors
- [ ] `/healthz/live` and `/healthz/ready` endpoints work
- [ ] SIGHUP reloads auth keys, log level, and rate limits without restart
- [ ] Dockerfile produces a minimal runtime image
- [ ] systemd unit with `Type=notify` works correctly
- [ ] docker-compose example runs end-to-end with TLS via nginx
- [ ] Kubernetes manifests deploy successfully
- [ ] `docs/RUNBOOK.md` covers all common operational scenarios
- [ ] `docs/DEPLOYMENT.md` guides a first-time deployer to success
- [ ] `docs/SECURITY.md` documents threat model, scopes, and hardening
- [ ] `cargo check`, `cargo test`, `cargo clippy` all clean
- [ ] End-to-end production smoke test: deploy to Kubernetes, send real traffic, scrape metrics, rotate a key, induce a failure, observe recovery

---

## v1 Release Summary

After Phase 6 lands, Loki v1 has transformed from a single-user CLI tool into a production-ready multi-tenant AI service. Here's what the v1 release notes should say:

**New in Loki v1:**

- **REST API** — full HTTP surface for completions, sessions, agents, roles, RAGs, and metadata. Streaming via Server-Sent Events, synchronous via JSON.
- **Multi-tenant sessions** — UUID-primary identity with optional human-readable aliases. Per-subject ownership with scope-based access control.
- **Concurrent safety** — per-session mutex serialization, per-MCP-server Arc sharing, per-agent runtime isolation. Run dozens of concurrent requests without corruption.
- **MCP pooling** — recently-used MCP subprocesses stay warm across requests. Near-zero warm-path latency. Configurable idle timeout and LRU cap.
- **Authentication** — static API keys or JWT with JWKS. Argon2-hashed credentials. Scope-based authorization per endpoint.
- **Observability** — Prometheus metrics, structured JSON logging with correlation IDs, dedicated audit log stream.
- **Rate limiting** — sliding-window per subject with configurable limits and burst allowance.
- **Graceful shutdown** — in-flight requests complete within a grace period; MCP subprocesses terminate cleanly; session state is persisted.
- **Deployment manifests** — Dockerfile, systemd unit, docker-compose example, Kubernetes manifests.
- **Full documentation** — runbook, deployment guide, security guide, API reference.

**Backward compatibility:**

CLI and REPL continue to work identically to pre-v1 builds. Existing `config.yaml`, `roles/`, `sessions/`, `agents/`, `rags/`, and `functions/` directories are read-compatible. The legacy session layout is migrated lazily on first access without destroying the old files.

**What's next (v2+):**

- Database-backed session store for cross-instance sharing
- Native TLS termination option
- SAML / LDAP authentication extensions
- Per-tenant cost accounting and quotas
- Dynamic plugin system for custom auth, storage, and middleware
- Multi-region replication
- WebSocket transport alongside SSE
