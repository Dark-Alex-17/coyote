# Agents

Agents in Loki follow the same style as OpenAI's GPTs. They consist of 3 parts:

* [Role](./ROLES.md) - Tell the LLM how to behave
* [RAG](./RAG.md) - Pre-built knowledge bases specifically for the agent
* [Function Calling](./function-calling/TOOLS.md#tools) ([#2](./function-calling/MCP-SERVERS.md)) - Extends the functionality of the LLM through custom functions it can call

![Agent example](./images/agents/sql.gif)

Agent configuration files are stored in the `agents` subdirectory of your Loki configuration directory. The location of
this directory varies between systems so you can use the following command to locate yours:

```shell
loki --info | grep 'agents_dir' | awk '{print $2}'
```

If you're looking for more example agents, refer to the [built-in agents](../assets/agents).

## Quick Links
<!--toc:start-->
- [Directory Structure](#directory-structure)
- [Metadata](#1-metadata)
- [2. Define the Instructions](#2-define-the-instructions)
  - [Static Instructions](#static-instructions)
    - [Special Variables](#special-variables)
    - [User-Defined Variables](#user-defined-variables)
  - [Dynamic Instructions](#dynamic-instructions)
  - [Variables](#variables)
- [3. Initializing RAG](#3-initializing-rag)
- [4. Building Tools for Agents](#4-building-tools-for-agents)
  - [Limitations](#limitations)
  - [.env File Support](#env-file-support)
  - [Python-Based Agent Tools](#python-based-agent-tools)
  - [Bash-Based Agent Tools](#bash-based-agent-tools)
- [5. Conversation Starters](#5-conversation-starters)
- [6. Todo System & Auto-Continuation](#6-todo-system--auto-continuation)
- [Built-In Agents](#built-in-agents)
<!--toc:end-->

---

## Directory Structure
Agent configurations often have the following directory structure:

```
<loki-config-dir>/agents
    └── my-agent
        ├── config.yaml
        ├── tools.sh
            or
        ├── tools.py
```

This means that agent configurations often are only two files: the agent configuration file (`config.yaml`), and the 
tool definitions (`agents/my-agent/tools.sh` or `tools.py`).

To see a full example configuration file, refer to the [example agent config file](../config.agent.example.yaml).

The best way to understand how an agent is built is to go step by step in the following manner:

---

## 1. Metadata
Agent configurations have the following settings available to customize each agent:

```yaml
# Model Configuration
model: openai:gpt-4o                 # Specify the LLM to use
temperature: null                    # Set default temperature parameter, range (0, 1)
top_p: null                          # Set default top-p parameter, with a range of (0, 1) or (0, 2), depending on the model
# Agent Metadata Configuration
agent_session: null                  # Set a session to use when starting the agent. (e.g. temp, default); defaults to globally set agent_session
# Agent Configuration
name: <agent-name>                   # Name of the agent, used in the UI and logs
description: <description>           # Description of the agent, used in the UI
version: 1                           # Version of the agent
# Function Calling Configuration
mcp_servers:                         # Optional list of MCP servers that the agent utilizes
  - github                           # Corresponds to the name of an MCP server in the `<loki-config-dir>/functions/mcp.json` file
global_tools:                        # Optional list of additional global tools to enable for the agent; i.e. not tools specific to the agent
  - web_search
  - fs
  - python
# Todo System & Auto-Continuation (see "Todo System & Auto-Continuation" section below)
auto_continue: false                 # Enable automatic continuation when incomplete todos remain
max_auto_continues: 10               # Maximum continuation attempts before stopping
inject_todo_instructions: true       # Inject todo tool instructions into system prompt
continuation_prompt: null            # Custom prompt for continuations (optional)
```

As mentioned previously: Agents utilize function calling to extend a model's capabilities. However, agents operate in 
isolated environment, so in order for an agent to use a tool or MCP server that you have defined globally, you must 
explicitly state which tools and/or MCP servers the agent uses. Otherwise, it is assumed that the agent doesn't use any 
tools outside its own custom defined tools.

And if you don't define a `agents/my-agent/tools.sh` or `agents/my-agent/tools.py`, then the agent is really just a 
`role`.

You'll notice there's no settings for agent-specific tooling. This is because they are handled separately and 
automatically. See the [Building Tools for Agents](#4-building-tools-for-agents) section below for more information.

To see a full example configuration file, refer to the [example agent config file](../config.agent.example.yaml).

## 2. Define the Instructions
At their heart, agents function similarly to roles in that they tell the model how to behave. Agent configuration files
have the following settings for the instruction definitions:

```yaml
dynamic_instructions:     # Whether to use dynamically generated instructions for the agent; if false, static instructions are used. False by default.
instructions:             # Static instructions for the LLM; These are ignored if dynamic instructions are used
variables:                # An array of optional variables that the agent expects and uses
```

### Static Instructions
By default, Loki agents use statically defined instructions. Think of them as being identical to the instructions for a
[role](./ROLES.md#instructions), because they virtually are. 

**Example:**
```yaml
instructions: |
  You are an AI agent designed to demonstrate agentic capabilities
```

Just like roles, agents support variable interpolation at runtime. There's two types of variables that can be 
interpolated into the instructions at runtime: special variables (like roles have), and user-defined variables. Just 
like roles, variables are interpolated into your instructions anywhere Loki sees the `{{variable}}` syntax.

#### Special Variables
The following special variables are provided by Loki at runtime and can be injected into your agent's instructions:

| Name            | Description                                                         | Example                    |
|-----------------|---------------------------------------------------------------------|----------------------------|
| `__os__`        | Operating system name                                               | `linux`                    |
| `__os_family__` | Operating system family                                             | `unix`                     |
| `__arch__`      | System architecture                                                 | `x86_64`                   |
| `__shell__`     | The current user's default shell                                    | `bash`                     |
| `__locale__`    | The current user's preferred language and region settings           | `en-US`                    |
| `__now__`       | Current timestamp in ISO 8601 format                                | `2025-11-07T10:15:44.268Z` |
| `__cwd__`       | The current working directory                                       | `/tmp`                     |
| `__tools__`     | A list of the enabled tools (global + mcp servers + agent-specific) |                            |

#### User-Defined Variables
Agents also support user-defined variables that can be interpolated into the instructions, and are made available to any
agent-specific tools you define (see [Building Tools for Agents](#4-building-tools-for-agents) for more details on how to 
create agent-specific tooling).

The `variables` setting in an agent's config has the following fields:

| Field         | Required | Description                                                                                        |
|---------------|----------|----------------------------------------------------------------------------------------------------|
| `name`        | *        | The name of the variable                                                                           |
| `description` | *        | The description of the field                                                                       |
| `default`     |          | A default value for the field. If left undefined, the user will be prompted for a value at runtime |

These variables can be referenced in both the agent's instructions, and in the tool definitions via `LLM_AGENT_VAR_<name>`.

**Example:**
```yaml
instructions: |
  You are an agent who answers questions about a user's system.

  <tools>
  {{__tools__}}
  </tools>

  <system>
  os: {{__os__}}
  os_family: {{__os_family__}}
  arch: {{__arch__}}
  shell: {{__shell__}}
  locale: {{__locale__}}
  now: {{__now__}}
  cwd: {{__cwd__}}
  </system>

  <user>
  username: {{username}}
  </user>
variables:
  - name: username                 # Accessible from the tool definitions via the `LLM_AGENT_VAR_USERNAME` environment variable
    description: Your user name
```

### Dynamic Instructions
Sometimes you may find it useful to dynamically generate instructions on startup. Whether that be via a call to Loki
itself to generate them, or by some other means. Loki supports this type of behavior using a special function defined
in your `agents/my-agent/tools.py` or `agents/my-agent/tools.sh`.

**Example: Instructions for a JSON-reader agent that specializes on each JSON input it receives**
`agents/json-reader/tools.py`:
```python
import json
from pathlib import Path
from genson import SchemaBuilder

def _instructions():
    """Generates instructions for the agent dynamically"""
    value = input("Enter a JSON file path OR paste raw JSON: ").strip()
    if not value:
        raise SystemExit("A file path or JSON string is required.")

    p = Path(value)
    if p.exists() and p.is_file():
        json_file_path = str(p.resolve())
        json_text = p.read_text(encoding="utf-8")
    else:
        try:
            json.loads(value)
        except json.JSONDecodeError as e:
            raise SystemExit(f"Input is neither a file nor valid JSON.\n{e}")
        json_file_path = "<provided-inline-json>"
        json_text = value

    try:
        data = json.loads(json_text)
    except json.JSONDecodeError as e:
        raise SystemExit(f"Provided content is not valid JSON.\n{e}")

    builder = SchemaBuilder()
    builder.add_object(data)
    json_schema = builder.to_schema()
    return f"""
        You are an AI agent that can view and filter JSON data with jq.
        
        ## Context
        json_file_path: {json_file_path}
        json_schema: {json.dumps(json_schema, indent=2)}
    """
```

or

`agents/json-reader/tools.sh`:
```bash
#!/usr/bin/env bash
set -e

# @meta require-tools jq,genson
# @env LLM_OUTPUT=/dev/stdout The output path

# @cmd Generates instructions for the agent dynamically
_instructions() {
	read -r -p "Enter a JSON file path OR paste raw JSON: " value
	
	if [[ -z "${value}" ]]; then
		echo "A file path or JSON string is required" >&2
		exit 1
	fi 
	json_file_path=""
    inline_temp=""
    cleanup() {
      [[ -n "${inline_temp:-}" && -f "${inline_temp}" ]] && rm -f "${inline_temp}"
    }
    trap cleanup EXIT
    
    if [[ -f "${value}" ]]; then
      json_file_path="$(realpath "${value}")"
      if ! jq empty "${json_file_path}" >/dev/null 2>&1; then
        echo "Error: File does not contain valid JSON: ${json_file_path}" >&2
        exit 1
      fi
    else
      inline_temp="$(mktemp)"
      printf "%s" "${value}" > "${inline_temp}"
      if ! jq empty "${inline_temp}" >/dev/null 2>&1; then
        echo "Error: Input is neither a file nor valid JSON." >&2
        exit 1
      fi
      json_file_path="<provided-inline-json>"
    fi
    
    source_file="${json_file_path}"
    if [[ "${json_file_path}" == "<provided-inline-json>" ]]; then
      source_file="${inline_temp}"
    fi
    
    json_schema="$(genson < "${source_file}" | jq -c '.')"
	cat <<EOF >> "$LLM_OUTPUT"
You are an AI agent that can view and filter JSON data with jq.

## Context
json_file_path: ${json_file_path}
json_schema: ${json_schema}
EOF
}
```

For more information on how to create custom tools for your agent and the structure of the `agent/my-agent/tools.sh` or 
`agent/my-agent/tools.py` files, refer to the [Building Tools for Agents](#4-building-tools-for-agents) section below.

#### Variables
All the same variable interpolations supported by static instructions is also supported by dynamic instructions. For 
more information on what variables are available and how to use them, refer to the [Special Variables](#special-variables)
and [User-Defined Variables](#user-defined-variables) sections above.

## 3. Initializing RAG
Each agent you create also has a dedicated knowledge base that adds additional context to your queries and helps the LLM
answer queries effectively. The documents to load into RAG are defined in the `documents` array of your agent 
configuration file:

```yaml
documents:
  - https://www.ohdsi.org/data-standardization/
  - https://github.com/OHDSI/Vocabulary-v5.0/wiki/**
  - OMOPCDM_ddl.sql       # Relative path to agent (i.e. file lives at '<loki-config-dir>/agents/my-agent/OMOPCDM_ddl.sql')
```

These documents use the same syntax as those you'd define when constructing RAG normally. To see all the available types
of documents that Loki supports and how to use custom document loaders, refer to the [RAG documentation](./RAG.md#supported-document-sources).

Anytime your agent starts up, it will automatically be using the RAG you've defined here.

## 4. Building Tools for Agents
Building tools for agents is virtually identical to building custom tools, with one slight difference: instead of 
defining a single function that gets executed at runtime (e.g. `main` for bash tools and `run` for Python tools), agent
tools define a number of *subcommands*.

### Limitations
You can only utilize either a bash-based `<loki-config-dir>/agents/my-agent/tools.sh` or a Python-based 
`<loki-config-dir>/agents/my-agent/tools.py`. However, if it's easier to achieve a task in one language vs the other, 
you're free to define other scripts in your agent's configuration directory and reference them from the main 
`tools.py/sh` file. **Any scripts *not* named `tools.{py,sh}` will not be picked up by Loki's compiler**, meaning they 
can be used like any other set of scripts.

It's important to keep in mind the following:

* **Do not give agents the same name as an executable**. Loki compiles the tools for each agent into a binary that it
  temporarily places on your path during execution. If you have a binary with the same name as your agent, then your 
  shell may execute the existing binary instead of your agent's tools
* **`LLM_ROOT_DIR` points to the agent's configuration directory**. This is where agents differ slightly from normal 
  tools: The `LLM_ROOT_DIR` environment variable does *not* point to the `functions/tools` directory like it does in 
  global tools. Instead, it points to the agent's configuration directory, making it easier to source scripts and other
  miscellaneous files

### .env File Support
When Loki loads an agent, it will also search the agent's configuration directory for a `.env` file. If found, all 
environment variables defined in the file will be made available to the agent's tools.

### Python-Based Agent Tools
Python-based tools are defined exactly the same as they are for custom tool definitions. The only difference is that 
instead of a single `run` function, you define as many as you like with whatever arguments you like.

**Example:**
`agents/my-agent/tools.py`
```python
import urllib.request

def get_ip_info():
  """
  Get your IP information
  """
  with urllib.request.urlopen("https://httpbin.org/ip") as response:
    data = response.read()
    return data.decode('utf-8')

def get_ip_address_from_aws():
    """
    Find your public IP address using AWS
    """
    with urllib.request.urlopen("https://checkip.amazonaws.com") as response:
        data = response.read()
        return data.decode('utf-8')
```

Loki automatically compiles these as separate functions for the LLM to call. No extra work is needed. Just make sure you
follow all the same steps to define each function as you would when creating custom Python tools.

For more information on how to build tools in Python, refer to the [custom Python tools documentation](./function-calling/CUSTOM-TOOLS.md#custom-python-based-tools)

### Bash-Based Agent Tools
Bash-based agent tools are virtually identical to custom bash tools, with only one difference. Instead of defining a 
single entrypoint via the `main` function, you actually define as many subcommands as you like.

**Example:**
`agents/my-agent/tools.sh`
```bash
#!/usr/bin/env bash

# @env LLM_OUTPUT=/dev/stdout The output path
# @describe Discover network information about your computer and its place in the internet

# Use the `@cmd` annotation to define subcommands for your script.
# @cmd Get your IP information
get_ip_info() {
  curl -fsSL https://httpbin.org/ip >> "$LLM_OUTPUT"
}

# @cmd Find your public IP address using AWS
get_ip_address_from_aws() {
  curl -fsSL https://checkip.amazonaws.com >> "$LLM_OUTPUT"
}
```
To compile the script so it's executable and testable:
```bash
$ loki --build-tools
```

Then you can execute your script (assuming your current working directory is `agents/my-agent`):
```bash
$ ./tools.sh get_ip_info
$ ./tools.sh get_ip_address_from_aws
```

All other special annotations (`@env`, `@arg`, `@option` `@flags`) apply to subcommands as well, so be sure to follow 
the same syntax ad formatting as is used to create custom bash tools globally.

For more information on how to write, [build and test](function-calling/CUSTOM-BASH-TOOLS.md#execute-and-test-your-bash-tools) tools in bash, refer to the 
[custom bash tools documentation](function-calling/CUSTOM-BASH-TOOLS.md).

## 5. Conversation Starters
It's often helpful to also have some conversation starters so users know what kinds of things the agent is capable of 
doing. These are available in the REPL via the `.starter` command and are selectable.

They are defined using the `conversation_starters` setting in your agent's configuration file:

**Example:**
`agents/my-agent/config.yaml`:
```yaml
conversation_starters:
  - What is my username?
  - What is my current shell?
  - What is my ip?
  - How much disk space is left on my PC??
  - How to create an agent?
```

![Example Conversation Starters](./images/agents/conversation-starters.gif)

## 6. Todo System & Auto-Continuation

Loki includes a built-in task tracking system designed to improve the reliability of agents, especially when using
smaller language models. The Todo System helps models:

- Break complex tasks into manageable steps
- Track progress through multi-step workflows
- Automatically continue work until all tasks are complete

### Quick Configuration

```yaml
# agents/my-agent/config.yaml
auto_continue: true              # Enable auto-continuation
max_auto_continues: 10           # Max continuation attempts
inject_todo_instructions: true   # Include the default todo instructions into prompt
```

### How It Works

1. When `inject_todo_instructions` is enabled, agents receive instructions on using four built-in tools:
    - `todo__init`: Initialize a todo list with a goal
    - `todo__add`: Add a task to the list
    - `todo__done`: Mark a task complete
    - `todo__list`: View current todo state
   
   These instructions are a reasonable default that detail how to use Loki's To-Do System. If you wish, 
   you can disable the injection of the default instructions and specify your own instructions for how 
   to use the To-Do System into your main `instructions` for the agent.

2. When `auto_continue` is enabled and the model stops with incomplete tasks, Loki automatically sends a
   continuation prompt with the current todo state, nudging the model to continue working.

3. This continues until all tasks are done or `max_auto_continues` is reached.

### When to Use

- Multistep tasks where the model might lose track
- Smaller models that need more structure
- Workflows requiring guaranteed completion of all steps

For complete documentation including all configuration options, tool details, and best practices, see the
[Todo System Guide](./TODO-SYSTEM.md).

## Built-In Agents
Loki comes packaged with some useful built-in agents:

* `coder`: An agent to assist you with all your coding tasks
* `demo`: An example agent to use for reference when learning to create your own agents
* `explore`: An agent designed to help you explore and understand your codebase
* `jira-helper`: An agent that assists you with all your Jira-related tasks
* `oracle`: An agent for high-level architecture, design decisions, and complex debugging
* `sisyphus`: A powerhouse agent for writing complex code and acting as a natural language interface for your codebase (similar to ClaudeCode, Gemini CLI, Codex, or OpenCode)
* `sql`: A universal SQL agent that enables you to talk to any relational database in natural language
