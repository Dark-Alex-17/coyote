# Macros
Macros are essentially Loki "scripts"; that is, a predefined sequence of REPL commands that automate repetitive tasks or
workflows. Macros run in isolated environments, ensuring that the macros don't inherit any pre-existing role, session, 
RAG, or agent state, and they will not affect your current context. 

This isolation ensures that your workspace remains clean and unaffected by macro operations.

![Macro Example](./images/macros/macros-example.gif)

For more information on Loki's REPL, refer to the [REPL](./REPL.md) documentation.

## Quick Links
<!--toc:start-->
- [Macro Definition](#macro-definition)
  - [Step Definitions](#step-definitions)
  - [Macro Variables](#macro-variables)
- [Built-In Macros](#built-in-macros)
<!--toc:end-->

---

## Macro Definition
Macros are defined as YAML files in the `macros` subdirectory of your Loki configuration directory. The Loki configuration 
directory can vary between systems, so to find the location of your macros config directory, you can use the following 
command:

```shell
loki --info | grep 'macros_dir' | awk '{print $2}'
```

Macro definitions are broken into two parts: the `steps` of the macro, and an optional `variables` section that lets 
users pass in variables to alter the behavior of the macro at runtime.

### Step Definitions
The step definitions for a macro are straightforward: They are simply the exact commands you would otherwise type in the
REPL. 

**Example: Macro to generate a git commit message**
`macros/generate-commit-message.yaml`
```yaml
steps:
  - .file `git diff` -- generate git commit message
```
Usage:
```shell
$ loki --macro generate-commit-message
>> .file `git diff` -- generate a git commit message
Add documentation on macros
```

For a full example configuration, refer to the [example macro configuration file](../config.macro.example.yaml) in the root of this project.

### Macro Variables
Sometimes it's useful to be able to modify the behavior of a macro at runtime. This is achieved with the `variables` 
array of the macro definition.

To pass variables to a macro, since they are just Loki scripts, the syntax is the same as it is for any other scripting 
language: You just pass them alongside your invocation.

**Example:**
```shell
$ loki --macro example-variable-macro first_argument second_argument
```

Each variable in the `variables` array has the following properties:
* `name` (Required): the name of the variable, which can be referenced in the actual steps of the macro using the 
  `{{name}}` syntax.
* `default` (Optional): A default value for the variable if no value is specified. If no default value is defined, and 
  no value is provided for the variable at runtime, Loki will error out.
* `rest` (Optional, Boolean): When set to `true`, this variable will collect all remaining arguments passed to the 
  macro. This behavior is only applicable when the variable is the last variable in the list. By default, this is 
  `false`.

The `variables` array is order-dependent; that is to say that all arguments passed to the macro are positional. So be
careful about the ordering if that is important to your macro's invocation.

**Example: Simple variable example to invoke an agent**
`macros/invoke-agent.yaml`
```yaml
variables:
  - name: agent                 # No default value means this must be defined at runtime
  - name: args
    rest: true                  # All remaining arguments to the macro are collected into this variable
    default: What can you do?   # This is used if no value is passed at runtime
steps:
  - .agent {{agent}}
  - '{{args}}'
```
Usage:
```shell
$ loki --macro invoke-agent sql
# or
$ loki --macro invoke-agent sql What tables are available?
```

For a full example configuration, refer to the [example macro configuration file](../config.macro.example.yaml) in the root of this project.

## Built-In Macros
Loki comes packaged with some useful built-in macros. These are also good examples if you're looking for more examples 
on how to make your own macros, so be sure to check out the [built-in macro definitions](../assets/macros) if you're 
looking for more examples.

* `generate-commit-message` - Generate a Git commit message based on the staged changes in the current directory
