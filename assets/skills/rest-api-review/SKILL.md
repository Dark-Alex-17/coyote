---
description: Review the API surface contract of a change - REST/HTTP routes and handlers, request/response types, OpenAPI specs, plus gRPC services and GraphQL schemas/resolvers when the diff is that flavor. Checks pagination, recorded auth decisions, HTTP method semantics, error-shape consistency, boundary validation, and versioned contract evolution. Load when a diff touches HTTP route/handler definitions, request/response types, OpenAPI specs, proto files, or GraphQL schemas/resolvers. Findings fold into the standard code-review severity taxonomy. Grants read-only filesystem access for tracing routes, types, and published contracts.
enabled_tools: fs_read, fs_grep, fs_glob, fs_cat, fs_ls
---
You are reviewing an API surface. The generic correctness checklist asks "does this handler work?"; you ask **"does this endpoint honor the contract its callers already depend on — and did anyone record the decisions callers will need?"** An API is a promise: clients you cannot see build against the shapes, semantics, and error formats you ship. Most API pain in production is not broken logic — it is a silently changed shape, an unbounded list that grew, or an error format that differs from every sibling endpoint.

## When to load this skill

The diff touches ANY of: HTTP route or handler definitions, request/response types, OpenAPI/Swagger specs, gRPC `.proto` files or service implementations, GraphQL schemas or resolvers. If the diff is internal logic behind an unchanged API surface — unload; this checklist has nothing for you.

## Marker semantics

Every checklist item below carries a severity emoji AND a `[convention]` or `[correctness]` marker; both ride in the finding title so downstream tooling can act on them mechanically. `[convention]` findings are rigor-foldable (the orchestrator may lower them under a relaxed quality bar) and rejectable — but ONLY with cited evidence: a repo convention at file:line, or a recorded plan decision. `[correctness]` is reserved for contract breaks; those findings are neither foldable nor rejectable.

## Linters and mechanized checks

The review orchestrator runs mechanized checks (spec linters, schema diff tools, breaking-change detectors); your CONTEXT may already include their output — do not re-derive it. Spend your prose on what linters cannot reach: whether pagination is warranted, whether the auth decision was recorded, whether an evolution is actually compatible for real callers. If the repo plausibly warrants a linter config it lacks (an OpenAPI spec but no spec linter, protos but no breaking-change check), emit a 🟢 `[convention]` finding naming the gap.

## The checklist (REST/HTTP)

Severities below are the production bar. Each item is a context-sensitive question, not an absolute — read the surrounding code and the callers before flagging.

### 1. 🟡 `[convention]` Unbounded collection endpoint without pagination

Does the diff add or modify an endpoint that returns a collection? If the collection can grow without bound (rows in a table, user-generated items), missing pagination is a finding: name the endpoint, the backing query, and the growth vector. A provably bounded small set is exempt — an enum-backed list, a fixed config table, a per-user set with a hard cap — and when you rely on that exemption, say so explicitly in your notes so the next reviewer sees it was considered, not missed.

### 2. 🟡 `[convention]` Route without a recorded authn/z decision

For every new or changed route: is there a recorded decision that this route is public, authenticated, or role-gated? "Recorded" means visible in code or spec — middleware attached, an annotation, a spec `security` block, or an explicit comment for deliberately public routes. A route with no discernible decision is the finding. Whether the auth implementation is *bypassable* is not your question — that belongs to `security-review`; you only verify the decision exists and is stated.

### 3. 🟡 `[convention]` Non-idempotent PUT/DELETE semantics

PUT and DELETE carry idempotency promises by HTTP contract: repeating a PUT must converge on the same state; repeating a DELETE must not error in a way that breaks retrying clients (a second DELETE returning 404 or 204 is fine; returning 500 is not). Does the diff's handler honor the method it is mounted on — or should it be a POST? You check the *declared method semantics*; whether the state change is mechanically idempotent under retries and concurrency belongs to `transactional-integrity`.

### 4. 🟡/🟢 `[convention]` Error responses leaking internals or inconsistent error shape

