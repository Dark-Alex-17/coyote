# Test Plan: Macros

## Behaviors to test
- [ ] Macro loaded from YAML file
- [ ] Macro steps executed sequentially
- [ ] Each step runs through run_repl_command
- [ ] Variable interpolation in macro steps
- [ ] Built-in macros installed on first run
- [ ] macro_execute creates isolated RequestContext
- [ ] Macro context inherits tool scope from parent
- [ ] Macro context has macro_flag set

## Old code reference
- `src/config/macros.rs` — macro_execute, Macro struct
