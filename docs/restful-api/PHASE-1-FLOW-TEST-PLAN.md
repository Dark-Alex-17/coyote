# Phase 1 Flow Test Plan

Comprehensive behavioral verification plan comparing the old codebase
(`~/code/testing/loki` on `develop` branch) against the new Phase 1
codebase (`~/code/loki`). Every test should produce identical behavior
in both codebases unless noted as an intentional improvement.

## How to run

For each test case:
1. Run the test in the OLD codebase (`cd ~/code/testing/loki && cargo run --`)
2. Run the same test in the NEW codebase (`cd ~/code/loki && cargo run --`)
3. Compare output/behavior
4. Mark PASS/FAIL/IMPROVED

Legend:
- `OLD:` = expected behavior from old codebase
- `NEW:` = expected behavior from new codebase (should match unless noted)
- `[IMPROVED]` = intentional behavioral improvement in new code

---

## 1. Build Baseline

| # | Test | Command | Expected |
|---|---|---|---|
| 1.1 | Compile check | `cargo check` | Zero warnings, zero errors |
| 1.2 | Clippy | `cargo clippy` | Zero warnings (excluding pre-existing) |
| 1.3 | Tests | `cargo test` | All tests pass |

---

## 2. CLI — Info and Listing (early-exit paths)

These should produce identical output in both codebases.

| # | Test | Command | Expected |
|---|---|---|---|
| 2.1 | System info | `loki --info` | Prints config paths, model, settings |
| 2.2 | List models | `loki --list-models` | Prints all available model IDs |
| 2.3 | List roles | `loki --list-roles` | Prints role names (no hidden files) |
| 2.4 | List sessions | `loki --list-sessions` | Prints session names |
| 2.5 | List agents | `loki --list-agents` | Prints agent names, no `.shared` [IMPROVED] |
| 2.6 | List RAGs | `loki --list-rags` | Prints RAG names |
| 2.7 | List macros | `loki --list-macros` | Prints macro names |
| 2.8 | Sync models | `loki --sync-models` | Fetches models.yaml, prints status |

---

## 3. CLI — Single-shot Chat

| # | Test | Command | Expected |
|---|---|---|---|
| 3.1 | Basic chat | `loki "What is 2+2?"` | Response printed, exits |
| 3.2 | With role | `loki --role coder "hello"` | Role context applied |
| 3.3 | With prompt | `loki --prompt "you are a pirate" "hello"` | Temp role applied |
| 3.4 | With model | `loki --model <model_id> "hello"` | Uses specified model |
| 3.5 | With session | `loki -s test "hello"` | Session created, message saved |
| 3.6 | Resume session | `loki -s test "what did I say?"` | Session context preserved |
| 3.7 | Dry run | `loki --dry-run "hello"` | Input echoed, no API call |
| 3.8 | No stream | `loki --no-stream "hello"` | Response printed all at once |
| 3.9 | Empty session | `loki -s test --empty-session "hello"` | Session cleared, fresh start |
| 3.10 | Save session | `loki -s test --save-session "hello"` | Forces session save |
| 3.11 | Code mode | `loki -c "fibonacci in python"` | Only code output |

---

## 4. CLI — File Input

| # | Test | Command | Expected |
|---|---|---|---|
| 4.1 | File + text | `loki -f /etc/hostname "summarize"` | File content included |
| 4.2 | File only | `loki -f /etc/hostname` | File sent as input |
| 4.3 | Multiple files | `loki -f /etc/hostname -f /etc/os-release "compare"` | Both files included |
| 4.4 | Stdin pipe | `echo "hello" \| loki "summarize"` | Stdin included |

---

## 5. CLI — Shell Execute

| # | Test | Command | Expected |
|---|---|---|---|
| 5.1 | Generate command | `loki -e "list files in /tmp"` | Shell command generated |
| 5.2 | Describe mode | Press 'd' when prompted | Explanation shown |
| 5.3 | Execute mode | Press 'y' when prompted | Command executed |
| 5.4 | Dry run | `loki -e --dry-run "list files"` | Input shown, no execution |

---

## 6. CLI — Agent (non-interactive)

