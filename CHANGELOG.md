## v0.3.0 (2026-04-02)

### Feat

- Added `todo__clear` function to the todo system and updated REPL commands to have a .clear todo as well for significant changes in agent direction
- Added available tools to prompts for sisyphus and code-reviewer agent families
- Added available tools to coder prompt
- Improved token efficiency when delegating from sisyphus -> coder
- modified sisyphus agents to use the new ddg-search MCP server for web searches instead of built-in model searches
- Added support for specifying a custom response to multiple-choice prompts when nothing suits the user's needs
- Supported theming in the inquire prompts in the REPL
- Added the duckduckgo-search MCP server for searching the web (in addition to the built-in tools for web searches)
- Support for Gemini OAuth
- Support authenticating or refreshing OAuth for supported clients from within the REPL
- Allow first-runs to select OAuth for supported providers
- Support OAuth authentication flows for Claude
- Improved MCP server spinup and spindown when switching contexts or settings in the REPL: Modify existing config rather than stopping all servers always and re-initializing if unnecessary
- Allow the explore agent to run search queries for understanding docs or API specs
- Allow the oracle to perform web searches for deeper research
- Added web search support to the main sisyphus agent to answer user queries
- Created a CodeRabbit-style code-reviewer agent
- Added configuration option in agents to indicate the timeout for user input before proceeding (defaults to 5 minutes)
- Added support for sub-agents to escalate user interaction requests from any depth to the parent agents for user interactions
- built-in user interaction tools to remove the need for the list/confirm/etc prompts in prompt tools and to enhance user interactions in Loki
- Experimental update to sisyphus to use the new parallel agent spawning system
- Added an agent configuration property that allows auto-injecting sub-agent spawning instructions (when using the built-in sub-agent spawning system)
- Auto-dispatch support of sub-agents and support for the teammate pattern between subagents
- Full passive task queue integration for parallelization of subagents
- Implemented initial scaffolding for built-in sub-agent spawning tool call operations
- Initial models for agent parallelization
- Added interactive prompting between the LLM and the user in Sisyphus using the built-in Bash utils scripts

### Fix

- Clarified user text input interaction
- recursion bug with similarly named Bash search functions in the explore agent
- updated the error for unauthenticated oauth to include the REPL .authenticated command
- Corrected a bug in the coder agent that wasn't outputting a summary of the changes made, so the parent Sisyphus agent has no idea if the agent worked or not
- Claude code system prompt injected into claude requests to make them valid once again
- Do not inject tools when models don't support them; detect this conflict before API calls happen
- The REPL .authenticate command works from within sessions, agents, and roles with pre-configured models
- Implemented the path normalization fix for the oracle and explore agents
- Updated the atlassian MCP server endpoint to account for future deprecation
- Fixed a bug in the coder agent that was causing the agent to create absolute paths from the current directory
- the updated regex for secrets injection broke MCP server secrets interpolation because the regex greedily matched on new lines, replacing too much content. This fix just ignores commented out lines in YAML files by skipping commented out lines.
- Don't try to inject secrets into commented-out lines in the config
- Removed top_p parameter from some agents so they can work across model providers
- Improved sub-agent stdout and stderr output for users to follow
- Inject agent variables into environment variables for global tool calls when invoked from agents to modify global tool behavior
- Removed the unnecessary execute_commands tool from the oracle agent
- Added auto_confirm to the coder agent so sub-agent spawning doesn't freeze
- Fixed a bug in the new supervisor and todo built-ins that was causing errors with OpenAI models
- Added condition to sisyphus to always output a summary to clearly indicate completion
- Updated the sisyphus prompt to explicitly tell it to delegate to the coder agent when it wants to write any code at all except for trivial changes
- Added back in the auto_confirm variable into sisyphus
- Removed the now unnecessary is_stale_response that was breaking auto-continuing with parallel agents
- Bypassed enabled_tools for user interaction tools so if function calling is enabled at all, the LLM has access to the user interaction tools when in REPL mode
- When parallel agents run, only write to stdout from the parent and only display the parent's throbber
- Forgot to implement support for failing a task and keep all dependents blocked
- Clean up orphaned sub-agents when the parent agent
- Fixed the bash prompt utils so that they correctly show output when being run by a tool invocation
- Forgot to automatically add the bidirectional communication back up to parent agents from sub-agents (i.e. need to be able to check inbox and send messages)
- Agent delegation tools were not being passed into the {{__tools__}} placeholder so agents weren't delegating to subagents

### Refactor

- Made the oauth module more generic so it can support loopback OAuth (not just manual)
- Changed the default session name for Sisyphus to temp (to require users to explicitly name sessions they wish to save)
- Updated the sisyphus agent to use the built-in user interaction tools instead of custom bash-based tools
- Cleaned up some left-over implementation stubs

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
