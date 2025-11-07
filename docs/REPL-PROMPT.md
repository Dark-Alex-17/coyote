# Customize REPL Prompt

[//]: # (TODO link to this doc from the main README)
The prompt you see when you start the Loki REPL can be customized to your liking. This is achieved via the `left_prompt`
and `right_prompt` settings in the global Loki configuration file:

```yaml
left_prompt: '{color.red}{model}){color.green}{?session {?agent {agent}>}{session}{?role /}}{!session {?agent {agent}>}}{role}{?rag @{rag}}{color.cyan}{?session )}{!session >}{color.reset} '
right_prompt: '{color.purple}{?session {?consume_tokens {consume_tokens}({consume_percent}%)}{!consume_tokens {consume_tokens}}}{color.reset}'
```

The location of the global configuration file differs between systems, so you can use the following command to find your
global configuration file's location:

```shell
loki --info | grep 'config_file' | awk '{print $2}'
```

## Quick Links
<!--toc:start-->
- [Syntax](#syntax)
- [Variables](#variables)
<!--toc:end-->

## Syntax
The syntax for the prompts consists of plain text and templates contained in `{...}`. The plain text is 
printed exactly as given. 

The syntax for the templates `{...}` is as follows:

* `{variable}` - Replaced with the value of `variable`
* `{?variable <template>}` - Evaluate the `<template>` when `variable` is evaluated to `true`
* `{!variable <template>}` - Evaluate the `<template>` when `variable` is evaluated to `false`

Where a `<template>` is another expression consisting of plain text and/or more special computations inside `{...}`.

Variables are evaluated to also be "truthy"; that is, if a variable is undefined, it is considered to be the exact same
as if that variable's value was `false`.

**Example 1: Simple Boolean Usage**
For the prompt `{?variable yay}{!variable boo}`, if `variable=true`, then the output will be
```
yay
```

And if `variable=false`:
```
boo
```

**Example 2: Nested Expressions**
For the prompt `{?variable {!variable2 yay}>}`, and assuming
* `variable=true`
* `variable2=false`
the output will be
```
yay>
```

If `variable2=true`, the output will be empty.

If `variable=false`, the output will be empty.

## Variables
The following variables and output modifiers are available to you when you're creating your prompts:

```yaml
# Model Variables
model: openai:gpt-4            # The active model's full name
client_name: openai            # The name of the client serving the active model
model_name: gpt-4              # The aliased name of the active model
max_input_tokens: 4096         # The maximum number of input tokens for the active model

# Configuration Variables
temperature: 1.0               # The temperature for the active model
top_p: 0.9                     # The top_p for the active model
dry_run: true                  # Whether the given command is flagged to be a dry run
stream: false                  # Whether streaming responses are enabled
save: true                     # Whether shell history is saved
wrap: 120                      # The number of characters to allow before wrapping around output to the next line

# Role Variables
role: code                     # The active role 

# Session Variables
session: temp                  # The name of the active session
dirty: false                   # Whether the session settings have been updated but not persisted
consume_tokens: 200            # The number of tokens consumed
consume_percent: 1%            # The percentage of tokens consumed to the maximum input tokens
user_messages_len: 0           # The total number of sent user messages 

# RAG Variables
rag: temp                      # The name of the active RAG

# Agent Variables
agent: todo-sh                 # The name of the active agent

# ANSI COLORS
color.reset:
color.black:
color.dark_gray:
color.red:
color.light_red:
color.green:
color.light_green:
color.yellow:
color.light_yellow:
color.blue:
color.light_blue:
color.purple:
color.light_purple:
color.magenta:
color.light_magenta:
color.cyan:
color.light_cyan:
color.white:
color.light_gray:
```
