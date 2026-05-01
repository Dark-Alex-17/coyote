# Test Plan: MCP Server Lifecycle

## Feature description

MCP (Model Context Protocol) servers are external tools that run
as subprocesses communicating via stdio. Loki manages their lifecycle
through McpFactory (start/share via Weak dedup) and McpRuntime
(per-scope active server handles). Servers are started/stopped
during scope transitions (role/session/agent enter/exit).

## Behaviors to test

### MCP config loading
- [x] mcp.json parsed correctly from functions directory
- [x] Server specs include command, args, env, cwd
- [ ] Vault secrets interpolated in mcp.json
- [ ] Missing secrets reported as warnings
- [x] McpServersConfig stored on AppState.mcp_config

### McpFactory
- [ ] acquire() spawns new server when none active (requires real subprocess)
- [ ] acquire() returns existing handle via Weak upgrade (requires real subprocess)
- [ ] acquire() spawns fresh when Weak is dead (requires real subprocess)
- [ ] Multiple acquire() calls for same spec share handle (requires real subprocess)
- [x] Different specs get different handles (via key inequality)
- [x] McpServerKey built correctly from spec (sorted args/env)

### McpRuntime
- [ ] insert() adds server handle by name (requires Arc<ConnectedServer>)
- [ ] get() retrieves handle by name (requires Arc<ConnectedServer>)
- [x] server_names() returns all active names
- [x] is_empty() correct for empty/non-empty
- [ ] search() finds tools by keyword (BM25 ranking) (requires live server)
- [ ] describe() returns tool input schema (requires live server)
- [ ] invoke() calls tool on server and returns result (requires live server)

### spawn_mcp_server
- [ ] Builds Command from spec (command, args, env, cwd) (integration test)
- [ ] Creates TokioChildProcess transport (integration test)
- [ ] Completes rmcp handshake (serve) (integration test)
- [ ] Returns Arc<ConnectedServer> (integration test)
- [ ] Log file created when log_path provided (integration test)

### rebuild_tool_scope (MCP integration)
- [x] Empty enabled_mcp_servers → no servers acquired
- [ ] "all" → all configured servers acquired (requires real subprocess)
- [ ] Comma-separated list → only listed servers acquired (requires real subprocess)
- [ ] Mapping resolution: alias → actual server key(s) (requires real subprocess)
- [ ] MCP meta functions appended for each started server (requires real subprocess)
- [ ] Old ToolScope dropped (releasing old server handles) (requires real subprocess)
- [ ] Loading spinner shown during acquisition (UI test)
- [ ] AbortSignal properly threaded through (integration test)

### Server lifecycle during scope transitions
- [ ] Enter role with MCP: servers start (integration test)
- [ ] Exit role: servers stop (handle dropped) (integration test)
- [ ] Enter role A (MCP-X) → exit → enter role B (MCP-Y):
      X stops, Y starts (integration test)
- [ ] Enter role with MCP → exit to no MCP: servers stop,
      global MCP restored (integration test)
- [ ] Start REPL with global MCP → enter agent with different MCP:
      agent MCP takes over (integration test)
- [ ] Exit agent: agent MCP stops, global MCP restored (integration test)

### MCP tool invocation chain
- [ ] LLM calls mcp__search_<server> → search results returned (integration test)
- [ ] LLM calls mcp__describe_<server> tool_name → schema returned (integration test)
- [ ] LLM calls mcp__invoke_<server> tool args → tool executed (integration test)
- [ ] Server not found → "MCP server not found in runtime" error (tested via McpRuntime.get)
- [ ] Tool not found → appropriate error (requires live server)

### MCP support flag
- [x] mcp_server_support=false → no MCP servers started
- [ ] mcp_server_support=false + agent with MCP → error (blocks) (requires agent init)
- [ ] mcp_server_support=false + role with MCP → warning, continues (requires role init)
- [ ] .set mcp_server_support true → MCP servers start (requires live server)

### MCP in child agents
- [ ] Child agent MCP servers acquired via factory (integration test)
- [ ] Child agent MCP runtime populated (integration test)
- [ ] Child agent MCP tool invocations work (integration test)
- [ ] Child agent exit drops MCP handles (integration test)

## Context switching scenarios (comprehensive)
- [ ] No MCP → role with MCP → exit role → no MCP (integration test)
- [ ] Global MCP-A → role MCP-B → exit role → global MCP-A (integration test)
- [ ] Global MCP-A → agent MCP-B → exit agent → global MCP-A (integration test)
- [ ] Role MCP-A → session MCP-B (overrides) → exit session (integration test)
- [ ] Agent MCP → child agent MCP → child exits → parent MCP intact (integration test)
- [ ] .set enabled_mcp_servers X → .set enabled_mcp_servers Y:
      X released, Y acquired (integration test)
- [ ] .set enabled_mcp_servers null → all released (integration test)

## Additional behaviors tested (not in original plan)

- [x] McpServerKey equality: same spec → equal keys
- [x] McpServerKey inequality: different names → different keys
- [x] McpServerKey inequality: different commands → different keys
- [x] McpServerKey env coercion: Bool/Int → String
- [x] McpFactory default has empty active map
- [x] McpServer::is_remote() true for Http/Sse, false for Stdio
- [x] McpServer::validate() all cross-field conflicts (6 cases)
- [x] McpServersConfig: empty servers map, multiple servers, cwd field
- [x] McpRegistry: default state, config accessor
- [x] McpRegistry: resolve with whitespace trimming
- [x] McpRegistry: resolve all-nonexistent returns empty
- [x] rebuild_tool_scope: no mcp_config yields empty runtime
- [x] rebuild_tool_scope: preserves tool_tracker across rebuild
- [x] rebuild_tool_scope: REPL mode appends user interaction functions
- [x] rebuild_tool_scope: CMD mode excludes user interaction functions
- [x] MCP meta function name prefix constants are correct
- [x] ToolScope default: empty functions, runtime, tracker

## Old code reference
- `src/mcp/mod.rs` — McpRegistry, init, reinit, start/stop
- `src/config/mcp_factory.rs` — McpFactory, acquire, McpServerKey
- `src/config/tool_scope.rs` — ToolScope, McpRuntime
- `src/config/request_context.rs` — rebuild_tool_scope, bootstrap_tools
