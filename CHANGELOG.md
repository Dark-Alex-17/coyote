## v0.1.2 (2025-11-08)

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
