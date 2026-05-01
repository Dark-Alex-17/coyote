# Iteration 4 — Test Implementation Notes

## Plan file addressed

`docs/testing/plans/04-agents.md`

## Tests created

### src/config/agent.rs (4 new tests)

| Test name | What it verifies |
|---|---|
| `agent_config_parses_from_yaml` | Full AgentConfig YAML with all fields |
| `agent_config_defaults` | Minimal AgentConfig gets correct defaults |
| `agent_config_with_model` | model_id, temperature, top_p from YAML |
| `agent_config_inject_defaults_true` | inject_todo/spawn_instructions default true |

### src/config/agent_runtime.rs (2 new tests)

| Test name | What it verifies |
|---|---|
| `agent_runtime_new_defaults` | All fields default correctly |
| `agent_runtime_builder_pattern` | with_depth, with_parent_supervisor work |

### src/config/request_context.rs (6 new tests, 17 total)

| Test name | What it verifies |
|---|---|
| `exit_agent_clears_all_agent_state` | exit_agent clears agent, agent_runtime, rag |
| `current_depth_returns_zero_without_agent` | Default depth is 0 |
| `current_depth_returns_agent_runtime_depth` | Depth from agent_runtime |
| `supervisor_returns_none_without_agent` | No agent → no supervisor |
| `inbox_returns_none_without_agent` | No agent → no inbox |
| `root_escalation_queue_returns_none_without_agent` | No agent → no queue |

**Total: 12 new tests (105 → 117)**

## Bugs discovered

None.

## Observations for future iterations

1. `Agent::init` can't be unit tested easily — requires agent config
   files, tool files on disk. Integration tests with temp directories
   would be needed for full coverage.

2. AgentConfig default values verified:
   - `max_concurrent_agents` = 4
   - `max_agent_depth` = 3
   - `max_auto_continues` = 10
   - `inject_todo_instructions` = true
   - `inject_spawn_instructions` = true
   These are important behavioral contracts.

3. The `exit_agent` test shows that clearing agent state also
   rebuilds the tool_scope with fresh functions. This is the
   correct behavior for returning to the global context.

4. Agent variable interpolation (special vars like __os__, __cwd__)
   happens in Agent::init which is filesystem-dependent. Deferred.

5. `list_agents()` (which filters hidden dirs) is tested via the
   `.shared` exclusion noted in improvements. Could add a unit test
   with a temp dir if needed.

## Next iteration

Plan file 05: MCP Lifecycle — the most critical test area. McpFactory,
McpRuntime, spawn_mcp_server, rebuild_tool_scope MCP integration,
scope transition MCP behavior.
