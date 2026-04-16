# Test Plan: Sub-Agent Spawning

## Feature description

Agents with can_spawn_agents=true can spawn child agents that run
in parallel as background tokio tasks. Children communicate results
back to the parent via collect/check. Escalation allows children
to request user input through the parent.

## Behaviors to test

### Spawn
- [ ] agent__spawn creates child agent in background
- [ ] Child gets own RequestContext with incremented depth
- [ ] Child gets own session, model, functions
- [ ] Child gets shared root_escalation_queue
- [ ] Child gets inbox for teammate messaging
- [ ] Child MCP servers acquired if configured
- [ ] Max concurrent agents enforced
- [ ] Max depth enforced
- [ ] Agent not found → error
- [ ] can_spawn_agents=false → no spawn tools available

### Collect/Check
- [ ] agent__check returns PENDING or result
- [ ] agent__collect blocks until done, returns output
- [ ] Output summarization when exceeds threshold
- [ ] Summarization uses configured model

### Task queue
- [ ] agent__task_create creates tasks with dependencies
- [ ] agent__task_complete marks done, unblocks dependents
- [ ] Auto-dispatch spawns agent for unblocked tasks
- [ ] agent__task_list shows all tasks with status

### Escalation
- [ ] Child calls user__ask → escalation created
- [ ] Parent sees pending_escalations notification
- [ ] agent__reply_escalation unblocks child
- [ ] Escalation timeout → fallback message

### Teammate messaging
- [ ] agent__send_message delivers to sibling inbox
- [ ] agent__check_inbox drains messages

### Child agent lifecycle
- [ ] run_child_agent loops: create input → call completions → process results
- [ ] Child uses before/after_chat_completion
- [ ] Child tool calls evaluated via eval_tool_calls
- [ ] Child exits cleanly, supervisor cancels on completion

## Context switching scenarios
- [ ] Parent spawns child with MCP → child MCP works independently
- [ ] Parent exits agent → all children cancelled
- [ ] Multiple children share escalation queue correctly

## Old code reference
- `src/function/supervisor.rs` — all handler functions
- `src/supervisor/` — Supervisor, EscalationQueue, Inbox, TaskQueue
