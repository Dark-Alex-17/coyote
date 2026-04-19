# Test Plan: RequestContext

## Feature description

`RequestContext` is the per-request mutable state container. It holds
the active model, role, session, agent, RAG, tool scope, and agent
runtime. It provides methods for scope transitions, state queries,
and chat completion lifecycle.

## Behaviors to test

### State management
- [ ] info() returns formatted system info
- [ ] state() returns correct StateFlags combination
- [ ] current_model() returns active model
- [ ] role_info(), session_info(), rag_info(), agent_info() format correctly
- [ ] sysinfo() returns system details
- [ ] working_mode correctly distinguishes Repl vs Cmd

### Scope transitions
- [ ] use_role changes role, rebuilds tool scope
- [ ] use_session creates/loads session, rebuilds tool scope
- [ ] use_agent initializes agent with all subsystems
- [ ] exit_role clears role
- [ ] exit_session saves and clears session
- [ ] exit_agent clears agent, supervisor, rag, session
- [ ] exit_rag clears rag
- [ ] bootstrap_tools rebuilds tool scope with global MCP

### Chat completion lifecycle
- [ ] before_chat_completion sets up for API call
- [ ] after_chat_completion saves messages, updates state
- [ ] discontinuous_last_message marks last message as non-continuous

### ToolScope management
- [ ] rebuild_tool_scope creates fresh Functions
- [ ] rebuild_tool_scope acquires MCP servers via factory
- [ ] rebuild_tool_scope appends user interaction functions in REPL mode
- [ ] rebuild_tool_scope appends MCP meta functions for started servers
- [ ] Tool tracker preserved across scope rebuilds

### AgentRuntime management
- [ ] agent_runtime populated by use_agent
- [ ] agent_runtime cleared by exit_agent
- [ ] Accessor methods (current_depth, supervisor, inbox, etc.) return
      correct values when agent active
- [ ] Accessor methods return defaults when no agent

### Settings update
- [ ] update() handles all .set keys correctly
- [ ] update_app_config() clones and replaces Arc properly
- [ ] delete() handles all delete subcommands

### Session helpers
- [ ] list_sessions() returns session names
- [ ] list_autoname_sessions() returns auto-named sessions
- [ ] session_file() returns correct path
- [ ] save_session() persists session
- [ ] empty_session() clears messages

## Context switching scenarios
- [ ] No state → use_role → exit_role → no state
- [ ] No state → use_agent → exit_agent → no state
- [ ] Role → use_agent (error: agent requires exiting role first)
- [ ] Agent → exit_agent → use_role (clean transition)

## Old code reference
- `src/config/request_context.rs` — all methods
- `src/config/mod.rs` — original Config methods (for parity)
