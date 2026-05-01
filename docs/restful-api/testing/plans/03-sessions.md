# Test Plan: Sessions

## Feature description

Sessions persist conversation history across multiple turns. They
store messages, role context, model info, and optional MCP config.
Sessions can be temporary, named, or auto-named.

## Behaviors to test

### Session creation
- [ ] Temp session created with TEMP_SESSION_NAME
- [ ] Named session created at correct file path
- [ ] New session captures current role via extract_role
- [ ] New session captures save_session from AppConfig
- [ ] Session tracks model_id

### Session loading
- [ ] Named session loads from YAML file
- [ ] Loaded session resolves model via Model::retrieve_model
- [ ] Loaded session restores role_prompt if role exists
- [ ] Auto-named sessions (prefixed `_/`) handled correctly

### Session saving
- [ ] Session saved to correct path
- [ ] Session file contains messages, model_id, role info
- [ ] save_session flag controls whether session is persisted
- [ ] set_save_session_this_time overrides for current turn

### Session lifecycle
- [ ] use_session creates or loads session
- [ ] Already in session → error
- [ ] exit_session saves and clears
- [ ] empty_session clears messages but keeps session active

### Session carry-over
- [ ] New empty session with last_message prompts "incorporate?"
- [ ] If accepted, last Q&A added to session
- [ ] If declined, session starts fresh
- [ ] Only prompts when continuous and output not empty

### Session compression
- [ ] maybe_compress_session returns true when threshold exceeded
- [ ] compress_session reduces message count
- [ ] Compression message shown to user
- [ ] Session usable after compression

### Session autoname
- [ ] maybe_autoname_session returns true for new sessions
- [ ] Auto-naming sets session name based on content
- [ ] Autoname only triggers once per session

### Session info
- [ ] session_info returns formatted session details
- [ ] Shows message count, model, role, tokens

## Context switching scenarios
- [ ] Session → role change: role updated within session
- [ ] Session → exit session: messages saved, state cleared
- [ ] Agent session → exit: agent session cleanup
- [ ] Session with MCP → exit: MCP servers handled

## Old code reference
- `src/config/mod.rs` — `use_session`, `exit_session`, `empty_session`
- `src/config/session.rs` — `Session` struct, new, load, save
- `src/config/request_context.rs` — `use_session`, `exit_session`
