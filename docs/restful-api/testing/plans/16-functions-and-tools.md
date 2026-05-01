# Test Plan: Functions and Tools

## Behaviors to test

### Function declarations
- [ ] Functions::init loads from visible_tools config
- [ ] Tool declarations parsed from bash scripts (argc annotations)
- [ ] Tool declarations parsed from python scripts (docstrings)
- [ ] Tool declarations parsed from typescript (JSDoc + type inference)
- [ ] Each declaration has name, description, parameters
- [ ] Agent tools loaded via Functions::init_agent
- [ ] Global tools loaded via build_global_tool_declarations

### Tool compilation
- [ ] Bash tools compiled to bin directory
- [ ] Python tools compiled to bin directory
- [ ] TypeScript tools compiled to bin directory
- [ ] clear_agent_bin_dir removes old binaries
- [ ] Tool file priority: .sh > .py > .ts > .js

### User interaction functions
- [ ] append_user_interaction_functions adds user__ask/confirm/input/checkbox
- [ ] Only appended in REPL mode
- [ ] User interaction tools work at depth 0 (direct prompt)
- [ ] User interaction tools escalate at depth > 0

### MCP meta functions
- [ ] append_mcp_meta_functions adds invoke/search/describe per server
- [ ] Meta functions removed when ToolScope rebuilt without those servers
- [ ] Function names follow mcp_invoke_<server> pattern

### Function selection
- [ ] select_functions filters by role's enabled_tools
- [ ] "all" enables everything
- [ ] Specific tool names enabled selectively
- [ ] mapping_tools aliases resolved
- [ ] Agent functions included when agent active
- [ ] MCP meta functions included when servers active

## Status
- Function declarations, append methods, find/contains tested in iteration 6
- MCP meta functions tested in iterations 5-7
- Function selection tested in iteration 7
- User interaction functions tested in iterations 6-7
- Python parser: extensive existing tests (400+ lines)
- TypeScript parser: extensive existing tests (400+ lines)
- parsers::common::underscore tested in iteration 13
- Functions::init and tool compilation require filesystem

## Additional behaviors tested

- [x] parsers::common::underscore: simple, dashes, spaces, special chars, consecutive, leading/trailing, uppercase, mixed

## Old code reference
- `src/function/mod.rs` — Functions struct, init, init_agent
- `src/config/paths.rs` — agent_functions_file (priority)
- `src/parsers/` — bash, python, typescript parsers