| # | Test | Command | Expected |
|---|---|---|---|
| 6.1 | Agent chat | `loki -a coder "write hello world in python"` | Agent tools available, response |
| 6.2 | Agent + session | `loki -a coder -s test "hello"` | Agent with specific session |
| 6.3 | Agent variables | `loki -a demo --agent-variable key val "hello"` | Variable injected |
| 6.4 | Agent MCP | `loki -a <mcp-agent> "use the server"` | MCP servers start, tools work |
| 6.5 | Build tools | `loki -a coder --build-tools` | Tools compiled, exits |

---

## 7. CLI — Macros

| # | Test | Command | Expected |
|---|---|---|---|
| 7.1 | Execute macro | `loki --macro generate-commit-message` | Macro executes |

---

## 8. CLI — Vault (early-exit)

| # | Test | Command | Expected |
|---|---|---|---|
| 8.1 | Add secret | `loki --add-secret test-secret` | Prompts for value, saves |
| 8.2 | Get secret | `loki --get-secret test-secret` | Prints decrypted value |
| 8.3 | List secrets | `loki --list-secrets` | Lists all secret names |
| 8.4 | Delete secret | `loki --delete-secret test-secret` | Deletes, confirms |

---

## 9. REPL — Startup and Exit

| # | Test | Steps | Expected |
|---|---|---|---|
| 9.1 | Start REPL | `loki` | Welcome message shown |
| 9.2 | Exit command | Type `.exit` | Clean exit |
| 9.3 | Ctrl+D | Press Ctrl+D | Clean exit |
| 9.4 | Ctrl+C | Press Ctrl+C | Hint message, stays in REPL |
| 9.5 | Prelude role | Set `repl_prelude: "role:coder"` in config, start REPL | Role auto-loaded, prompt changes |
| 9.6 | Prelude session | Set `repl_prelude: "mysession:coder"`, start | Session+role auto-loaded |

---

## 10. REPL — Basic Chat

| # | Test | Steps | Expected |
|---|---|---|---|
| 10.1 | Chat message | Type `hello` | Response streamed |
| 10.2 | Continue | Type `.continue` after response | Continuation generated |
| 10.3 | Regenerate | Type `.regenerate` | New response generated |
| 10.4 | Copy | Type `.copy` | Last response copied to clipboard |
| 10.5 | Multi-line | Type `:::`, then multi-line, then `:::` | Multi-line sent as one message |
| 10.6 | Empty input | Press Enter on empty line | No action |
| 10.7 | Help | Type `.help` | Help text shown |
| 10.8 | Info | Type `.info` | System info printed |

---

## 11. REPL — Roles

| # | Test | Steps | Expected |
|---|---|---|---|
| 11.1 | Enter role | `.role coder` | Prompt changes, role active |
| 11.2 | One-shot role | `.role coder write hello world` | Response with role, then returns to no-role |
| 11.3 | Role info | `.info role` (while in role) | Role details shown |
| 11.4 | Edit role | `.edit role` (while in role) | Editor opens |
| 11.5 | Save role | `.save role myname` | Role saved to file |
| 11.6 | Exit role | `.exit role` | Prompt resets, role cleared |
| 11.7 | Create new role | `.role newname` (non-existent) | Editor opens for new role |
| 11.8 | Role + MCP | `.role <mcp-role>` | MCP servers start with spinner, tools available |
| 11.9 | Exit role + MCP | `.exit role` (from MCP role) | MCP servers stop, global MCP restored |
| 11.10 | Role in session | `.session test` then `.role coder` | Role applied within session |

---

## 12. REPL — Sessions

| # | Test | Steps | Expected |
|---|---|---|---|
| 12.1 | Temp session | `.session` | Temp session started |
| 12.2 | Named session | `.session mytest` | Named session created/resumed |
| 12.3 | Session info | `.info session` | Session details shown |
| 12.4 | Edit session | `.edit session` | Editor opens |
| 12.5 | Save session | `.save session myname` | Session saved |
| 12.6 | Empty session | `.empty session` | Messages cleared |
| 12.7 | Compress session | `.compress session` | Compression runs with spinner |
| 12.8 | Exit session | `.exit session` | Session exited |
| 12.9 | Carry-over prompt | Send message, then `.session test` | "incorporate last Q&A?" prompt |
| 12.10 | Session + MCP | `.session <mcp-session>` | MCP servers start |
| 12.11 | Already in session | `.session` while in session | Error: "Already in a session" |

