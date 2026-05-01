# Phase 1 QA — Test Implementation Plan

## Purpose

Verify that all existing Loki behaviors are preserved after the
Phase 1 refactoring (Config god-state → AppState + RequestContext
split). Tests should validate behavior, not implementation details,
unless a specific implementation pattern is fragile and needs
regression protection.

## Reference codebases

- **Old code**: `~/code/testing/loki` (branch: `develop`)
- **New code**: `~/code/loki` (branch: working branch with Phase 1)

## Process (per iteration)

1. Read the previous iteration's test implementation notes (if any)
2. Read the test plan file for the current feature area
3. Read the old code to identify the logic that creates those flows
4. While reading old code:
   - Note additional behaviors not in the plan file → update the file
   - Note feature overlaps / context-switching scenarios → add tests
5. Create unit/integration tests in the new code
6. Ensure all tests pass
7. Write test implementation notes for the iteration
8. Pause for user approval before proceeding to next iteration

## Test philosophy

- **Behavior over implementation**: Test what the system DOES, not
  HOW it does it internally
- **Exception**: If implementation logic is fragile and a slight
  change would break Loki, add an implementation-specific test
- **No business logic changes**: Only modify non-test code if a
  genuine bug is discovered (old behavior missing in new code)
- **Context switching**: Pay special attention to state transitions
  (role→agent, MCP-enabled→disabled, etc.)

## Test location

All new tests go in `tests/` directory as integration tests, or
inline as `#[cfg(test)] mod tests` in the relevant source file,
depending on what's being tested:

- **Unit tests** (pure logic, no I/O): inline in source file
- **Integration tests** (multi-module, state transitions): `tests/`
- **Behavior tests** (config parsing, tool resolution): can be either

## Feature areas (test plan files)

Each feature area has a plan file in `docs/testing/plans/`. The
files are numbered for execution order (dependencies first):

| # | File | Feature area | Priority | Status |
|---|---|---|---|---|
| 01 | `01-config-and-appconfig.md` | Config loading, AppConfig fields, defaults | High | ✅ Iter 1-4 |
| 02 | `02-roles.md` | Role loading, retrieval, role-likes, temp roles | High | ✅ Iter 1-4 |
| 03 | `03-sessions.md` | Session create/load/save, compression, autoname | High | ✅ Iter 1-4 |
| 04 | `04-agents.md` | Agent init, tool compilation, variables, lifecycle | Critical | ✅ Iter 1-4 |
| 05 | `05-mcp-lifecycle.md` | MCP server start/stop, factory, runtime, scope transitions | Critical | ✅ Iter 5 |
| 06 | `06-tool-evaluation.md` | eval_tool_calls, ToolCall dispatch, tool handlers | Critical | ✅ Iter 6 |
| 07 | `07-input-construction.md` | Input::from_str, from_files, field capturing, function selection | High | ✅ Iter 7 |
| 08 | `08-request-context.md` | RequestContext methods, scope transitions, state management | Critical | ✅ Iter 8 |
| 09 | `09-repl-commands.md` | REPL command handlers, state assertions, argument parsing | High | ✅ Iter 9 |
| 10 | `10-cli-flags.md` | CLI argument handling, mode switching, early exits | High | ✅ Iter 10 |
| 11 | `11-sub-agent-spawning.md` | Supervisor, child agents, escalation, messaging | Critical | ✅ Iter 11 |
| 12 | `12-rag.md` | RAG init/load/search, embeddings, document management | Medium | ✅ Iter 12 |
| 13 | `13-completions-and-prompt.md` | Tab completion, prompt rendering, highlighter | Medium | ✅ Iter 13 |
| 14 | `14-macros.md` | Macro loading, execution, variable interpolation | Medium | ✅ Iter 13 |
| 15 | `15-vault.md` | Secret management, interpolation in MCP config | Medium | ✅ Iter 13 |
| 16 | `16-functions-and-tools.md` | Function declarations, tool compilation, binaries | High | ✅ Iter 13 |

## Iteration tracking

Each completed iteration produces a notes file at:
`docs/testing/notes/ITERATION-<N>-NOTES.md`

These notes contain:
- Which plan file(s) were addressed
- Tests created (file paths, test names)
- Bugs discovered (if any)
- Observations for future iterations
- Updates made to other plan files

## Intentional improvements (NEW ≠ OLD)

These are behavioral changes that are intentional and should NOT
be tested for old-code parity:

| # | What | Old | New |
|---|---|---|---|
| 1 | Agent list hides `.shared` | Shown | Hidden |
| 2 | Tool file priority | Filesystem order | .sh > .py > .ts > .js |
| 3 | MCP disabled + agent | Warning, continues | Error, blocks |
| 4 | Role MCP warning | Always when mcp_support=false | Only when role has MCP |
| 5 | Enabled tools completions | Shows internal tools | Hides user__/mcp_/todo__/agent__ |
| 6 | MCP server completions | Only aliases | Configured servers + aliases |

## How to pick up in a new session

If context is lost (new chat session):

1. Read this file first
2. Read the latest `docs/testing/notes/ITERATION-<N>-NOTES.md`
3. That file tells you which plan file to work on next
4. Read that plan file
5. Follow the process above
