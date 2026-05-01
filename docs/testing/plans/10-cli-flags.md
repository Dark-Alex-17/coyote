# Test Plan: CLI Flags

## Feature description

Loki CLI accepts flags for model, role, session, agent, file input,
execution mode, and various info/list commands. Flags determine
the execution path through main.rs.

## Behaviors to test

### Early-exit flags
- [x] --info parsed correctly
- [x] --list-models parsed correctly
- [x] --list-roles parsed correctly
- [x] --list-sessions parsed correctly
- [x] --list-agents parsed correctly
- [x] --list-rags parsed correctly
- [x] --list-macros parsed correctly
- [x] --sync-models parsed correctly
- [x] --build-tools parsed correctly
- [ ] --authenticate runs OAuth and exits (integration)
- [ ] --completions generates shell completions and exits (integration)
- [x] Vault flags (--add/get/update/delete-secret, --list-secrets) parsed

### Mode selection
- [x] No text/file → text returns None (REPL indicator)
- [x] Text provided → text joined and returned
- [x] --agent → agent field set
- [x] --role → role field set
- [x] --execute (-e) → execute flag set
- [x] --code (-c) → code flag set
- [x] --prompt → prompt field set
- [x] --macro → macro_name field set

### Flag combinations
- [x] --model + --role parsed together
- [x] --session + --role parsed together
- [ ] --session + --agent → agent with session (integration)
- [ ] --agent + --agent-variable → variables set (integration)
- [x] --dry-run flag parsed
- [x] --no-stream (-S) flag parsed
- [x] --file + text → both parsed
- [x] --empty-session + --session parsed
- [x] --save-session + --session parsed

### Prelude
- [ ] apply_prelude runs before main execution (async + filesystem)
- [ ] Prelude "role:name" loads role (async + filesystem)
- [ ] Prelude "session:name" loads session (async + filesystem)
- [ ] Prelude "session:role" loads both (async + filesystem)
- [ ] Prelude skipped if macro_flag set (async)
- [ ] Prelude skipped if state already has role/session/agent (async)

## Additional behaviors tested (not in original plan)

- [x] Default Cli has all flags unset/empty
- [x] Short flags: -m, -r, -a, -s, -e, -c, -S, -f
- [x] Multiple -f flags accumulate
- [x] Trailing text args collected as vec
- [x] Cli::text() returns None with no args (terminal stdin)
- [x] Cli::text() joins trailing args with spaces
- [x] --rag flag parsed
- [x] --macro flag parsed

## Old code reference
- `src/cli/mod.rs` — Cli struct, flag definitions
- `src/main.rs` — run(), flag processing, mode branching