---

## 13. REPL — Agents

| # | Test | Steps | Expected |
|---|---|---|---|
| 13.1 | Start agent | `.agent coder` | Tools compiled, prompt changes, agent active |
| 13.2 | Agent + session | `.agent coder mysession` | Agent with specific session |
| 13.3 | Agent variables | `.agent demo key=value` | Variable set, available in tools |
| 13.4 | Agent info | `.info agent` | Agent details shown |
| 13.5 | Starter list | `.starter` | Conversation starters listed |
| 13.6 | Starter select | `.starter 1` | Starter message sent |
| 13.7 | Edit agent config | `.edit agent-config` | Editor opens |
| 13.8 | Exit agent | `.exit agent` | Agent cleared, prompt resets |
| 13.9 | Agent + MCP | `.agent <mcp-agent>` | MCP servers start, tools available |
| 13.10 | MCP disabled | `.agent <mcp-agent>` with mcp_server_support=false | Error, agent blocked [IMPROVED] |
| 13.11 | Tool execution | Send message that triggers tool call | Tool executes, result returned |
| 13.12 | Global tools | Agent with `global_tools` configured | Global tools available alongside agent tools |
| 13.13 | Tool file priority | Delete .ts, have .sh | .sh used [IMPROVED] |
| 13.14 | Clear todo | `.clear todo` (in agent with auto-continue) | Todo list cleared |
| 13.15 | Auto-continuation | Agent with auto_continue=true, create todos | Agent continues until todos done |
| 13.16 | Already in agent | `.agent coder` while agent active | Error: "Already in an agent" |

---

## 14. REPL — Sub-Agent Spawning and Escalation

| # | Test | Steps | Expected |
|---|---|---|---|
| 14.1 | Spawn sub-agent | Use agent with can_spawn_agents=true, trigger spawn | Sub-agent starts in background |
| 14.2 | Check sub-agent | Call agent__check with agent ID | Returns PENDING or result |
| 14.3 | Collect sub-agent | Call agent__collect with agent ID | Blocks until done, returns output |
| 14.4 | List sub-agents | Call agent__list | Shows all spawned agents + status |
| 14.5 | Cancel sub-agent | Call agent__cancel with agent ID | Agent cancelled |
| 14.6 | Escalation | Sub-agent calls user__ask | Parent gets notification |
| 14.7 | Reply escalation | Parent calls agent__reply_escalation | Sub-agent unblocked |
| 14.8 | Max depth | Spawn beyond max_agent_depth | Error: "Max agent depth exceeded" |
| 14.9 | Max concurrent | Spawn beyond max_concurrent_agents | Error: capacity reached |
| 14.10 | Teammate messaging | Sub-agent sends message to sibling | Message delivered via inbox |

---

## 15. REPL — RAG

| # | Test | Steps | Expected |
|---|---|---|---|
| 15.1 | Init RAG | `.rag <name>` | RAG initialized/loaded |
| 15.2 | RAG info | `.info rag` | RAG details shown |
| 15.3 | RAG sources | `.sources rag` (after a query) | Citation sources listed |
| 15.4 | Edit RAG docs | `.edit rag-docs` | Editor opens |
| 15.5 | Rebuild RAG | `.rebuild rag` | RAG rebuilt |
| 15.6 | Exit RAG | `.exit rag` | RAG cleared |
| 15.7 | RAG embeddings | Send query with RAG active | Embeddings included in context |

---

## 16. REPL — MCP Servers

| # | Test | Steps | Expected |
|---|---|---|---|
| 16.1 | Global MCP start | Start REPL with `enabled_mcp_servers` configured | Servers start |
| 16.2 | MCP search | LLM calls `mcp__search_<server>` | Tools found and ranked |
| 16.3 | MCP describe | LLM calls `mcp__describe_<server>` tool_name | Schema returned |
| 16.4 | MCP invoke | LLM calls `mcp__invoke_<server>` tool args | Tool executed, result returned |
| 16.5 | Change servers | `.set enabled_mcp_servers <other>` | Old stopped, new started |
| 16.6 | Disable MCP | `.set mcp_server_support false` | MCP tools removed |
| 16.7 | Enable MCP | `.set mcp_server_support true` | MCP tools restored |
| 16.8 | Role MCP switch | Enter role with MCP X, exit, enter role with MCP Y | X stops, Y starts |
| 16.9 | Null servers | `.set enabled_mcp_servers null` | All MCP servers stop, tools removed |

