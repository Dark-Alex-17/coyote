# Test Plan: CLI Flags

## Feature description

Loki CLI accepts flags for model, role, session, agent, file input,
execution mode, and various info/list commands. Flags determine
the execution path through main.rs.

## Behaviors to test

### Early-exit flags
- [ ] --info prints info and exits
- [ ] --list-models prints models and exits
- [ ] --list-roles prints roles and exits
- [ ] --list-sessions prints sessions and exits
- [ ] --list-agents prints agents and exits
- [ ] --list-rags prints RAGs and exits
- [ ] --list-macros prints macros and exits
- [ ] --sync-models fetches and exits
- [ ] --build-tools (with --agent) builds and exits
- [ ] --authenticate runs OAuth and exits
- [ ] --completions generates shell completions and exits
- [ ] Vault flags (--add/get/update/delete-secret, --list-secrets) and exit

### Mode selection
- [ ] No text/file → REPL mode
- [ ] Text provided → command mode (single-shot)
- [ ] --agent → agent mode
- [ ] --role → role mode
- [ ] --execute (-e) → shell execute mode
- [ ] --code (-c) → code output mode
- [ ] --prompt → temp role mode
- [ ] --macro → macro execution mode

### Flag combinations
- [ ] --model + any mode → model applied
- [ ] --session + --role → session with role
- [ ] --session + --agent → agent with session
- [ ] --agent + --agent-variable → variables set
- [ ] --dry-run + any mode → input shown, no API call
- [ ] --no-stream + any mode → non-streaming response
- [ ] --file + text → file content + text combined
- [ ] --empty-session + --session → fresh session
- [ ] --save-session + --session → force save

### Prelude
- [ ] apply_prelude runs before main execution
- [ ] Prelude "role:name" loads role
- [ ] Prelude "session:name" loads session
- [ ] Prelude "session:role" loads both
- [ ] Prelude skipped if macro_flag set
- [ ] Prelude skipped if state already has role/session/agent

## Old code reference
- `src/cli/mod.rs` — Cli struct, flag definitions
- `src/main.rs` — run(), flag processing, mode branching
