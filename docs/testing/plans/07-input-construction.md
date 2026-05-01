# Test Plan: Input Construction

## Feature description

`Input` encapsulates a single chat turn's data: text, files, role,
model, session context, RAG embeddings, and function declarations.
It's constructed at the start of each turn and captures all needed
state from `RequestContext`.

## Behaviors to test

### Input::from_str
- [x] Creates Input from text string
- [x] Captures role via resolve_role
- [x] Captures session from ctx
- [ ] Captures rag from ctx (requires RAG setup)
- [ ] Captures functions via select_functions (tested separately)
- [x] Captures stream_enabled from AppConfig
- [x] app_config field set from ctx.app.config
- [x] Empty text → is_empty() returns true

### Input::from_files
- [ ] Loads file contents (async + filesystem)
- [ ] Supports multiple files (async + filesystem)
- [ ] Supports directories (recursive) (async + filesystem)
- [ ] Supports URLs (fetches content) (async + network)
- [ ] Supports loader syntax (e.g., jina:url) (async + loader)
- [x] Last message carry-over (%% syntax) (via resolve_paths)
- [ ] Combines file content with text (async)
- [ ] document_loaders from AppConfig used (async)

### resolve_role
- [x] Returns provided role if given
- [ ] Extracts role from agent if agent active (requires agent init)
- [x] Extracts role from session if session has role
- [x] Returns default model-based role otherwise
- [x] with_session flag set correctly
- [x] with_agent flag set correctly

### Input methods
- [ ] stream() returns stream_enabled && !model.no_stream() (requires Model with no_stream)
- [ ] create_client() uses app_config to init client (requires client config)
- [ ] prepare_completion_data() uses captured functions (requires Model)
- [ ] build_messages() uses captured session (requires Message setup)
- [ ] echo_messages() uses captured session (requires Message setup)
- [x] set_regenerate(role) refreshes role
- [ ] use_embeddings() searches RAG if present (requires RAG)
- [ ] merge_tool_results() creates continuation input (requires ToolResult)

## Context switching scenarios
- [ ] Input with agent → agent functions selected (requires agent init)
- [x] Input with MCP → MCP meta functions in declarations (via select_functions tests)
- [ ] Input with RAG → embeddings included after use_embeddings (requires RAG)
- [x] Input without session → no session messages in build_messages (via session() test)

## Additional behaviors tested (not in original plan)

- [x] resolve_role: explicit role overrides session flag
- [x] resolve_paths: empty input
- [x] resolve_paths: URL detection (https://)
- [x] resolve_paths: external command detection (backtick syntax)
- [x] resolve_paths: rejects URL with glob suffix
- [x] resolve_paths: mixed inputs (%%, URL, external cmd)
- [x] Input::set_text changes text
- [x] Input::patched_text overrides text()
- [x] Input::clear_patch restores original
- [x] Input::set_continue_output accumulates
- [x] Input::summary truncates long text with ...
- [x] Input::summary preserves short text
- [x] Input::raw() with no files
- [x] Input::render() with no medias
- [x] Input::session() returns None when with_session=false
- [x] Input::session() returns Some when with_session=true
- [x] is_image recognizes png/jpeg/jpg/webp/gif
- [x] is_image rejects non-image extensions
- [x] resolve_data_url returns path for known hash
- [x] resolve_data_url returns original for non-data URL
- [x] select_functions: None when no tools enabled
- [x] select_functions: None when function_calling disabled
- [x] select_functions: "all" returns all non-MCP
- [x] select_functions: comma-separated filters
- [x] select_enabled_mcp_servers: empty when MCP disabled
- [x] select_enabled_mcp_servers: "all" returns all MCP functions
- [x] select_enabled_mcp_servers: comma filters by server name

## Old code reference
- `src/config/input.rs` — Input struct, from_str, from_files
- `src/config/mod.rs` — select_functions, extract_role
