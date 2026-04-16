# Test Plan: Roles

## Feature description

Roles define a system prompt + optional model/temperature/MCP config
that customizes LLM behavior. Roles can be built-in or user-defined
(markdown files). Roles are "role-likes" — sessions and agents also
implement the RoleLike trait.

## Behaviors to test

### Role loading
- [x] Built-in roles load correctly (shell, code)
- [ ] User-defined roles load from markdown files (requires filesystem)
- [x] Role parses model_id from metadata
- [x] Role parses temperature, top_p from metadata
- [x] Role parses enabled_tools from metadata
- [x] Role parses enabled_mcp_servers from metadata
- [ ] Role with no model_id inherits current model (requires retrieve_role + client config)
- [ ] Role with no temperature inherits from AppConfig (requires retrieve_role)
- [ ] Role with no top_p inherits from AppConfig (requires retrieve_role)

### retrieve_role
- [ ] Retrieves by name from file system
- [ ] Resolves model via Model::retrieve_model
- [ ] Falls back to current model if role has no model_id
- [ ] Sets temperature/top_p from AppConfig when role doesn't specify

### use_role (scope transition)
- [x] Sets role on RequestContext (use_role_obj_sets_role)
- [ ] Triggers rebuild_tool_scope (async, deferred to plan 05/08)
- [ ] MCP servers start if role has enabled_mcp_servers (deferred to plan 05)
- [ ] MCP meta functions added to function list (deferred to plan 05)
- [ ] Previous role cleared when switching (deferred to plan 08)
- [x] Role-like temperature/top_p take effect (role_set_temperature_works)

### exit_role
- [x] Clears role from RequestContext (exit_role_clears_role)
- [ ] Followed by bootstrap_tools to restore global tool scope (async, deferred)
- [ ] MCP servers from role are stopped (deferred to plan 05)
- [ ] Global MCP servers restored (deferred to plan 05)

### use_prompt (temp role)
- [x] Creates a TEMP_ROLE_NAME role with the prompt text (use_prompt_creates_temp_role)
- [x] Uses current model
- [x] Activates via use_role_obj

### extract_role
- [ ] Returns role from agent if agent active (deferred to plan 04)
- [ ] Returns role from session if session active with role (deferred to plan 03)
- [x] Returns standalone role if active (extract_role_returns_standalone_role)
- [x] Returns default role if none active (extract_role_returns_default_when_nothing_active)

### One-shot role messages (REPL)
- [ ] `.role coder write hello` sends message with role, then exits role
- [ ] Original state restored after one-shot

## Context switching scenarios
- [ ] Role → different role: old role replaced, MCP swapped
- [ ] Role → session: role cleared, session takes over
- [ ] Role with MCP → exit: MCP servers stop, global MCP restored
- [ ] No MCP → role with MCP: servers start
- [ ] Role with MCP → role without MCP: servers stop

## Old code reference
- `src/config/mod.rs` — `use_role`, `exit_role`, `retrieve_role`
- `src/config/role.rs` — `Role` struct, parsing
- `src/config/request_context.rs` — `use_role`, `exit_role`, `use_prompt`, `retrieve_role`
