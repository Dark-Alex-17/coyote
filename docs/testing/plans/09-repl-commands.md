# Test Plan: REPL Commands

## Feature description

The REPL processes dot-commands (`.role`, `.session`, `.agent`, etc.)
and plain text (chat messages). Each command has state assertions
(e.g., `.info role` requires an active role).

## Behaviors to test

### Command parsing
- [x] Dot-commands parsed correctly (command + args)
- [x] Multi-line input (:::) handled (regex)
- [x] Plain text treated as chat message (parse_command returns None)
- [x] Empty input ignored (parse_command returns None)

### State assertions (REPL_COMMANDS array)
- [x] Each command's assert_state enforced correctly
- [x] Invalid state → command rejected (via is_valid)
- [x] Commands with AssertState::pass() always available

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

## Additional behaviors tested (not in original plan)

- [x] AssertState::pass() always returns true (all flag combos)
- [x] AssertState::bare() only matches empty flags
- [x] AssertState::True requires any matching flag present
- [x] AssertState::True with multiple flags — any match suffices
- [x] AssertState::False requires all specified flags absent
- [x] AssertState::False with multiple flags
- [x] AssertState::TrueFalse — true present AND false absent
- [x] AssertState::Equal — exact flag match
- [x] REPL_COMMANDS has exactly 39 entries
- [x] All commands start with '.'
- [x] All commands have non-empty descriptions
- [x] .help, .exit always available (pass)
- [x] .info role requires ROLE
- [x] .session blocked when already in session
- [x] .exit session requires session
- [x] .exit agent requires agent
- [x] .agent only when bare (no role/session/agent)
- [x] .role blocked in session/agent
- [x] .prompt blocked in session/agent
- [x] .rag blocked in agent
- [x] .starter requires agent
- [x] .clear todo requires agent
- [x] .edit role requires ROLE, blocked in SESSION
- [x] .exit rag requires RAG, blocked in AGENT
- [x] split_first_arg: None, single word, two words, extra spaces
- [x] parse_command: plain text, empty, whitespace, dot only
- [x] ReplCommand::is_valid with pass/True/False
- [x] Multiline regex: captures content, rejects unclosed, rejects plain text

## Old code reference
- `src/repl/mod.rs` — run_repl_command, ask, REPL_COMMANDS
