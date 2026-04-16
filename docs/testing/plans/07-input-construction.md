# Test Plan: Input Construction

## Feature description

`Input` encapsulates a single chat turn's data: text, files, role,
model, session context, RAG embeddings, and function declarations.
It's constructed at the start of each turn and captures all needed
state from `RequestContext`.

## Behaviors to test

### Input::from_str
- [ ] Creates Input from text string
- [ ] Captures role via resolve_role
- [ ] Captures session from ctx
- [ ] Captures rag from ctx
- [ ] Captures functions via select_functions
- [ ] Captures stream_enabled from AppConfig
- [ ] app_config field set from ctx.app.config
- [ ] Empty text → is_empty() returns true

### Input::from_files
- [ ] Loads file contents
- [ ] Supports multiple files
- [ ] Supports directories (recursive)
- [ ] Supports URLs (fetches content)
- [ ] Supports loader syntax (e.g., jina:url)
- [ ] Last message carry-over (%% syntax)
- [ ] Combines file content with text
- [ ] document_loaders from AppConfig used

### resolve_role
- [ ] Returns provided role if given
- [ ] Extracts role from agent if agent active
- [ ] Extracts role from session if session has role
- [ ] Returns default model-based role otherwise
- [ ] with_session flag set correctly
- [ ] with_agent flag set correctly

### Input methods
- [ ] stream() returns stream_enabled && !model.no_stream()
- [ ] create_client() uses app_config to init client
- [ ] prepare_completion_data() uses captured functions
- [ ] build_messages() uses captured session
- [ ] echo_messages() uses captured session
- [ ] set_regenerate(role) refreshes role
- [ ] use_embeddings() searches RAG if present
- [ ] merge_tool_results() creates continuation input

## Context switching scenarios
- [ ] Input with agent → agent functions selected
- [ ] Input with MCP → MCP meta functions in declarations
- [ ] Input with RAG → embeddings included after use_embeddings
- [ ] Input without session → no session messages in build_messages

## Old code reference
- `src/config/input.rs` — Input struct, from_str, from_files
- `src/config/mod.rs` — select_functions, extract_role
