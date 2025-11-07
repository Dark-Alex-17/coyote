# Loki REPL Guide
In addition to being a CLI, Loki also has a built-in REPL (Read-Execute-Print-Loop). This enables users to quickly try
out prompts, commands, configurations, and everything in between without having to modify the same command every time.

You can enter the REPL by simply typing `loki` without any follow-up flags or arguments.
## Quick Links
<!--toc:start-->
- [Features](#features)
- [REPL Commands](#repl-commands)
    - [`.model` - Change the current LLM](#model---change-the-current-llm)
    - [`.role` - Role management](#role---role-management)
    - [`.prompt` - Set a temporary role using a prompt](#prompt---set-a-temporary-role-using-a-prompt)
    - [`.session` - Session management](#session---session-management)
    - [`.agent` - Chat with an AI agent](#agent---chat-with-an-ai-agent)
    - [`.rag` - Chat with documents](#rag---chat-with-documents)
    - [`.macro` - Execute a macro](#macro---execute-a-macro)
    - [`.file` - Read files and use them as input](#file---read-files-and-use-them-as-input)
    - [`.vault` - Manage the Loki vault](#vault---manage-the-loki-vault)
    - [`.continue` - Continue the previous response](#continue---continue-the-previous-response)
    - [`.regenerate` - Regenerate the last response](#regenerate---regenerate-the-last-response)
    - [`.copy` - Copy the last response to your clipboard](#copy---copy-the-last-response-to-your-clipboard)
    - [`.set` - Adjust runtime settings](#set---adjust-runtime-settings)
    - [`.edit` - Modify configuration files](#edit---modify-configuration-files)
    - [`.delete` - Delete configurations from Loki](#delete---delete-configurations-from-loki)
    - [`.info` - Display information about the current mode](#info---display-information-about-the-current-mode)
    - [`.exit` - Exit an agent/role/session/rag or the Loki REPL itself](#exit---exit-an-agentrolesessionrag-or-the-loki-repl-itself)
    - [`.help` - Show the help guide](#help---show-the-help-guide)
<!--toc:end-->

---

## Features
The REPL has features that are intended to make your Loki experience as easy and as enjoyable as possible! This includes
things like

* **Tab Autocompletion:** Every command in the REPL (i.e. everything that starts with a `.`) has fuzzy search auto 
  completions. 
  * `.<tab>` to complete REPL commands
  * `.model <tab>` to complete chat models
  * `.set <tab>` to complete configuration keys
  * `.set key <tab>` to complete configuration values
* **Multi-Line Prompts:** You can also type prompts that span more than one line to help organize your thoughts. This 
  can be done in the following ways:
  * `Ctrl-o` to open the current input buffer in your preferred editor (either the value of `editor` or `$EDITOR`)
  * You can paste multi-line text
  * You can type `:::` to start multi-line editing, and use `:::` to finish it.
  * And finally, you can use hotkeys like `{ctrl/shift/alt}+enter` or `ctrl-j` to insert a new line directly in the 
    REPL.
* **History Search** Press `ctrl+r` to search the REPL history, and navigate it with `↑↓`
* **Configurable Keybindings:** You can switch between `emacs` style keybindings or `vi` style keybindings
* [**Custom REPL Prompt:**](./REPL-PROMPT.md) You can even customize the REPL prompt to display information about the 
  current context in the prompt

---

## REPL Commands
All REPL commands begin with a `.` to indicate that they're not part of a prompt. The following list details the 
commands available in Loki:

### `.model` - Change the current LLM
When browsing models in the REPL, use the following legend to understand the purpose of each column in the model table:
```
openai:gpt-4o     128000 /     4096  |       5 /     15    👁 ⚒ 
|                 |            |             |       |     |  └─ supports function calling
|                 |            |             |       |     └─ support vision (multi-modal)
|                 |            |             |       └─ output price ($/1M)
|                 |            |             └─ input price ($/1M)
|                 |            |
|                 |            └─ max output tokens
|                 └─ max input tokens
└─ model id
```
![model](./images/repl/model.gif)

For more information about how to add models to Loki, refer to the [clients documentation](./clients/CLIENTS.md).

### `.role` - Role management
Loki offers the following commands to manage your roles:

| Command      | Description                                                             |
|--------------|-------------------------------------------------------------------------|
| `.role`      | Create or switch to a role                                              |
| `.info role` | Show information about the active role                                  |
| `.edit role` | Open the active role's configuration file in your preferred text editor |
| `.save role` | Save the active role and its configurations to a configuration file     |
| `.exit role` | Exit the active role                                                    |

![role](./images/roles/code.gif)

For more information about roles in Loki and how to build them, refer to the [roles documentation](./ROLES.md).

### `.prompt` - Set a temporary role using a prompt
If you need to create a temporary role that you want to discard after use, you use `.prompt`. `.prompt`-based roles 
cannot be persisted to a file and saved.

![prompt-role](./images/roles/prompt-role.gif)

### `.session` - Session management
Use the following commands to manage sessions in Loki:

| Command             | Description                                                                                 |
|---------------------|---------------------------------------------------------------------------------------------|
| `.session`          | Start or switch to a session                                                                |
| `.empty session`    | Clear all messages for the active session                                                   |
| `.compress session` | Compress the session messages using the `summarization_prompt` setting in the global config |
| `.info session`     | Display information about the active session                                                |
| `.edit session`     | Open the active session's configuration in your preferred text editor                       |
| `.save session`     | Save the active session to a `session` configuration file                                   |
| `.exit session`     | Exit the active session                                                                     |

![sessions](./images/sessions/sessions-example.gif)

For more information on sessions and how to use them in Loki, refer to the [sessions documentation](./SESSIONS.md).

### `.agent` - Chat with an AI agent
Loki lets you build OpenAI GPT-style agents. The following commands let you interact with and manage your agents in 
Loki:

| Command              | Description                                                |
|----------------------|------------------------------------------------------------|
| `.agent`             | Use an agent                                               |
| `.starter`           | Display and use conversation starters for the active agent |
| `.edit agent-config` | Open the agent configuration in your preferred text editor |
| `.info agent`        | Display information about the active agent                 |
| `.exit agent`        | Leave the active agent                                     |

![agent](./images/agents/sql.gif)

For more information on agents in Loki and how to create them, refer to the [agents documentation](./AGENTS.md).

### `.rag` - Chat with documents
RAG (Retrieval Augmented Generation) enables you to load documents into the LLM so you can ask questions about it or 
complete tasks using the documents as additional context.

| Command          | Description                                                                  |
|------------------|------------------------------------------------------------------------------|
| `.rag`           | Initialize or access a RAG                                                   |
| `.edit rag-docs` | Add or remove documents from the active RAG using your preferred text editor |
| `.rebuild rag`   | Rebuild the active RAG to accommodate document changes                       |
| `.sources rag`   | Show a works-cited of the sources used in the last query                     |
| `.info rag`      | Display information about the active RAG                                     |
| `.exit rag`      | Exit the active RAG                                                          |

![rag](./images/rag/persistent-rag.gif)

For more information about RAG in Loki and how to utilize it, refer to the [rag documentation](./RAG.md).

### `.macro` - Execute a macro
Macros in Loki are like "scripts" of commands that can be run in isolated environments; that means they do not use any
active settings and use the same settings they had when written. They are created/executed using the `.macro <name>` 
command.

![macro](./images/macros/macros-example.gif)

For more information on macros in Loki and how to create them, refer to the [macros documentation](./MACROS.md).

### `.file` - Read files and use them as input
Loki lets you specify any number of documents that you can load and use as ephemeral RAG to chat with the LLM. To see
what files or values you can pass to it, simply run the command `.file` with no arguments:

```shell
openai:gpt-4o)> .file
Usage: .file <file|dir|url|%%|cmd>... [-- <text>...]
```

![ephemeral-rag](./images/rag/ephemeral-rag.gif)

For more information about ephemeral RAG, refer to the [ephemeral RAG documentation](./RAG.md#ephemeral-rag).

### `.vault` - Manage the Loki vault
The Loki vault lets users store sensitive secrets and credentials securely so that there's no plaintext secrets
anywhere in your configurations.

![vault](./images/vault/vault-demo.gif)

For more information about the Loki vault, refer to the [vault documentation](./VAULT.md).

### `.continue` - Continue the previous response
When you have a response that exceeds the context length, you can use the `.continue` command to continue the generation
of the last response.

![continue](./images/repl/continue.gif)

### `.regenerate` - Regenerate the last response
If ever your response is interrupted, or you want to try generating it again, you can use the `.regenerate` command to do
this without having to retype your query:

![regenerate](./images/repl/regenerate.gif)

### `.copy` - Copy the last response to your clipboard
If you're trying to copy the last response (like copying some code), you can use the `.copy` command to copy the entire
last response to your system clipboard:

![copy](./images/repl/copy.gif)

### `.set` - Adjust runtime settings
You can use `.set` to adjust select settings at runtime. This is useful when you're experimenting with settings and want
to know how they'll affect Loki. To persist the changes you make, be sure to update them in the global configuration 
file.

![set](./images/repl/set.gif)

### `.edit` - Modify configuration files
The `.edit` command lets you modify configuration files for the current mode of the REPL. It will open the selected 
configuration in your preferred text editor. It lets you modify the following configurations:

* `.edit config` - Modify the global configuration
* `.edit role` - Modify the active role's configuration
* `.edit session` - Modify the active session's configuration
* `.edit agent-config` - Modify the active agent's configuration
* `.edit rag-docs` - Add or remove documents from the active RAG

### `.delete` - Delete configurations from Loki
The `.delete` command allows you to delete entities in Loki without having to directly run `rm -rf` on the configuration 
directory or file corresponding to the target entity. You can use it to delete the following entities:

* `.delete role` - Delete select roles
* `.delete session` - Delete select sessions
* `.delete macro` - Delete select macros
* `.delete rag` - Delete select RAGs
* `.delete agent-data` - Delete select agent's configurations and all tools

### `.info` - Display information about the current mode
The `.info` command provides useful information about different modes that Loki may be operating in. It's helpful if you
want a quick understanding of the system info, a role's configuration, an agent's configuration, etc.

The following entities are supported:

| Command         | Description                                                 |
|-----------------|-------------------------------------------------------------|
| `.info`         | Display system information (identical to the `--info` flag) |
| `.info role`    | Display information about the active role                   |
| `.info session` | Display information about the active session                |
| `.info agent`   | Display information about the active agent                  |
| `.info rag`     | Display information about the active RAG                    |

### `.exit` - Exit an agent/role/session/rag or the Loki REPL itself
The `.exit` command is used to move between modes in the Loki REPL.

| Command         | Description             |
|-----------------|-------------------------|
| `.exit role`    | Exit the active role    |
| `.exit session` | Exit the active session |
| `.exit agent`   | Exit the active agent   |
| `.exit rag`     | Exit the active RAG     |
| `.exit`         | Exit the Loki REPL      |

### `.help` - Show the help guide
Just like with any shell or REPL, you sometimes need a little help and want to know what commands are available to you.
That's when you use the `.help` command.