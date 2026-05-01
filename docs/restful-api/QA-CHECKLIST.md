# Loki QA Checklist

Behavioral verification checklist for the REST API refactor.
Run after each step or phase to confirm existing functionality
is preserved.

## How to use

- [ ] = not yet verified for current step
- [x] = verified working
- SKIP = not applicable to current step

Check each item manually in the REPL and/or CLI. If a check
fails, stop and investigate before proceeding.

---

## 1. Build & Test Baseline

- [ ] `cargo check` — zero warnings, zero errors
- [ ] `cargo clippy` — zero warnings
- [ ] `cargo test` — all tests pass (63 as of Step 8g)

## 2. CLI — Basic Operations

- [ ] `loki "hello"` — single-shot chat works, response printed
- [ ] `loki --role <name> "hello"` — role applied, response uses role context
- [ ] `loki --session <name> "hello"` — session created/resumed, response saved
- [ ] `loki --model <model_id> "hello"` — specified model used
- [ ] `loki --prompt "you are a pirate" "hello"` — temp role applied
- [ ] `loki --info` — system info printed, exits cleanly
- [ ] `loki --list-models` — model list printed
- [ ] `loki --list-roles` — role list printed (no hidden files)
- [ ] `loki --list-sessions` — session list printed
- [ ] `loki --list-agents` — agent list printed (no `.shared` directory)
- [ ] `loki --dry-run "hello"` — no API call, input echoed
- [ ] `loki --no-stream "hello"` — non-streaming response

## 3. CLI — File Input

- [ ] `loki --file /tmp/test.txt "summarize"` — file content included
- [ ] `loki --file /tmp/test.txt` — file content sent without extra text

## 4. CLI — Agent (non-interactive)

- [ ] `loki --agent <name> "do something"` — agent starts, tools available, response returned
- [ ] Agent MCP servers start (if configured)
- [ ] Agent tool calls execute correctly (e.g., execute_command)

## 5. CLI — Shell Execute

- [ ] `loki -e "list files in /tmp"` — shell command generated
- [ ] Shell command explanation shown (describe mode)
- [ ] Shell command execution works when confirmed

## 6. CLI — Macro

- [ ] `loki --macro <name> "input"` — macro executes

## 7. REPL — Startup & Exit

- [ ] `loki` — REPL starts, welcome message shown
- [ ] `.exit` — REPL exits cleanly
- [ ] Ctrl+D — REPL exits cleanly
- [ ] Ctrl+C — prints exit hint, does not exit

## 8. REPL — Chat

- [ ] Type a message — response printed
- [ ] `.continue` — continues previous response
- [ ] `.regenerate` — regenerates last response
- [ ] `.copy` — copies last response to clipboard

## 9. REPL — Roles

- [ ] `.role <name>` — switches to role, prompt changes
- [ ] `.role <name> <text>` — one-shot role message
- [ ] `.info role` — shows role info
- [ ] `.edit role` — opens editor for current role
- [ ] `.save role <name>` — saves current role
- [ ] `.exit role` — exits role, prompt resets
- [ ] Role with MCP servers — servers start on `.role <name>`
- [ ] Role with MCP servers — MCP tools available in chat
- [ ] `.exit role` with MCP — servers stop, MCP tools removed

## 10. REPL — Sessions

- [ ] `.session` — starts temp session
- [ ] `.session <name>` — starts/resumes named session
- [ ] `.info session` — shows session info
- [ ] `.edit session` — opens editor
- [ ] `.save session <name>` — saves session
- [ ] `.empty session` — clears messages
- [ ] `.compress session` — compresses session
- [ ] `.exit session` — exits session
- [ ] Session with MCP servers — servers start
- [ ] Session carry-over prompt — "incorporate last Q&A?" appears when applicable

## 11. REPL — Agents

- [ ] `.agent <name>` — agent starts, tools compiled, prompt changes
- [ ] `.agent <name> <session>` — agent starts with specific session
- [ ] `.agent <name> key=value` — agent starts with variables
- [ ] `.info agent` — shows agent info
- [ ] `.starter` — shows conversation starters
- [ ] `.starter <n>` — executes starter
- [ ] `.edit agent-config` — opens agent config editor
- [ ] `.exit agent` — exits agent cleanly
- [ ] Agent with MCP servers — servers start
- [ ] Agent tool calls work (execute_command, fs_read, etc.)
- [ ] Agent global tools work (tools listed in `global_tools`)
- [ ] Agent tool file changes picked up on restart (delete .ts, .sh used instead)
- [ ] Auto-continuation works (todo list drives continuation)
- [ ] `.clear todo` — clears todo list

## 12. REPL — Sub-Agent Escalation

