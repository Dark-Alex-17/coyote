# Test Plan: RequestContext

## Feature description

`RequestContext` is the per-request mutable state container. It holds
the active model, role, session, agent, RAG, tool scope, and agent
runtime. It provides methods for scope transitions, state queries,
and chat completion lifecycle.

## Behaviors to test

### State management
- [ ] info() returns formatted system info (requires model provider config)
- [x] state() returns correct StateFlags combination
- [ ] current_model() returns active model (tested implicitly via extract_role)
- [x] role_info() errors when no role, succeeds with role
- [ ] session_info() format (requires filesystem for sessions)
- [x] rag_info() errors when no rag
- [x] agent_info() errors when no agent
- [ ] sysinfo() returns system details (requires model provider config)
- [x] working_mode correctly distinguishes Repl vs Cmd

### Scope transitions
- [x] use_role changes role (via use_role_obj)
- [ ] use_session creates/loads session, rebuilds tool scope (async + filesystem)
- [x] use_agent initializes agent with all subsystems (via exit_agent test)
- [x] exit_role clears role
- [x] exit_session saves and clears session
- [x] exit_agent clears agent, supervisor, rag, session
- [x] exit_rag clears rag
- [ ] bootstrap_tools rebuilds tool scope with global MCP (async + MCP servers)

### Chat completion lifecycle
- [x] before_chat_completion sets up for API call
- [ ] after_chat_completion saves messages, updates state (async + client)
- [x] discontinuous_last_message marks last message as non-continuous

### ToolScope management
- [x] rebuild_tool_scope creates fresh Functions
- [ ] rebuild_tool_scope acquires MCP servers via factory (requires live MCP)
- [x] rebuild_tool_scope appends user interaction functions in REPL mode
- [ ] rebuild_tool_scope appends MCP meta functions for started servers (requires live MCP)
- [x] Tool tracker preserved across scope rebuilds

### AgentRuntime management
- [x] agent_runtime populated by use_agent (via exit_agent test)
- [x] agent_runtime cleared by exit_agent
- [x] Accessor methods (current_depth, supervisor, inbox, etc.) return
      correct values when agent active
- [x] Accessor methods return defaults when no agent

### Settings update
- [ ] update() handles all .set keys correctly (requires REPL command infra)
- [x] update_app_config() clones and replaces Arc properly
- [ ] delete() handles all delete subcommands (requires REPL command infra)

### Session helpers
- [ ] list_sessions() returns session names (requires filesystem)
- [ ] list_autoname_sessions() returns auto-named sessions (requires filesystem)
- [x] session_file() returns correct path
- [ ] save_session() persists session (requires filesystem)
- [x] empty_session() clears messages

## Context switching scenarios
- [x] No state → use_role → exit_role → no state
- [x] No state → use_agent → exit_agent → no state
- [x] Agent active → use_role_obj errors
- [ ] Agent → exit_agent → use_role (clean transition) (async)

## Additional behaviors tested (not in original plan)

- [x] state() empty context returns empty flags
- [x] state() role only → ROLE flag
- [x] state() empty session → SESSION_EMPTY flag
- [x] state() role + session flags combine
- [x] discontinuous_last_message noop when no last_message
- [x] before_chat_completion creates LastMessage with empty output and continuous=true
- [x] role_like_mut returns None when no active scope
- [x] role_like_mut returns role when only role active
- [x] role_like_mut prefers session over role
- [x] session_file handles subdir/name format
- [x] is_compressing_session false with no session
- [x] is_compressing_session false with default session

## Old code reference
- `src/config/request_context.rs` — all methods
- `src/config/mod.rs` — original Config methods (for parity)