---

## 17. REPL — Settings (.set)

| # | Test | Steps | Expected |
|---|---|---|---|
| 17.1 | Temperature | `.set temperature 0.5` | Temperature changed |
| 17.2 | Top-p | `.set top_p 0.9` | Top-p changed |
| 17.3 | Model | `.set model <name>` | Model switched |
| 17.4 | Dry run | `.set dry_run true` | Dry run enabled |
| 17.5 | Stream | `.set stream false` | Streaming disabled |
| 17.6 | Save | `.set save false` | Auto-save disabled |
| 17.7 | Highlight | `.set highlight false` | Syntax highlighting disabled |
| 17.8 | Save session | `.set save_session true` | Session auto-save enabled |
| 17.9 | Null value | `.set temperature null` | Temperature reset to default |
| 17.10 | Compression threshold | `.set compression_threshold 2000` | Threshold changed |
| 17.11 | Max output tokens | `.set max_output_tokens 4096` | Max tokens set |
| 17.12 | Enabled tools | `.set enabled_tools all` | All tools enabled |
| 17.13 | Function calling | `.set function_calling_support false` | Function calling disabled |

---

## 18. REPL — Tab Completion

| # | Test | Steps | Expected |
|---|---|---|---|
| 18.1 | Role completion | `.role<TAB>` | Shows role names |
| 18.2 | Agent completion | `.agent<TAB>` | Shows agent names (no .shared) [IMPROVED] |
| 18.3 | Session completion | `.session<TAB>` | Shows session names |
| 18.4 | RAG completion | `.rag<TAB>` | Shows RAG names |
| 18.5 | Macro completion | `.macro<TAB>` | Shows macro names |
| 18.6 | Model completion | `.model<TAB>` | Shows model names with descriptions |
| 18.7 | Set keys | `.set <TAB>` | Shows all setting names |
| 18.8 | Set values | `.set temperature <TAB>` | Shows current/suggested value |
| 18.9 | Enabled tools | `.set enabled_tools <TAB>` | Shows tools (no user__/mcp_/todo__/agent__) [IMPROVED] |
| 18.10 | MCP servers | `.set enabled_mcp_servers <TAB>` | Shows configured servers + mappings [IMPROVED] |
| 18.11 | Delete types | `.delete <TAB>` | Shows: role, session, rag, macro, agent-data |
| 18.12 | Vault cmds | `.vault <TAB>` | Shows: add, get, update, delete, list |

---

## 19. REPL — Delete

| # | Test | Steps | Expected |
|---|---|---|---|
| 19.1 | Delete role | `.delete role` | Shows role picker, deletes selected |
| 19.2 | Delete session | `.delete session` | Shows session picker, deletes |
| 19.3 | Delete RAG | `.delete rag` | Shows RAG picker, deletes |
| 19.4 | Delete macro | `.delete macro` | Shows macro picker, deletes |
| 19.5 | Delete agent data | `.delete agent-data` | Shows agent picker, deletes data |

---

## 20. REPL — Vault

| # | Test | Steps | Expected |
|---|---|---|---|
| 20.1 | Add secret | `.vault add mysecret` | Prompts for value, saves |
| 20.2 | Get secret | `.vault get mysecret` | Prints decrypted value |
| 20.3 | Update secret | `.vault update mysecret` | Prompts for new value |
| 20.4 | Delete secret | `.vault delete mysecret` | Deletes |
| 20.5 | List secrets | `.vault list` | Lists all secret names |

---

## 21. REPL — Macros and File

| # | Test | Steps | Expected |
|---|---|---|---|
| 21.1 | Execute macro | `.macro generate-commit-message` | Macro runs |
| 21.2 | Create macro | `.macro newname` (non-existent) | Editor opens |
| 21.3 | File include | `.file /etc/hostname -- summarize this` | File included, query sent |
| 21.4 | URL include | `.file https://example.com -- summarize` | URL fetched, content included |

---

## 22. REPL — Edit Commands

