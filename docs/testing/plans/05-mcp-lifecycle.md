# Test Plan: MCP Server Lifecycle

## Feature description

MCP (Model Context Protocol) servers are external tools that run
as subprocesses communicating via stdio. Loki manages their lifecycle
through McpFactory (start/share via Weak dedup) and McpRuntime
(per-scope active server handles). Servers are started/stopped
during scope transitions (role/session/agent enter/exit).

## Behaviors to test

### MCP config loading
- [ ] mcp.json parsed correctly from functions directory
- [ ] Server specs include command, args, env, cwd
- [ ] Vault secrets interpolated in mcp.json
- [ ] Missing secrets reported as warnings
- [ ] McpServersConfig stored on AppState.mcp_config

### McpFactory
- [ ] acquire() spawns new server when none active
- [ ] acquire() returns existing handle via Weak upgrade
- [ ] acquire() spawns fresh when Weak is dead
- [ ] Multiple acquire() calls for same spec share handle
- [ ] Different specs get different handles
- [ ] McpServerKey built correctly from spec (sorted args/env)

### McpRuntime
- [ ] insert() adds server handle by name
- [ ] get() retrieves handle by name
- [ ] server_names() returns all active names
- [ ] is_empty() correct for empty/non-empty
- [ ] search() finds tools by keyword (BM25 ranking)
- [ ] describe() returns tool input schema
- [ ] invoke() calls tool on server and returns result

### spawn_mcp_server
- [ ] Builds Command from spec (command, args, env, cwd)
- [ ] Creates TokioChildProcess transport
- [ ] Completes rmcp handshake (serve)
- [ ] Returns Arc<ConnectedServer>
- [ ] Log file created when log_path provided

### rebuild_tool_scope (MCP integration)
- [ ] Empty enabled_mcp_servers → no servers acquired
- [ ] "all" → all configured servers acquired
- [ ] Comma-separated list → only listed servers acquired
- [ ] Mapping resolution: alias → actual server key(s)
- [ ] MCP meta functions appended for each started server
- [ ] Old ToolScope dropped (releasing old server handles)
- [ ] Loading spinner shown during acquisition
- [ ] AbortSignal properly threaded through

### Server lifecycle during scope transitions
- [ ] Enter role with MCP: servers start
- [ ] Exit role: servers stop (handle dropped)
- [ ] Enter role A (MCP-X) → exit → enter role B (MCP-Y):
      X stops, Y starts
- [ ] Enter role with MCP → exit to no MCP: servers stop,
      global MCP restored
- [ ] Start REPL with global MCP → enter agent with different MCP:
      agent MCP takes over
- [ ] Exit agent: agent MCP stops, global MCP restored

### MCP tool invocation chain
- [ ] LLM calls mcp__search_<server> → search results returned
- [ ] LLM calls mcp__describe_<server> tool_name → schema returned
- [ ] LLM calls mcp__invoke_<server> tool args → tool executed
- [ ] Server not found → "MCP server not found in runtime" error
- [ ] Tool not found → appropriate error

### MCP support flag
- [ ] mcp_server_support=false → no MCP servers started
- [ ] mcp_server_support=false + agent with MCP → error (blocks)
- [ ] mcp_server_support=false + role with MCP → warning, continues
- [ ] .set mcp_server_support true → MCP servers start

### MCP in child agents
- [ ] Child agent MCP servers acquired via factory
- [ ] Child agent MCP runtime populated
- [ ] Child agent MCP tool invocations work
- [ ] Child agent exit drops MCP handles

## Context switching scenarios (comprehensive)
- [ ] No MCP → role with MCP → exit role → no MCP
- [ ] Global MCP-A → role MCP-B → exit role → global MCP-A
- [ ] Global MCP-A → agent MCP-B → exit agent → global MCP-A
- [ ] Role MCP-A → session MCP-B (overrides) → exit session
- [ ] Agent MCP → child agent MCP → child exits → parent MCP intact
- [ ] .set enabled_mcp_servers X → .set enabled_mcp_servers Y:
      X released, Y acquired
- [ ] .set enabled_mcp_servers null → all released

## Old code reference
- `src/mcp/mod.rs` — McpRegistry, init, reinit, start/stop
- `src/config/mcp_factory.rs` — McpFactory, acquire, McpServerKey
- `src/config/tool_scope.rs` — ToolScope, McpRuntime
- `src/config/request_context.rs` — rebuild_tool_scope, bootstrap_tools
