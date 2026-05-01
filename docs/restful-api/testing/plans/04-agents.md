# Test Plan: Agents

## Feature description

Agents combine a role (instructions), tools (bash/python/ts scripts),
optional RAG, optional MCP servers, and optional sub-agent spawning
capability. Agent::init compiles tools, resolves model, loads RAG,
and sets up the agent environment.

## Behaviors to test

### Agent initialization
- [ ] Agent::init loads config.yaml from agent directory
- [ ] Agent tools compiled from tools.sh / tools.py / tools.ts
- [ ] Tool file priority: .sh > .py > .ts > .js
- [ ] Global tools loaded (from global_tools config)
- [ ] Model resolved from agent config or defaults to current
- [ ] Agent with no model_id uses current model
- [ ] Temperature/top_p from agent config applied
- [ ] Dynamic instructions (_instructions function) invoked if configured
- [ ] Static instructions loaded from config
- [ ] Agent variables interpolated into instructions
- [ ] Special variables (__os__, __cwd__, __now__, etc.) interpolated
- [ ] Agent .env file loaded if present
- [ ] Built-in agents installed on first run (skip if exists)

### Agent tools
- [ ] Agent-specific tools available as function declarations
- [ ] Global tools (from global_tools) also available
- [ ] Tool binaries built in agent bin directory
- [ ] clear_agent_bin_dir removes old binaries before rebuild
- [ ] Tool declarations include name, description, parameters

### Agent with MCP
- [ ] MCP servers listed in agent config started
- [ ] MCP meta functions (invoke/search/describe) added
- [ ] Agent with MCP but mcp_server_support=false → error
- [ ] MCP servers stopped on agent exit

### Agent with RAG
- [ ] RAG documents loaded from agent config
- [ ] RAG available during agent conversation
- [ ] RAG search results included in context

### Agent sessions
- [ ] Agent session started (temp or named)
- [ ] agent_session config used if no explicit session
- [ ] Agent session variables initialized

### Agent lifecycle
- [ ] use_agent checks function_calling_support
- [ ] use_agent errors if agent already active
- [ ] exit_agent clears agent, session, rag, supervisor
- [ ] exit_agent restores global tool scope

### Auto-continuation
- [ ] Agents with auto_continue=true continue after incomplete todos
- [ ] max_auto_continues limits continuation attempts
- [ ] Continuation prompt sent with todo state
- [ ] clear todo stops continuation

### Conversation starters
- [ ] Starters loaded from agent config
- [ ] .starter lists available starters
- [ ] .starter <n> sends the starter as a message

## Context switching scenarios
- [ ] Agent → exit: tools cleared, MCP stopped, session ended
- [ ] Agent with MCP → exit: MCP servers released, global MCP restored
- [ ] Already in agent → start agent: error
- [ ] Agent with RAG → exit: RAG cleared

## Old code reference
- `src/config/agent.rs` — Agent::init, agent config parsing
- `src/config/mod.rs` — use_agent, exit_agent
- `src/config/request_context.rs` — use_agent, exit_agent
- `src/function/mod.rs` — Functions::init_agent, tool compilation
