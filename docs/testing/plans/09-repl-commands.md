# Test Plan: REPL Commands

## Feature description

The REPL processes dot-commands (`.role`, `.session`, `.agent`, etc.)
and plain text (chat messages). Each command has state assertions
(e.g., `.info role` requires an active role).

## Behaviors to test

### Command parsing
- [ ] Dot-commands parsed correctly (command + args)
- [ ] Multi-line input (:::) handled
- [ ] Plain text treated as chat message
- [ ] Empty input ignored

### State assertions (REPL_COMMANDS array)
- [ ] Each command's assert_state enforced correctly
- [ ] Invalid state → command rejected with appropriate error
- [ ] Commands with AssertState::pass() always available

### Command handlers (each one)
- [ ] .help — prints help text
- [ ] .info [subcommand] — displays appropriate info
- [ ] .model <name> — switches model
- [ ] .prompt <text> — sets temp role
- [ ] .role <name> [text] — enters role or one-shot
- [ ] .session [name] — starts/resumes session
- [ ] .agent <name> [session] [key=value] — starts agent
- [ ] .rag [name] — initializes RAG
- [ ] .starter [n] — lists or executes conversation starter
- [ ] .set <key> <value> — updates setting
- [ ] .delete <type> — deletes item
- [ ] .exit [type] — exits scope or REPL
- [ ] .save role/session [name] — saves to file
- [ ] .edit role/session/config/agent-config/rag-docs — opens editor
- [ ] .empty session — clears session
- [ ] .compress session — compresses session
- [ ] .rebuild rag — rebuilds RAG
- [ ] .sources rag — shows RAG sources
- [ ] .copy — copies last response
- [ ] .continue — continues response
- [ ] .regenerate — regenerates response
- [ ] .file <path> [-- text] — includes files
- [ ] .macro <name> [text] — runs/creates macro
- [ ] .authenticate — OAuth flow
- [ ] .vault <cmd> [name] — vault operations
- [ ] .clear todo — clears agent todo

### ask function (chat flow)
- [ ] Input constructed from text
- [ ] Embeddings applied if RAG active
- [ ] Waits for compression to complete
- [ ] before_chat_completion called
- [ ] Streaming vs non-streaming based on config
- [ ] Tool results loop (recursive ask with merged results)
- [ ] after_chat_completion called
- [ ] Auto-continuation for agents with todos

## Old code reference
- `src/repl/mod.rs` — run_repl_command, ask, REPL_COMMANDS
