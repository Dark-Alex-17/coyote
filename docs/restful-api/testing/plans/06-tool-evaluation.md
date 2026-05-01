# Test Plan: Tool Evaluation

## Feature description

When the LLM returns tool calls, `eval_tool_calls` dispatches each
call to the appropriate handler. Handlers include: shell tools
(bash/python/ts scripts), MCP tools, supervisor tools (agent spawn),
todo tools, and user interaction tools.

## Behaviors to test

### eval_tool_calls dispatch
- [ ] Calls dispatched to correct handler by function name prefix (requires RequestContext)
- [ ] Tool results returned for each call (requires RequestContext)
- [ ] Multiple concurrent tool calls processed (requires RequestContext)
- [x] Tool call tracker updated (chain length, repeats)
- [ ] Root agent (depth 0) checks escalation queue after eval (requires RequestContext)
- [ ] Escalation notifications injected into results (requires RequestContext)

### ToolCall::eval routing
- [ ] agent__* → handle_supervisor_tool (requires RequestContext)
- [ ] todo__* → handle_todo_tool (requires RequestContext)
- [ ] user__* → handle_user_tool (depth 0) or escalate (depth > 0) (requires RequestContext)
- [ ] mcp_invoke_* → invoke_mcp_tool (requires RequestContext + live MCP)
- [ ] mcp_search_* → search_mcp_tools (requires RequestContext + live MCP)
- [ ] mcp_describe_* → describe_mcp_tool (requires RequestContext + live MCP)
- [ ] Other → shell tool execution (requires RequestContext + binary)

### Shell tool execution
- [ ] Tool binary found and executed (integration test)
- [ ] Arguments passed correctly (integration test)
- [ ] Environment variables set (LLM_OUTPUT, etc.) (integration test)
- [ ] Tool output returned as result (integration test)
- [ ] Tool failure → error returned as tool result (not panic) (integration test)

### Tool call tracking
- [x] Tracker counts consecutive identical calls
- [x] Max repeats triggers warning
- [x] Chain length tracked across turns
- [x] Tracker state preserved across tool-result loops

### Function selection
- [ ] select_functions filters by role's enabled_tools (requires filesystem)
- [x] select_functions includes MCP meta functions for enabled servers
- [x] select_functions includes agent functions when agent active (via append tests)
- [ ] "all" enables all functions (requires filesystem)
- [ ] Comma-separated list enables specific functions (requires filesystem)

## Context switching scenarios
- [ ] Tool calls during agent → agent tools available (integration test)
- [ ] Tool calls during role → role tools available (integration test)
- [ ] Tool calls with MCP → MCP invoke/search/describe work (integration test)
- [x] No agent → no agent__/todo__ tools in declarations (via Functions::default)

## Additional behaviors tested (not in original plan)

- [x] ToolCall::new sets name, arguments, id correctly
- [x] ToolCall::default has empty/null fields
- [x] ToolCall::with_thought_signature sets and clears
- [x] ToolCall::dedup keeps last occurrence for duplicate ids
- [x] ToolCall::dedup keeps all calls without ids
- [x] ToolCall::dedup empty input returns empty
- [x] ToolCall::dedup mixed with/without ids
- [x] ToolCallTracker default values (max_repeats=2, chain_len=3)
- [x] ToolCallTracker no loop on fresh tracker
- [x] ToolCallTracker no loop below threshold
- [x] ToolCallTracker different args breaks loop
- [x] ToolCallTracker different names breaks loop
- [x] ToolCallTracker record_call respects capacity
- [x] ToolCallTracker loop message includes call_history
- [x] All 6 prefix constants verified
- [x] Functions::append_todo adds all 5 todo tools
- [x] Functions::append_supervisor adds spawn/check/collect/list/cancel/reply + task queue
- [x] Functions::append_teammate adds send_message/check_inbox
- [x] Functions::append_user_interaction adds ask/confirm/input/checkbox
- [x] Functions::append_mcp_meta creates 3 per server with correct schemas
- [x] Functions::append_mcp_meta empty servers → no declarations
- [x] Functions::find/contains work correctly
- [x] ToolResult::new stores call and output

## Old code reference
- `src/function/mod.rs` — eval_tool_calls, ToolCall::eval
- `src/function/supervisor.rs` — handle_supervisor_tool
- `src/function/todo.rs` — handle_todo_tool
- `src/function/user_interaction.rs` — handle_user_tool
