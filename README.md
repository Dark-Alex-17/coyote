# Loki: All-in-one, batteries-included LLM CLI Tool

![Test](https://github.com/Dark-Alex-17/loki/actions/workflows/ci.yml/badge.svg)
![LOC](https://tokei.rs/b1/github/Dark-Alex-17/loki?category=code)
[![crates.io link](https://img.shields.io/crates/v/loki-ai.svg)](https://crates.io/crates/loki-ai)
![Release](https://img.shields.io/github/v/release/Dark-Alex-17/loki?color=%23c694ff)
![Crate.io downloads](https://img.shields.io/crates/d/loki-ai?label=Crate%20downloads)
[![GitHub Downloads](https://img.shields.io/github/downloads/Dark-Alex-17/loki/total.svg?label=GitHub%20downloads)](https://github.com/Dark-Alex-17/loki/releases)

Loki is an all-in-one, batteries-included, LLM CLI tool featuring Shell Assistant, CLI & REPL Mode, RAG, AI Tools & 
Agents, and More.

It is designed to include a number of useful agents, roles, macros, and more so users can get up and running with Loki 
in as little time as possible.

![Agent example](./docs/images/agents/sql.gif)

Coming from [AIChat](https://github.com/sigoden/aichat)? Follow the [migration guide](./docs/AICHAT-MIGRATION.md) to get started.

## Quick Links
* [AIChat Migration Guide](./docs/AICHAT-MIGRATION.md): Coming from AIChat? Follow the migration guide to get started.
* [History](#history): A history of how Loki came to be.
* [Installation](#install): Install Loki
* [Getting Started](#getting-started): Get started with Loki by doing first-run setup steps.
* [REPL](./docs/REPL.md): Interactive Read-Eval-Print Loop for conversational interactions with LLMs and Loki.
  * [Custom REPL Prompt](./docs/REPL-PROMPT.md): Customize the REPL prompt to provide useful contextual information.
* [Vault](./docs/VAULT.md): Securely store and manage sensitive information such as API keys and credentials.
* [Shell Integrations](./docs/SHELL-INTEGRATIONS.md): Seamlessly integrate Loki with your shell environment for enhanced command-line assistance.
* [Function Calling](./docs/function-calling/TOOLS.md#Tools): Leverage function calling capabilities to extend Loki's functionality with custom tools
    * [Creating Custom Tools](./docs/function-calling/CUSTOM-TOOLS.md): You can create your own custom tools to enhance Loki's capabilities.
        * [Create Custom Python Tools](./docs/function-calling/CUSTOM-TOOLS.md#custom-python-based-tools)
        * [Create Custom Bash Tools](./docs/function-calling/CUSTOM-BASH-TOOLS.md)
            * [Bash Prompt Utilities](./docs/function-calling/BASH-PROMPT-HELPERS.md)
* [First-Class MCP Server Support](./docs/function-calling/MCP-SERVERS.md): Easily connect and interact with MCP servers for advanced functionality.
* [Macros](./docs/MACROS.md): Automate repetitive tasks and workflows with Loki "scripts" (macros).
* [RAG](./docs/RAG.md): Retrieval-Augmented Generation for enhanced information retrieval and generation.
* [Sessions](/docs/SESSIONS.md): Manage and persist conversational contexts and settings across multiple interactions.
* [Roles](./docs/ROLES.md): Customize model behavior for specific tasks or domains.
* [Agents](/docs/AGENTS.md): Leverage AI agents to perform complex tasks and workflows.
* [Environment Variables](./docs/ENVIRONMENT-VARIABLES.md): Override and customize your Loki configuration at runtime with environment variables.
* [Client Configurations](./docs/clients/CLIENTS.md): Configuration instructions for various LLM providers.
    * [Patching API Requests](./docs/clients/PATCHES.md): Learn how to patch API requests for advanced customization.
* [Custom Themes](./docs/THEMES.md): Change the look and feel of Loki to your preferences with custom themes.

---

## History
Loki originally started as a fork of the fantastic [AIChat CLI](https://github.com/sigoden/aichat). The purpose was to 
simply fix a bug in how MCP servers worked with AIChat so that I could specify different ones for agents. However, it 
has since evolved far beyond that and become a passion project with a life of its own!

Loki now has first class MCP server support (with support for local and remote servers alike), a built-in vault for 
interpolating secrets in configuration files, built-in agents, built-in macros, dynamic tab completions, integrated
custom functions (no `argc` dependency), improved documentation, and much more with many more plans for the future!

The original kudos goes out to all the developers of the wonderful AIChat project!

---

## Prerequisites
Loki requires the following tools to be installed on your system:
* [jq](https://github.com/jqlang/jq)
    * `brew install jq`
* [jira (optional)](https://github.com/ankitpokhrel/jira-cli/wiki/Installation) (For the `jira-helper` agent)
    * `brew tap ankitpokhrel/jira-cli && brew install jira-cli`
    * You'll need to [create a JIRA API token](https://id.atlassian.com/manage-profile/security/api-tokens) for authentication
    * Then, save it as an environment variable to your shell profile:
      ```sh
      # ~/.bashrc or ~/.zshrc
      export JIRA_API_TOKEN="your_jira_api_token_here"
      ```
    * Then run `jira init`, select installation type as `cloud`, and provide the required details to generate a config
      file for the Jira CLI.
* [usql](https://github.com/xo/usql) (For the `sql` agent)
    * `brew install xo/xo/usql`
* [docker](https://docs.docker.com/engine/install/)
* [uv](https://docs.astral.sh/uv/getting-started/installation/)
    * `curl -LsSf https://astral.sh/uv/install.sh | sh`

These tools are used to provide various functionalities within Loki, such as document processing, JSON manipulation,
interaction with Jira, and they are used within agents and tools.

## Install

### Cargo
If you have Cargo installed, then you can install `loki` from Crates.io:

```shell
cargo install loki-ai # Binary name is `loki`

# If you encounter issues installing, try installing with '--locked'
cargo install --locked loki-ai
```

### Homebrew (Mac/Linux)
To install Loki from Homebrew, install the `loki` tap. Then you'll be able to install `loki`:

```shell
brew tap Dark-Alex-17/loki
brew install loki

# If you need to be more specific, use:
brew install Dark-Alex-17/loki/loki
```

To upgrade `loki` using Homebrew:

```shell
brew upgrade loki
```

### Scripts
#### Linux/MacOS (`bash`)
You can use the following command to run a bash script that downloads and installs the latest version of `loki` for your
OS (Linux/MacOS) and architecture (x86_64/arm64):

```shell
curl -fsSL https://raw.githubusercontent.com/Dark-Alex-17/loki/main/install_loki.sh | bash
```

#### Windows/Linux/MacOS (`PowerShell`)
You can use the following command to run a PowerShell script that downloads and installs the latest version of `loki`
for your OS (Windows/Linux/MacOS) and architecture (x86_64/arm64):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "iwr -useb https://raw.githubusercontent.com/Dark-Alex-17/loki/main/scripts/install_loki.ps1 | iex"
```

### Manual
Binaries are available on the [releases](https://github.com/Dark-Alex-17/loki/releases) page for the following platforms:

| Platform       | Architecture(s) |
|----------------|-----------------|
| macOS          | x86_64, arm64   |
| Linux GNU/MUSL | x86_64, aarch64 |
| Windows        | x86_64, aarch64 |

#### Windows Instructions
To use a binary from the releases page on Windows, do the following:

1. Download the latest [binary](https://github.com/Dark-Alex-17/loki/releases) for your OS.
2. Use 7-Zip or TarTool to unpack the Tar file.
3. Run the executable `loki.exe`!

#### Linux/MacOS Instructions
To use a binary from the releases page on Linux/MacOS, do the following:

1. Download the latest [binary](https://github.com/Dark-Alex-17/loki/releases) for your OS.
2. `cd` to the directory where you downloaded the binary.
3. Extract the binary with `tar -C /usr/local/bin -xzf loki-<arch>.tar.gz` (Note: This may require `sudo`)
4. Now you can run `loki`!

## Getting Started
After installation, you can generate the configuration files and directories by simply running:

```sh
loki --info
```

Then, you need to set up the Loki vault by creating a vault password file. Loki will do this for you automatically and
guide you through the process when you first attempt to access the vault. So, to get started, you can run:

```sh
loki --list-secrets
```

### First Time Setup
In order for Loki to function correctly, you'll need to add a few secrets to the Loki vault so the MCP servers can
function.

**GitHub MCP Server:**
* `GITHUB_PERSONAL_ACCESS_TOKEN` - A GitHub Personal Access Token with `repo` and `workflow` scopes.
  See [Creating a GitHub Personal Access Token](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens)

#### Add the secrets to the Loki vault
You can add the secrets to the Loki vault using the following commands (First time use will prompt you to create a vault 
password file):
```sh
loki --add-secret GITHUB_PERSONAL_ACCESS_TOKEN
```

### Tab-Completions
You can also enable tab completions to make using Loki easier. To do so, add the following to your shell profile:
```shell
# Bash
# (add to: `~/.bashrc`)
source <(COMPLETE=bash loki) 

# Zsh
# (add to: `~/.zshrc`)
source <(COMPLETE=zsh loki)

# Fish
# (add to: `~/.config/fish/config.fish`)
source <(COMPLETE=fish loki | psub)

# Elvish
# (add to: `~/.elvish/rc.elv`)
eval (E:COMPLETE=elvish loki | slurp)

# PowerShell
# (add to: `$PROFILE`)
$env:COMPLETE = "powershell"
loki | Out-String | Invoke-Expression
```

### Shell Integration
You can integrate Loki's Shell Assistant into your shell for enhanced command-line assistance. Add the code in the
corresponding [shell integration script](./scripts/shell-integration) to your shell. Then, you can invoke Loki to convert natural language to 
shell commands by pressing `Alt-e`. For example:

```shell
$ find all markdown files<Alt-e>
# Will be converted to:
find . -name "*.md"
```

## Configuration
The location of the global Loki configuration varies between systems, so you can use the following command to find your
`config.yaml` file:

```shell
loki --info | grep 'config_file' | awk '{print $2}'
```

The configuration file consists of a number of settings. To see a full example configuration file with every setting
defined, refer to the [example configuration file](./config.example.yaml).

### Default LLM
The following settings are available to configure the default LLM that is used when you start Loki, and its
hyperparameters:

| Setting       | Description                                                                                                                                             |
|---------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| `model`       | The default LLM to use when no model is provided                                                                                                        |
| `temperature` | The default `temperature` parameter for all models (0,1); Used unless explicitly overridden                                                             |
| `top_p`       | The default `top_p` hyperparameter value to use for all models, with a range of (0,1) (or (0,2) for some models); <br>Used unless explicitly overridden |

### CLI Behavior
You can use the following settings to modify the behavior of Loki:

| Setting       | Default Value | Description                                                                                                                         |
|---------------|---------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `stream`      | `true`        | Controls whether to use stream-style APIs when querying for completions from LLM providers                                          |
| `save`        | `true`        | Controls whether to save each query/response to every model to `messages.md` for posterity; Useful for debugging                    |
| `keybindings` | `emacs`       | Specifies which keybinding schema to use; can either be `emacs` or `vi`                                                             |
| `editor`      | `null`        | What text editor Loki should use to edit the input buffer or session (e.g. `vim`, `emacs`, `nano`, `hx`); <br>Defaults to `$EDITOR` |
| `wrap`        | `no`          | Controls whether text is wrapped (can be `no`, `auto`, or some `<max_width>`                                                        |
| `wrap_code`   | `false`       | Enables or disables the wrapping of code blocks                                                                                     |

### Preludes
Preludes let you define the default behavior for the different operating modes of Loki. The available settings are
shown below:

| Setting         | Description                                                                                                                                                                                                                                                                                                 |
|-----------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `repl_prelude`  | This setting lets you specify a default `session` or `role` to use when starting Loki in [REPL](./docs/REPL.md) mode. <br>Values can be <ul><li>`role:<name>` to define a role</li><li>`session:<name>` to define a session</li><li>`<session>:<role>` to define both a session and a role to use</li></ul> |
| `cmd_prelude`   | This setting lets you specify a default `session` or `role` to use when running one-off queries in Loki via the CLI. <br>Values can be <ul><li>`role:<name>` to define a role</li><li>`session:<name>` to define a session</li><li>`<session>:<role>` to define both a session and a role to use</li></ul>  |
| `agent_session` | This setting is used to specify a default session that all agents should start into, unless otherwise specified in the agent configuration. (e.g. `temp`, `default`)                                                                                                                                        |

### Appearance
The appearance of Loki can be modified using the following settings:

| Setting       | Default Value | Description                                          |
|---------------|---------------|------------------------------------------------------|
| `highlight`   | `true`        | This setting enables or disables syntax highlighting |
| `light_theme` | `false`       | This setting toggles light mode in Loki              |

### Miscellaneous Settings
| Setting              | Default Value | Description                                                                                                      |
|----------------------|---------------|------------------------------------------------------------------------------------------------------------------|
| `user_agent`         | `null`        | The name of the `User-Agent` that should be passed in the `User-Agent` header on all requests to model providers |
| `save_shell_history` | `true`        | Enables or disables REPL command history                                                                         |

## Creator
* [Alex Clarke](https://github.com/Dark-Alex-17)