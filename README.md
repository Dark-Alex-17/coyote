# Coyote: All-in-one, batteries-included LLM CLI Tool

![Test](https://github.com/Dark-Alex-17/coyote/actions/workflows/ci.yaml/badge.svg)
[![crates.io link](https://img.shields.io/crates/v/coyote-ai.svg)](https://crates.io/crates/coyote-ai)
![Release](https://img.shields.io/github/v/release/Dark-Alex-17/coyote?color=%23c694ff)
![Crate.io downloads](https://img.shields.io/crates/d/coyote-ai?label=Crate%20downloads)
[![GitHub Downloads](https://img.shields.io/github/downloads/Dark-Alex-17/coyote/total.svg?label=GitHub%20downloads)](https://github.com/Dark-Alex-17/coyote/releases)

Coyote is an all-in-one, batteries-included, LLM CLI tool featuring Shell Assistant, CLI & REPL Mode, RAG, AI Tools & 
Agents, and More.

It is designed to include a number of useful agents, roles, macros, and more so users can get up and running with Coyote 
in as little time as possible. You can also install entire bundles of agents, roles, macros, tools, and MCP servers from 
any git repository. See [Sharing Configurations](https://github.com/Dark-Alex-17/coyote/wiki/Sharing-Configurations) for more information.

![Agent example](https://raw.githubusercontent.com/wiki/Dark-Alex-17/coyote/images/agents/sql.gif)

Coming from [AIChat](https://github.com/sigoden/aichat)? Follow the [migration guide](https://github.com/Dark-Alex-17/coyote/wiki/AIChat-Migration) to get started.

## Quick Links
* [AIChat Migration Guide](https://github.com/Dark-Alex-17/coyote/wiki/AIChat-Migration): Coming from AIChat? Follow the migration guide to get started.
* [Installation](#install): Install Coyote
* [Getting Started](#getting-started): Get started with Coyote by doing first-run setup steps.
* [Sharing Configurations](https://github.com/Dark-Alex-17/coyote/wiki/Sharing-Configurations): Install bundles of agents, roles, macros, tools, and MCP servers from any git repo, and share your own.
* [REPL](https://github.com/Dark-Alex-17/coyote/wiki/REPL): Interactive Read-Eval-Print Loop for conversational interactions with LLMs and Coyote.
  * [Custom REPL Prompt](https://github.com/Dark-Alex-17/coyote/wiki/REPL-Prompt): Customize the REPL prompt to provide useful contextual information.
* [Vault](https://github.com/Dark-Alex-17/coyote/wiki/Vault): Securely store and manage sensitive information such as API keys and credentials.
* [Sandboxes](https://github.com/Dark-Alex-17/coyote/wiki/Sandboxes): Launch Coyote inside an isolated [Docker Sandbox](https://docs.docker.com/ai/sandboxes/) with one command. Host config and vault credentials are projected in automatically; everything else is delegated to the `sbx` CLI.
* [Shell Integrations](https://github.com/Dark-Alex-17/coyote/wiki/Shell-Integrations): Seamlessly integrate Coyote with your shell environment for enhanced command-line assistance.
* [Function Calling](https://github.com/Dark-Alex-17/coyote/wiki/Tools): Leverage function calling capabilities to extend Coyote's functionality with custom tools
    * [Creating Custom Tools](https://github.com/Dark-Alex-17/coyote/wiki/Custom-Tools): You can create your own custom tools to enhance Coyote's capabilities.
        * [Create Custom Python Tools](https://github.com/Dark-Alex-17/coyote/wiki/Custom-Tools#custom-python-based-tools)
        * [Create Custom TypeScript Tools](https://github.com/Dark-Alex-17/coyote/wiki/Custom-Tools#custom-typescript-based-tools)
        * [Create Custom Bash Tools](https://github.com/Dark-Alex-17/coyote/wiki/Custom-Bash-Tools)
            * [Bash Prompt Utilities](https://github.com/Dark-Alex-17/coyote/wiki/Bash-Prompt-Helpers)
* [First-Class MCP Server Support](https://github.com/Dark-Alex-17/coyote/wiki/MCP-Servers): Easily connect and interact with MCP servers for advanced functionality.
* [Macros](https://github.com/Dark-Alex-17/coyote/wiki/Macros): Automate repetitive tasks and workflows with Coyote "scripts" (macros).
* [RAG](https://github.com/Dark-Alex-17/coyote/wiki/RAG): Retrieval-Augmented Generation for enhanced information retrieval and generation.
* [Sessions](https://github.com/Dark-Alex-17/coyote/wiki/Sessions): Manage and persist conversational contexts and settings across multiple interactions.
* [Memory](https://github.com/Dark-Alex-17/coyote/wiki/Memory): Persistent file-based memory that survives across sessions. Bootstrap with `coyote --init-memory [global|workspace]`.
* [Roles](https://github.com/Dark-Alex-17/coyote/wiki/Roles): Customize model behavior for specific tasks or domains.
* [Skills](https://github.com/Dark-Alex-17/coyote/wiki/Skills): Modular knowledge or capability packs the LLM can load and unload mid-conversation. Multiple skills compose; instructions stack, tools and MCPs union.
* [Agents](https://github.com/Dark-Alex-17/coyote/wiki/Agents): Leverage AI agents to perform complex tasks and workflows, including sub-agent spawning, teammate messaging, and user interaction tools.
    * [Graph Agents](https://github.com/Dark-Alex-17/coyote/wiki/Graph-Agents): Define an agent as a declarative, YAML-driven workflow. A directed graph of typed nodes (LLM calls, scripts, approvals, user input, RAG retrieval, sub-agent spawns).
* [Todo System](https://github.com/Dark-Alex-17/coyote/wiki/TODO-System): Built-in task tracking for improved LLM reliability with smaller models.
* [Environment Variables](https://github.com/Dark-Alex-17/coyote/wiki/Environment-Variables): Override and customize your Coyote configuration at runtime with environment variables.
* [Client Configurations](https://github.com/Dark-Alex-17/coyote/wiki/Clients): Configuration instructions for various LLM providers.
    * [Authentication (API Key & OAuth)](https://github.com/Dark-Alex-17/coyote/wiki/Clients#authentication): Authenticate with API keys or OAuth for subscription-based access.
    * [Patching API Requests](https://github.com/Dark-Alex-17/coyote/wiki/Patches): Learn how to patch API requests for advanced customization.
* [Custom Themes](https://github.com/Dark-Alex-17/coyote/wiki/Themes): Change the look and feel of Coyote to your preferences with custom themes.
* [History](#history): A history of how Coyote came to be.

## Prerequisites
Coyote requires the following tools to be installed on your system:
* [jq](https://github.com/jqlang/jq)
    * `brew install jq`
* [usql](https://github.com/xo/usql) (For the `sql` agent)
    * `brew install xo/xo/usql`
* [docker](https://docs.docker.com/engine/install/)
* [uv](https://docs.astral.sh/uv/getting-started/installation/)
    * `curl -LsSf https://astral.sh/uv/install.sh | sh`
* [iwe](https://github.com/iwe-org/iwe) (`iwec`, for the built-in `iwe` MCP server that navigates large markdown knowledgebases)
    * **Homebrew:** `brew tap iwe-org/iwe && brew install iwe`
    * **Cargo:** `cargo install iwec`
* [ast-grep](https://ast-grep.github.io/) (for the built-in `ast_grep` structural code search tool, used by the `explore` agent)
    * **Homebrew:** `brew install ast-grep`
    * **Cargo:** `cargo install ast-grep --locked`
    * **npm:** `npm i -g @ast-grep/cli`
    * Optional: if `ast-grep` is not installed, the `ast_grep` tool reports it and agents fall back to `fs_grep`

These tools are used to provide various functionalities within Coyote, such as document processing, JSON manipulation,
etc., and they are used within agents and tools.

## Install

### Cargo
If you have Cargo installed, then you can install `coyote` from Crates.io:

```shell
cargo install coyote-ai # Binary name is `coyote`

# If you encounter issues installing, try installing with '--locked'
cargo install --locked coyote-ai
```

### Homebrew (Mac/Linux)
To install Coyote from Homebrew, install the `coyote` tap. Then you'll be able to install `coyote`:

```shell
brew tap Dark-Alex-17/coyote
brew install coyote

# If you need to be more specific, use:
brew install Dark-Alex-17/coyote/coyote
```

To upgrade `coyote` using Homebrew:

```shell
brew upgrade coyote
```

### Docker
Coyote is available as a Docker image on Docker Hub (`darkalex17/coyote`) for Linux amd64 and arm64.
Useful for CI, ephemeral environments, or anywhere you prefer not to install it natively.

```bash
docker pull darkalex17/coyote
docker run --rm -it darkalex17/coyote
```

To persist your configuration across container runs, mount your existing config directory:

```bash
docker run --rm -it \
  -v ~/.config/coyote:/home/agent/.config/coyote \
  darkalex17/coyote
```

If you use the local vault provider and want your vault credentials available in the container, also mount the password file:

```bash
docker run --rm -it \
  -v ~/.config/coyote:/home/agent/.config/coyote \
  -v ~/.coyote_password:/home/agent/.coyote_password:ro \
  darkalex17/coyote
```

### Scripts
#### Linux/MacOS (`bash`)
You can use the following command to run a bash script that downloads and installs the latest version of `coyote` for your
OS (Linux/MacOS) and architecture (x86_64/arm64):

```shell
curl -fsSL https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/install_coyote.sh | bash
```

#### Windows/Linux/MacOS (`PowerShell`)
You can use the following command to run a PowerShell script that downloads and installs the latest version of `coyote`
for your OS (Windows/Linux/MacOS) and architecture (x86_64/arm64):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "iwr -useb https://raw.githubusercontent.com/Dark-Alex-17/coyote/refs/heads/main/scripts/install_coyote.ps1 | iex"
```

### Manual
Binaries are available on the [releases](https://github.com/Dark-Alex-17/coyote/releases) page for the following platforms:

| Platform       | Architecture(s) |
|----------------|-----------------|
| macOS          | x86_64, arm64   |
| Linux GNU/MUSL | x86_64, aarch64 |
| Windows        | x86_64, aarch64 |

#### Windows Instructions
To use a binary from the releases page on Windows, do the following:

1. Download the latest [binary](https://github.com/Dark-Alex-17/coyote/releases) for your OS.
2. Use 7-Zip or TarTool to unpack the Tar file.
3. Run the executable `coyote.exe`!

#### Linux/MacOS Instructions
To use a binary from the releases page on Linux/MacOS, do the following:

1. Download the latest [binary](https://github.com/Dark-Alex-17/coyote/releases) for your OS.
2. `cd` to the directory where you downloaded the binary.
3. Extract the binary with `tar -C /usr/local/bin -xzf coyote-<arch>.tar.gz` (Note: This may require `sudo`)
4. Now you can run `coyote`!

## Updating
Coyote can update itself in place to the latest GitHub release. Run `coyote --update`
for the newest release, or `coyote --update v0.4.0` for a specific version:

```shell
coyote --update
coyote --update v0.4.0
```

The same is available from within the REPL via `.update` and `.update v0.4.0`.

If Coyote was installed with a package manager, prefer that package manager so its
records stay in sync with the binary on disk; i.e. `brew upgrade coyote` for Homebrew,
or `cargo install --locked coyote-ai` for Cargo.

When Coyote detects a package-manager install it prints a warning and asks for
confirmation. In a non-interactive shell (no TTY), pass `--force` to update
anyway:

```shell
coyote --update --force
```

## Getting Started
After installation, you can generate the configuration files and directories by simply running:

```sh
coyote --info
```

Then, you need to set up the Coyote vault by creating a vault password file. Coyote will do this for you automatically and
guide you through the process when you first attempt to access the vault. So, to get started, you can run:

```sh
coyote --list-secrets
```

### Authentication
Each client in your configuration needs authentication (with a few exceptions; e.g. ollama). Most clients use an API key
(set via `api_key` in the config or through the [vault](https://github.com/Dark-Alex-17/coyote/wiki/Vault)). For providers that support OAuth (e.g. Claude Pro/Max 
subscribers, Google Gemini), you can authenticate with your existing subscription instead:

```yaml
# In your config.yaml
clients:
  - type: claude
    name: my-claude-oauth
    auth: oauth # Indicate you want to authenticate with OAuth instead of an API key
```

```sh
coyote --authenticate my-claude-oauth
# Or via the REPL: .authenticate
```

For full details, see the [authentication documentation](https://github.com/Dark-Alex-17/coyote/wiki/Clients#authentication).

### Tab-Completions
You can also enable tab completions to make using Coyote easier. To do so, add the following to your shell profile:
```shell
# Bash
# (add to: `~/.bashrc`)
source <(COMPLETE=bash coyote) 

# Zsh
# (add to: `~/.zshrc`)
source <(COMPLETE=zsh coyote)

# Fish
# (add to: `~/.config/fish/config.fish`)
source <(COMPLETE=fish coyote | psub)

# Elvish
# (add to: `~/.elvish/rc.elv`)
eval (E:COMPLETE=elvish coyote | slurp)

# PowerShell
# (add to: `$PROFILE`)
$env:COMPLETE = "powershell"
coyote | Out-String | Invoke-Expression
```

### Shell Integration
You can integrate Coyote's Shell Assistant into your shell for enhanced command-line assistance. Add the code in the
corresponding [shell integration script](./scripts/shell-integration) to your shell. Then, you can invoke Coyote to convert natural language to 
shell commands by pressing `Alt-e`. For example:

```shell
$ find all markdown files<Alt-e>
# Will be converted to:
find . -name "*.md"
```

## Configuration
The location of the global Coyote configuration varies between systems, so you can use the following command to find your
`config.yaml` file:

```shell
coyote --info | grep 'config_file' | awk '{print $2}'
```

The configuration file consists of a number of settings. To see a full example configuration file with every setting
defined, refer to the [example configuration file](./config.example.yaml).

### Default LLM
The following settings are available to configure the default LLM that is used when you start Coyote, and its
hyperparameters:

| Setting       | Description                                                                                                                                             |
|---------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| `model`       | The default LLM to use when no model is provided                                                                                                        |
| `temperature` | The default `temperature` parameter for all models (0,1); Used unless explicitly overridden                                                             |
| `top_p`       | The default `top_p` hyperparameter value to use for all models, with a range of (0,1) (or (0,2) for some models); <br>Used unless explicitly overridden |

### CLI Behavior
You can use the following settings to modify the behavior of Coyote:

| Setting       | Default Value | Description                                                                                                                         |
|---------------|---------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `stream`      | `true`        | Controls whether to use stream-style APIs when querying for completions from LLM providers                                          |
| `save`        | `true`        | Controls whether to save each query/response to every model to `messages.md` for posterity; Useful for debugging                    |
| `keybindings` | `emacs`       | Specifies which keybinding schema to use; can either be `emacs` or `vi`                                                             |
| `editor`      | `null`        | What text editor Coyote should use to edit the input buffer or session (e.g. `vim`, `emacs`, `nano`, `hx`); <br>Defaults to `$EDITOR` |
| `wrap`        | `no`          | Controls whether text is wrapped (can be `no`, `auto`, or some `<max_width>`                                                        |
| `wrap_code`   | `false`       | Enables or disables the wrapping of code blocks                                                                                     |

### Preludes
Preludes let you define the default behavior for the different operating modes of Coyote. The available settings are
shown below:

| Setting         | Description                                                                                                                                                                                                                                                                                                 |
|-----------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `repl_prelude`  | This setting lets you specify a default `session` or `role` to use when starting Coyote in [REPL](https://github.com/Dark-Alex-17/coyote/wiki/REPL) mode. <br>Values can be <ul><li>`role:<name>` to define a role</li><li>`session:<name>` to define a session</li><li>`<session>:<role>` to define both a session and a role to use</li></ul> |
| `cmd_prelude`   | This setting lets you specify a default `session` or `role` to use when running one-off queries in Coyote via the CLI. <br>Values can be <ul><li>`role:<name>` to define a role</li><li>`session:<name>` to define a session</li><li>`<session>:<role>` to define both a session and a role to use</li></ul>  |
| `agent_session` | This setting is used to specify a default session that all agents should start into, unless otherwise specified in the agent configuration. (e.g. `temp`, `default`)                                                                                                                                        |

### Appearance
The appearance of Coyote can be modified using the following settings:

| Setting       | Default Value | Description                                          |
|---------------|---------------|------------------------------------------------------|
| `highlight`   | `true`        | This setting enables or disables syntax highlighting |
| `light_theme` | `false`       | This setting toggles light mode in Coyote              |

### Miscellaneous Settings
| Setting              | Default Value | Description                                                                                                      |
|----------------------|---------------|------------------------------------------------------------------------------------------------------------------|
| `user_agent`         | `null`        | The name of the `User-Agent` that should be passed in the `User-Agent` header on all requests to model providers |
| `save_shell_history` | `true`        | Enables or disables REPL command history                                                                         |

---

## History

Coyote began as a fork of [AIChat CLI](https://github.com/sigoden/aichat) and has since evolved into an independent project.

See [CREDITS.md](./CREDITS.md) for full attribution and background.

---

## Creator
* [Alex Clarke](https://github.com/Dark-Alex-17)