Read the error paths: do responses leak internals — stack traces, SQL fragments, internal hostnames, framework default error pages (🟡)? Do they match the error shape the repo's sibling endpoints already return — same envelope, same code/message fields (🟢 when merely inconsistent)? `fs_grep` a sibling handler's error response to establish the house shape before flagging. Whether a leak is *exploitable* is `security-review`'s call; yours is the contract and consistency question.

### 5. 🔴 `[correctness]` Breaking a published request/response shape without versioning

Does the diff remove or rename a field, change a type, tighten accepted input, or change status codes on an endpoint that is already published (in a released spec, consumed by known clients, or exposed beyond this repo)? That is a contract break — a 🔴 `[correctness]` finding unless the change ships behind a new version (path version, header version, or an additive evolution that old clients tolerate). Verify "published" before firing: an endpoint added earlier in this same unreleased branch is not published, and changing it freely is fine.

### 6. 🟡 `[convention]` Missing boundary input validation

At the request boundary, is input validated at all — types enforced, required fields checked, sizes/ranges bounded — before it flows inward? Absence of any validation on a new input path is the finding. Whether unvalidated input is *exploitable* (injection, traversal) is deferred to `security-review`; you flag the missing guardrail, not the attack.

## gRPC section (apply when the diff touches protos or gRPC services)

- 🟡 `[convention]` **Deadlines** — do new client calls set deadlines, and do servers propagate the caller's deadline to their own outbound calls? A call chain with no deadline anywhere hangs forever on a stuck dependency.
- 🟡 `[convention]` **Status-code discipline** — do handlers return meaningful gRPC status codes (`NOT_FOUND`, `INVALID_ARGUMENT`, `ALREADY_EXISTS`) rather than collapsing every failure into `UNKNOWN`/`INTERNAL`? Callers branch on these codes; a flattened code space breaks their error handling.
- 🔴 `[correctness]` **Backwards-compatible proto evolution** — on a published proto: no field-number reuse, no type changes on existing fields, no renumbering, new fields optional with fresh numbers. Violating any of these silently corrupts data for old clients — same contract-break bar as item 5.
- 🟢 `[convention]` **Field deprecation** — removed fields should be `reserved` (number and name) and deprecations marked with the `deprecated` option, not deleted outright, so the number can never be reused.

## GraphQL section (apply when the diff touches schemas or resolvers)

- 🟡 `[convention]` **Resolver N+1** — does a new list-field resolver fetch per-item (a query inside a loop, or a per-parent resolver hitting the DB)? Look for a dataloader/batching layer; its absence on a list path is the finding.
- 🟡 `[convention]` **Depth/complexity limits** — if the diff grows the schema's reachable graph (new nested relations), is there a depth or complexity limit configured anywhere? An unlimited schema is a self-service DoS invitation; check the server setup before assuming.
- 🟡 `[convention]` **Connection-style pagination** — list fields over unbounded collections should use the repo's established pagination idiom (connections/edges or equivalent). The same bounded-set exemption as REST item 1 applies — and state it when you use it. Breaking a published schema field without a deprecation cycle falls under item 5's 🔴 `[correctness]` bar.

## Ground-truth discipline

- READ the route registration and middleware chain, not just the handler — auth decisions and pagination defaults often live up-stack.
- `fs_grep` for the spec file (OpenAPI, proto, GraphQL schema) that publishes the shape the diff changes; the spec, not the struct, is the contract.
- Check sibling endpoints for the house error shape, pagination idiom, and auth annotation style before flagging deviation — the strongest finding cites the sibling at file:line.

## What this skill does NOT check

- Whether auth is bypassable, input is exploitable, or errors leak abusable secrets → `security-review`.
- Whether state-changing handlers are mechanically idempotent, atomic, or retry-safe → `transactional-integrity`.
- Log lines, levels, and message conventions in handlers → `logging-discipline`.
- Metrics, alerts, and dashboards for new endpoints → `observability-review`.
