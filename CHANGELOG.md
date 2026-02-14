## v0.2.0 (2026-02-14)

### Feat

- Simplified sisyphus prompt to improve functionality
- Supported the injection of RAG sources into the prompt, not just via the `.sources rag` command in the REPL so models can directly reference the documents that supported their responses
- Created the Sisyphus agent to make Loki function like Claude Code, Gemini, Codex, etc.
- Created the Oracle agent to handle high-level architectural decisions and design questions about a given codebase
- Updated the coder agent to be much more task-focused and to be delegated to by Sisyphus
- Created the explore agent for exploring codebases to help answer questions
- Use the official atlassian MCP server for the jira-helper agent
- Created fs_glob to enable more targeted file exploration utilities
- Created a new tool 'fs_grep' to search a given file's contents for relevant lines to reduce token usage for smaller models
- Created the new fs_read tool to enable controlled reading of a file
- Let agent level variables be defined to bypass guard protections for tool invocations
- Implemented a built-in task management system to help smaller LLMs complete larger multistep tasks and minimize context drift
- Improved tool and MCP invocation error handling by returning stderr to the model when it is available
- Added variable interpolation for conversation starters in agents
- Implemented retry logic for failed tool invocations so the LLM can learn from the result and try again; Also implemented chain loop detection to prevent loops
- Added gemini-3-pro to the supported vertexai models
- Added an environment variable that lets users bypass guard operations in bash scripts. This is useful for agent routing
- Added support for thought-signatures for Gemini 3+ models

### Fix

- Improved continuation prompt to not make broad todo-items
- Allow auto-continuation to work in agents after a session is compressed and if there's still unfinish items in the to-do list
- fs_ls and fs_cat outputs should always redirect to "$LLM_OUTPUT" including on errors.
- Claude tool calls work incorrectly when tool doesn't require any arguments or flags; would provide an empty JSON object or error on no args
- Fixed a bug where --agent-variable values were not being passed to the agents

## v0.1.3 (2025-12-13)

### Feat

- Improved MCP implementation to minimize the tokens needed to utilize it so it doesn't quickly overwhelm the token space for a given model

## v0.1.2 (2025-11-08)

### Refactor

- Gave the GitHub MCP server a default placeholder value that doesn't require the vault

## v0.1.1 (2025-11-08)

## v0.1.0 (2025-11-07)

### Refactor

- Updated to the most recent Rust version with 2024 syntax

## v0.0.1 (2025-11-07)

### Feat

- Added the agents directory to sysinfo output
- Added built-in macros
- Updated the example role configuration file to also have the prompt field
- Updated the code role
- Secret injection as environment variables into agent tools
- Removed the server functionality
- Require Vault set up for first-time setup so all passed in secrets can be encrypted right off the bat
- Added static completions via a --completions flag
- Support for secret injection into the global config file (API keys, for example)
- Improved MCP handling toggle handling
- Secret injection into the MCP configuration
- added REPL support for interacting with the Loki vault
- Integrated gman with Loki to create a vault and added flags to configure the Loki vault
- Added a default session to the jira helper to make interaction more natural
- Created the repo-analyzer role
- Created the coder and sql agents
- Cleaned the built-in functions to not have leftover dependencies
- Created additional built-in roles for slack, repo analysis, and github
- Install built-in agents
- Embedded baseline MCP config and global tools

### Fix

- Corrected a typo for sourcing the bash utility script in some agent definitions

### Refactor

- Changed the name of the summary_prompt setting to summary_context_prompt
- Renamed summarize_prompt setting to summarization_prompt
- Renamed the compress_threshold setting to compression_threshold
- Migrated around the location of some of the more large documents for documentation
- Factored out the macros structs from the large config module
- Refactored mcp_servers and function_calling to mcp_server_support and function_calling_support to make the purpose of the fields more clear
- Refactored the use_mcp_servers field to enabled_mcp_servers to make the purpose of the field more clear
- Refactored use_tools field to enabled_tools field to make the use of the field more clear
- Removed the use of the tools.txt file and added tool visibility declarations to the global configuration file
- Agents that depend on global tools now have all binaries compiled and stored in the agent's bin directory so multiple agents can run at once
- Removed the git MCP server and used the newer, better mcp-server-docker for local docker integration
- Renamed the argument for the --completions flag to SHELL
- Updated the instructions for the jira-helper agent
- Modified the default PS1 look
- Fixed a linting issue for Windows builds
- Changed the name of agent_prelude to agent_session to make its purpose more clear
- Removed leftover javascript function support; will not implement
