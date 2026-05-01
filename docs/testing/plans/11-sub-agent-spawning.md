# Test Plan: Sub-Agent Spawning

## Feature description

Agents with can_spawn_agents=true can spawn child agents that run
in parallel as background tokio tasks. Children communicate results
back to the parent via collect/check. Escalation allows children
to request user input through the parent.

## Behaviors to test

### Spawn
- [ ] agent__spawn creates child agent in background (requires agent config on disk)
- [x] Child gets own RequestContext with incremented depth (new_for_child)
- [x] Child starts with empty scope (new_for_child)
- [x] Child gets shared root_escalation_queue (new_for_child)
- [x] Child gets inbox for teammate messaging (new_for_child)
- [x] Child inherits parent_supervisor (new_for_child)
- [ ] Child MCP servers acquired if configured (requires live MCP)
- [x] Max concurrent agents enforced (Supervisor.register)
- [x] Max depth enforced (Supervisor.register)
- [ ] Agent not found → error (requires agent config on disk)
- [ ] can_spawn_agents=false → no spawn tools available (requires agent init)

### Collect/Check
- [x] agent__check returns PENDING for running agent
- [x] agent__check returns error for unknown agent
- [ ] agent__collect blocks until done, returns output (requires real child completion)
- [ ] Output summarization when exceeds threshold (requires LLM client)
- [ ] Summarization uses configured model (requires LLM client)

### Task queue (handler integration tests)
- [x] handle_task_create creates tasks (simple, with deps, with dispatch_agent)
- [x] handle_task_create errors when agent set without prompt
- [x] handle_task_complete unblocks dependents
- [x] handle_task_list shows all tasks
- [x] handle_task_fail marks failed and reports blocked dependents
- [x] handle_task_fail returns error for missing task

### Escalation (handler integration tests)
- [x] handle_reply_escalation delivers reply via oneshot channel
- [x] handle_reply_escalation errors for missing escalation_id
- [x] handle_reply_escalation errors when no queue
- [x] Pending summary contains correct fields
- [x] Reply reaches receiver via oneshot channel
- [ ] Escalation timeout → fallback message (requires tokio timeout)

### Teammate messaging (handler integration tests)
- [x] handle_send_message delivers to registered agent's inbox
- [x] handle_send_message errors for unknown agent
- [x] handle_check_inbox returns messages with count
- [x] handle_check_inbox returns empty when no inbox
- [x] handle_check_inbox returns empty for empty inbox

### Cancel/List (handler integration tests)
- [x] handle_list returns empty for fresh supervisor
- [x] handle_list returns registered agents
- [x] handle_list errors when no supervisor
- [x] handle_cancel removes agent and signals abort
- [x] handle_cancel errors for unknown agent
- [x] handle_cancel errors when no supervisor

### Dispatch routing
- [x] Unknown action → error with "Unknown supervisor action"
- [x] agent__list routes to handle_list
- [x] agent__task_list routes to handle_task_list

### Child agent lifecycle
- [ ] run_child_agent loops (requires LLM client)
- [ ] Child uses before/after_chat_completion (requires LLM client)
- [ ] Child tool calls evaluated (requires LLM client)
- [ ] Child exits cleanly (requires LLM client)

## Context switching scenarios
- [ ] Parent spawns child with MCP (requires live MCP + agent config)
- [ ] Parent exits agent → all children cancelled (requires agent init)
- [x] Multiple children share escalation queue (new_for_child + ensure_root_escalation_queue)

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

## Integration handler tests added

- [x] All handle_* functions tested via handler integration tests (36 tests)
- [x] new_for_child: depth, id, inbox, escalation queue, parent supervisor, empty scope
- [x] ensure_root_escalation_queue: lazy init, same Arc on repeated calls
- [x] AppState::test_default() helper added for cross-module test construction

## Old code reference
- `src/function/supervisor.rs` — all handler functions
- `src/supervisor/` — Supervisor, EscalationQueue, Inbox, TaskQueue
