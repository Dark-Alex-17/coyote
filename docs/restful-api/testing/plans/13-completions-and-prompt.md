# Test Plan: Tab Completion and Prompt

## Behaviors to test

### Tab completion (repl_complete)
- [ ] .role<TAB> → role names (no hidden files)
- [ ] .agent<TAB> → agent names (no .shared)
- [ ] .session<TAB> → session names
- [ ] .rag<TAB> → RAG names
- [ ] .macro<TAB> → macro names
- [ ] .model<TAB> → model names with descriptions
- [ ] .set <TAB> → setting keys (sorted)
- [ ] .set temperature <TAB> → current value suggestions
- [ ] .set enabled_tools <TAB> → tool names (no internal tools)
- [ ] .set enabled_mcp_servers <TAB> → configured servers + aliases
- [ ] .delete <TAB> → type names
- [ ] .vault <TAB> → subcommands
- [ ] .agent <name> <TAB> → session names for that agent
- [ ] Fuzzy filtering applied to all completions

### Prompt rendering
- [ ] Left prompt shows role/session/agent name
- [ ] Right prompt shows model name
- [ ] Prompt updates after scope transitions
- [ ] Multi-line indicator shown during ::: input

## Status
Most completion logic requires filesystem access for role/session/agent lists.
The `split_line` function has existing tests. Prompt rendering methods are trivial
wrappers around stored strings. Low additional unit-test yield.

## Old code reference
- `src/config/request_context.rs` — repl_complete
- `src/repl/completer.rs` — ReplCompleter (split_line already tested)
- `src/repl/prompt.rs` — ReplPrompt
