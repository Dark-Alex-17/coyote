# Test Plan: Tool Evaluation

## Feature description

When the LLM returns tool calls, `eval_tool_calls` dispatches each
call to the appropriate handler. Handlers include: shell tools
(bash/python/ts scripts), MCP tools, supervisor tools (agent spawn),
todo tools, and user interaction tools.

## Behaviors to test

### eval_tool_calls dispatch
- [ ] Calls dispatched to correct handler by function name prefix
- [ ] Tool results returned for each call
- [ ] Multiple concurrent tool calls processed
- [ ] Tool call tracker updated (chain length, repeats)
- [ ] Root agent (depth 0) checks escalation queue after eval
- [ ] Escalation notifications injected into results

### ToolCall::eval routing
- [ ] agent__* → handle_supervisor_tool
- [ ] todo__* → handle_todo_tool
- [ ] user__* → handle_user_tool (depth 0) or escalate (depth > 0)
- [ ] mcp_invoke_* → invoke_mcp_tool
- [ ] mcp_search_* → search_mcp_tools
- [ ] mcp_describe_* → describe_mcp_tool
- [ ] Other → shell tool execution

### Shell tool execution
- [ ] Tool binary found and executed
- [ ] Arguments passed correctly
- [ ] Environment variables set (LLM_OUTPUT, etc.)
- [ ] Tool output returned as result
- [ ] Tool failure → error returned as tool result (not panic)

### Tool call tracking
- [ ] Tracker counts consecutive identical calls
- [ ] Max repeats triggers warning
- [ ] Chain length tracked across turns
- [ ] Tracker state preserved across tool-result loops

### Function selection
- [ ] select_functions filters by role's enabled_tools
- [ ] select_functions includes MCP meta functions for enabled servers
- [ ] select_functions includes agent functions when agent active
- [ ] "all" enables all functions
- [ ] Comma-separated list enables specific functions

## Context switching scenarios
- [ ] Tool calls during agent → agent tools available
- [ ] Tool calls during role → role tools available
- [ ] Tool calls with MCP → MCP invoke/search/describe work
- [ ] No agent → no agent__/todo__ tools in declarations

## Old code reference
- `src/function/mod.rs` — eval_tool_calls, ToolCall::eval
- `src/function/supervisor.rs` — handle_supervisor_tool
- `src/function/todo.rs` — handle_todo_tool
- `src/function/user_interaction.rs` — handle_user_tool
