# Test Plan: Macros

## Behaviors to test
- [ ] Macro loaded from YAML file (requires filesystem)
- [ ] Macro steps executed sequentially (requires async + RequestContext)
- [ ] Each step runs through run_repl_command (requires async)
- [x] Variable interpolation in macro steps
- [ ] Built-in macros installed on first run (requires filesystem)
- [ ] macro_execute creates isolated RequestContext (requires async)
- [ ] Macro context inherits tool scope from parent (requires async)
- [ ] Macro context has macro_flag set (requires async)

## Additional behaviors tested

- [x] resolve_variables: no variables, required provided, required missing errors
- [x] resolve_variables: default used, default overridden
- [x] resolve_variables: rest captures remaining args, rest with default
- [x] resolve_variables: multiple variables mixed
- [x] usage: no variables, required, optional, rest, rest+default, mixed
- [x] interpolate_command: single, multiple, no vars, missing var passthrough
- [x] YAML deserialization: with variables, with defaults, no variables

## Old code reference
- `src/config/macros.rs` — macro_execute, Macro struct
