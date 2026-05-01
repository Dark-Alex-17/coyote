# Test Plan: Sub-Agent Spawning

## Feature description

Agents with can_spawn_agents=true can spawn child agents that run
in parallel as background tokio tasks. Children communicate results
back to the parent via collect/check. Escalation allows children
to request user input through the parent.

## Behaviors to test

### Spawn
- [ ] agent__spawn creates child agent in background (integration)
- [ ] Child gets own RequestContext with incremented depth (integration)
- [ ] Child gets own session, model, functions (integration)
- [ ] Child gets shared root_escalation_queue (integration)
- [ ] Child gets inbox for teammate messaging (integration)
- [ ] Child MCP servers acquired if configured (integration)
- [x] Max concurrent agents enforced (Supervisor.register)
- [x] Max depth enforced (Supervisor.register)
- [ ] Agent not found → error (integration)
- [ ] can_spawn_agents=false → no spawn tools available (integration)

### Collect/Check
- [ ] agent__check returns PENDING or result (integration)
- [ ] agent__collect blocks until done, returns output (integration)
- [ ] Output summarization when exceeds threshold (integration)
- [ ] Summarization uses configured model (integration)

### Task queue
- [x] agent__task_create creates tasks with dependencies
- [x] agent__task_complete marks done, unblocks dependents
- [x] Auto-dispatch agent/prompt stored on task
- [x] agent__task_list shows all tasks with status

### Escalation
- [x] Escalation submitted and retrievable
- [x] Pending summary contains correct fields
- [x] Reply reaches receiver via oneshot channel
- [ ] Escalation timeout → fallback message (integration)

### Teammate messaging
- [x] Deliver to inbox
- [x] Drain empties inbox
- [x] Drain ordering: shutdown > task_completed > text

### Child agent lifecycle
- [ ] run_child_agent loops (integration)
- [ ] Child uses before/after_chat_completion (integration)
- [ ] Child tool calls evaluated (integration)
- [ ] Child exits cleanly (integration)

## Context switching scenarios
- [ ] Parent spawns child with MCP (integration)
- [ ] Parent exits agent → all children cancelled (integration)
- [ ] Multiple children share escalation queue correctly (integration)

## Additional behaviors tested (not in original plan)

- [x] EscalationQueue: default, submit, take, take_nonexistent, has_pending
- [x] EscalationQueue: pending_summary with/without options, empty
- [x] EscalationQueue: reply via oneshot channel
- [x] new_escalation_id: prefix and uniqueness
- [x] Inbox: new/default empty, deliver+drain, drain empties, multiple deliveries
- [x] Inbox: clone preserves messages, clone is independent
- [x] Supervisor: new defaults, register count, take removes, take nonexistent
- [x] Supervisor: inbox accessor, list_agents, task_queue accessible
- [x] Supervisor: register allows at max_depth boundary
- [x] AgentExitStatus: equality/inequality
- [x] TaskQueue: fail sets status, get missing returns None
- [x] TaskQueue: dispatch_agent/prompt stored, claim blocked fails
- [x] TaskQueue: list sorted by id, default empty
- [x] TaskQueue: dependency on nonexistent errors, complete nonexistent
- [x] TaskNode: is_runnable when pending+unblocked, not when blocked

## Old code reference
- `src/function/supervisor.rs` — all handler functions
- `src/supervisor/` — Supervisor, EscalationQueue, Inbox, TaskQueue