| # | Test | Steps | Expected |
|---|---|---|---|
| 22.1 | Edit config | `.edit config` | Config file opens in editor |
| 22.2 | Edit role | `.edit role` (in role) | Role file opens in editor |
| 22.3 | Edit session | `.edit session` (in session) | Session file opens in editor |
| 22.4 | Edit agent config | `.edit agent-config` (in agent) | Agent config opens in editor |
| 22.5 | Edit RAG docs | `.edit rag-docs` (in RAG) | RAG docs opens in editor |

---

## 23. Session Compression and Autoname

| # | Test | Steps | Expected |
|---|---|---|---|
| 23.1 | Auto-compress | Set low compression_threshold, send many messages | "Compressing the session." shown |
| 23.2 | Manual compress | `.compress session` | Compression runs with spinner |
| 23.3 | Auto-name | Start temp session, send messages | Session auto-named |

---

## 24. Error Handling

| # | Test | Steps | Expected |
|---|---|---|---|
| 24.1 | Invalid role | `.role nonexistent_role_xxxxxxx` | Error shown, REPL continues |
| 24.2 | Invalid model | `.set model nonexistent_model` | Error shown, REPL continues |
| 24.3 | No session active | `.info session` (no session) | Error or empty |
| 24.4 | No agent active | `.info agent` (no agent) | Error or empty |
| 24.5 | Already in session | `.session` then `.session` again | Error: "Already in a session" |
| 24.6 | Already in agent | `.agent coder` then `.agent coder` | Error: "Already in an agent" |
| 24.7 | Unknown command | `.nonexistent` | Error message shown |
| 24.8 | Tool failure | Trigger tool that fails | Error returned to LLM as tool result |

---

## 25. MCP Lifecycle State Transitions (Critical)

These test the most bug-prone area of the migration.

| # | Test | Steps | Expected |
|---|---|---|---|
| 25.1 | Role A→B MCP swap | Enter role with MCP-A, exit, enter role with MCP-B | A stops, B starts, B tools work |
| 25.2 | Role MCP→no MCP | Enter role with MCP, exit role | MCP stops, global MCP restored |
| 25.3 | No MCP→Role MCP | Start REPL (no MCP), enter role with MCP | MCP starts, tools work |
| 25.4 | Agent MCP lifecycle | Start agent with MCP, use tools, exit agent | Agent MCP starts, works, stops on exit |
| 25.5 | Session MCP | Start session with MCP config | MCP starts for session |
| 25.6 | Global→Agent→Global | Start with global MCP-A, enter agent with MCP-B, exit agent | A→B→A transitions clean |
| 25.7 | MCP mapping resolution | Role has `enabled_mcp_servers: alias`, mapping configured | Alias resolved, correct servers start |
| 25.8 | MCP disabled + agent | Agent requires MCP, mcp_server_support=false | Error blocks agent start [IMPROVED] |

---

## Intentional Improvements (NEW ≠ OLD, by design)

| # | What changed | Old behavior | New behavior |
|---|---|---|---|
| I.1 | Agent list hides `.shared` | `.shared` shown in completions | `.shared` hidden |
| I.2 | Tool file priority | Filesystem order (non-deterministic) | Priority: .sh > .py > .ts > .js |
| I.3 | MCP disabled + agent | Warning printed, agent starts anyway | Error, agent blocked |
| I.4 | Role MCP disabled warning | Warning always shown (even if role has no MCP) | Warning only when role actually has MCP |
| I.5 | Enabled tools completions | Shows internal tools (user__, mcp_, etc.) | Internal tools hidden |
| I.6 | MCP server completions | Only mapping aliases | Both configured servers + aliases |

---

## Test Execution Notes

- Run tests in order — some depend on state from previous tests
  (e.g., session tests create sessions that later tests reference)
- For MCP tests, ensure at least one MCP server is configured in
  `~/.config/loki/functions/mcp.json`
- For agent tests, use built-in agents (coder, demo, explore)
- For sub-agent tests, use the sisyphus agent (has can_spawn_agents)
- For RAG tests, configure a RAG with test documents
- For vault tests, use temporary secret names to avoid polluting
  the real vault
- Compare error messages between old and new — they may differ
  slightly in wording but should convey the same meaning