- [ ] Parent agent spawns sub-agent via tool call
- [ ] Sub-agent runs at depth > 0
- [ ] Sub-agent escalation: sub-agent calls user__ask → parent gets notification
- [ ] Parent calls agent__reply_escalation → sub-agent unblocked, resumes
- [ ] Multiple pending escalations shown in notification
- [ ] Max depth enforcement — sub-agent spawn rejected beyond max_agent_depth

## 13. REPL — RAG

- [ ] `.rag <name>` — initializes/loads RAG
- [ ] `.info rag` — shows RAG info
- [ ] `.sources rag` — shows citation sources
- [ ] `.edit rag-docs` — modify RAG documents
- [ ] `.rebuild rag` — rebuilds RAG index
- [ ] `.exit rag` — exits RAG
- [ ] RAG embeddings used in chat (search results included)

## 14. REPL — MCP Servers

- [ ] MCP servers start at REPL init (if globally enabled)
- [ ] `.set enabled_mcp_servers <name>` — changes active servers
- [ ] `.set mcp_server_support true/false` — toggles support
- [ ] MCP tool invocation works (mcp__invoke_<server>)
- [ ] MCP tool search works (mcp__search_<server>)
- [ ] MCP tool describe works (mcp__describe_<server>)

## 15. REPL — Settings

- [ ] `.set temperature 0.5` — changes temperature
- [ ] `.set top_p 0.9` — changes top_p
- [ ] `.set model <name>` — changes model
- [ ] `.set dry_run true` — enables dry run
- [ ] `.set stream false` — disables streaming
- [ ] `.set save true/false` — toggles save
- [ ] `.set highlight true/false` — toggles highlighting
- [ ] `.set save_session true/false/null` — changes session save behavior
- [ ] `.set compression_threshold <n>` — changes threshold

## 16. REPL — Tab Completion

- [ ] `.role<TAB>` — shows role names (no hidden files)
- [ ] `.agent<TAB>` — shows agent names (no `.shared` directory)
- [ ] `.session<TAB>` — shows session names
- [ ] `.rag<TAB>` — shows RAG names
- [ ] `.macro<TAB>` — shows macro names
- [ ] `.model<TAB>` — shows model names with descriptions
- [ ] `.set <TAB>` — shows setting names
- [ ] `.set temperature <TAB>` — shows current value
- [ ] `.set enabled_tools <TAB>` — shows tool names
- [ ] `.set enabled_mcp_servers <TAB>` — shows server names

## 17. REPL — Delete

- [ ] `.delete role <name>` — deletes role
- [ ] `.delete session <name>` — deletes session
- [ ] `.delete rag <name>` — deletes RAG
- [ ] `.delete macro <name>` — deletes macro
- [ ] `.delete agent-data <name>` — deletes agent data

## 18. REPL — Vault

- [ ] `.vault list` — lists secrets
- [ ] `.vault add <name>` — adds secret
- [ ] `.vault get <name>` — retrieves secret
- [ ] `.vault update <name>` — updates secret
- [ ] `.vault delete <name>` — deletes secret

## 19. REPL — Prelude

- [ ] `repl_prelude: "role:coder"` — auto-loads role on REPL start
- [ ] `repl_prelude: "session:mysession"` — auto-loads session
- [ ] `repl_prelude: "mysession:coder"` — auto-loads session with role

## 20. REPL — Miscellaneous

- [ ] `.help` — shows help text
- [ ] `.info` — shows system info
- [ ] `.authenticate` — OAuth flow (if configured)
- [ ] `.file <path>` — includes file in next message
- [ ] `.file <url>` — fetches URL content
- [ ] Unknown command — shows error message
- [ ] Multi-line input (:::) — works correctly
- [ ] Ctrl+O — opens editor for input buffer

## 21. Session Compression & Autoname

- [ ] Session auto-compression triggers when threshold exceeded
- [ ] Compression message shown ("Compressing the session.")
- [ ] Session auto-naming triggers for new sessions
- [ ] Auto-continuation after compression works (agent resumes)

## 22. Error Handling

- [ ] Invalid role name — error shown, REPL continues
- [ ] Invalid model name — error shown, REPL continues
- [ ] Network error during chat — error shown, REPL continues
- [ ] MCP server crash — error shown, REPL continues
- [ ] Tool execution failure — error returned to LLM as tool result

---

## Phase-specific notes

### Phase 1 (Steps 3-10): Config split into AppState + RequestContext

Known bridge-window limitations (acceptable until Steps 9-10):
- `ReplCompleter`/`ReplPrompt` still hold `GlobalConfig`
- `Input` still holds `GlobalConfig` internally
- `eval_tool_calls` still takes `&GlobalConfig`
- Dual sync (`sync_ctx_to_config`/`sync_config_to_ctx`) required

### Post-Phase 1 verification focus:
- All items above should work identically to pre-refactor behavior
- No new warnings or errors in build
- Performance should be equivalent (no observable slowdown)
