## v0.7.3 (2026-06-24)

### Fix

- apply bootstrapping of functions at startup to fix edge case

## v0.7.2 (2026-06-19)

### Fix

- usql version upgrade

## v0.7.1 (2026-06-19)

### Fix

- sbx mixins must be passed in directories, not as files and the files must be named spec.yaml per new sbx version

## v0.7.0 (2026-06-18)

### Feat

- added configurable cache path via the COYOTE_CACHE_PATH environment variable
- added a memory option to .set tab completions
- Added a diagnostic .info tools subcommand to make it easier to see what tools are enabled in all contexts
- Added additional info outputs for enabled skills and sbx directories
- directly execute shell commands from within the REPL
- created mixin kit for built-in functions and MCP servers
- Added sbx mixins for the secrets providers so users can also bootstrap those as well.
- added support for loading sbx mixins that are dynamically discovered in the users workspace and config directory
- Added a --fresh flag to let users create a truly bare bones sandbox without bootstrapping their config
- initial built-in sandboxing support powered by Docker sbx
- Added the ability to auto-bootstrap workspace memory when in git repos
- Added explicit guardrail handling for pending agents
- auto-append memory to memory index and don't necessarily require the LLM to remember to do it after a write
- Added an --init-memory [global|workspace] flag to easily and quickly enable memory
- added memory global configuration settings to the output of --info and .info
- added .set memory REPL commands to control memory injection and applied formatting
- Create the built-in memory management tools
- Append the memory system prompts (readonly or r/w) to the system prompt when applicable
- Created the --no-memory CLI flag to disable memory for this invocation
- Added the memory configuration properties and storage to the main app config, roles, sessions, and agents.
- initial scaffolding of a memory system

### Fix

- rebuild the tool scope after dynamically updating the skills_enabled value in the REPL
- properly resolve Windows-based local vault password file locations and bootstrap them into the sandbox when possible
- auto-translation of user-prefixed Mac and Linux paths for the vault password file when running inside a sandbox
- don't attempt to auto complete .vault list in the REPL; that's the end of the command
- buffer tool stdout as well as stderr so that any tools that error to stdout are captured and included in the response to the model, enabling the model to see what went wrong and to reason about how to fix it.
- auto-bootstrapped memory was accidentally putting the MEMORY.md directly in the repo root rather than .coyote/memory/MEMORY.md
- improved the fs_patch script description and added improved error handling to it.
- added in forgotten require_max_tokens to the fable model
- append memory functions to non-graph based agents on init
- when auto_continue is disabled via the .set auto_continue false command, it should strip the todo functions from the list of functions
- use rawPredict for non-streaming Claude requests

### Refactor

- Migrated the .skills command completion to use StateFlags and updated the help messages

## v0.6.0 (2026-06-05)

### Feat

- added skill hint prompt injection and configuration
- Fallthrough on missing secrets during mcp.json merging
- validate visible_skills field at config load time
- implemented reflexion (sorta) in sisyphus for significant code changes to delegate to the code-reviewer agent
- improved explore agent
- removed conditional fallback of LLM_*_RAW_JSON from built-ins
- updated enabled_skills handling to support both list and comma-separated strings
- added new REPL set commands for toggling skills and changing what skills are enabled
- upgraded to the latest version of mcp-remote
- fs_grep now works with both files and directories
- improved code reviewer agents with skills
- added round trip validation for vault providers to ensure permissions and authentication
- created new first-time run wizard for secrets provider
- vault_password_file or nothing at all is shorthand for just using the local gman provider for secret management
- refactored gman usage to be generic and work with various vault providers and use the SupportedProvider enum directly for configurations
- created initial parity gman generalization for vault provider
- Refactored the sisyhpus agent system to utilize the new skills system to improve performance and reliability
- llm graph nodes support skills
- updated sisyphus and coder tools
- removed potentially confusing tab completions for .skill
- .edit skill <name> support from within the REPL
- Added skills_dir to the info output of Coyote
- Created a few auto built-in skills
- Added support for auto_unload skills during chat
- cleaned up skill implementation
- support multiple skill flags to load multiple skills at CLI startup
- Modified --skill CLI to allow users to specify skills to start the REPL or CLI with.
- added CLI --skill flag for modifying skills easily
- REPL integration with skills
- dynamic loading/unloading of skill tools and MCP servers whenever load_skill/unload_skill are invoked
- created built-in functions for listing, loading, and unloading skills
- implemented the skills policy to track available skills per context
- added remote install and install support for skills
- created the skill registry
- decided to make skills persist to disk like agents and not in-memory like built-in roles
- scaffold skill module

### Fix

- disable skills for specific built-in roles
- redirect stderr into user's /dev/tty for guards
- azure doesn't support underscores in key vault
- accidental regression on enabled_skills being empty = all
- greedy secrets regex caused multiple secrets on one line to fail
- add agent context check to skill visibility validation
- enforced global visible_skills in llm node validation and improved skill loading error handling across the project
- restore agent skill policy on error during effective policy calculation
- apply the same validation for skill filenames on list_skills as happens everywhere else
- the vault's init_bare should try to load the provisioned secret_provider from the config file without also interpolating any of the rest of the configuration file. It should only fail if the user has not yet created a configuration file; i.e. done a first-time run.
- the vault roundtrip test used characters that are unsupported by some major secrets providers
- fixed tool filtering logic for skills and user functions in agents
- privilege leak when unloading skills and leaving tool scope untouched
- When bootstrapping an app config to interpolate secrets, clone the secrets provider configuration as well so config secrets stored in remote vaults can be used properly
- forgot to move back up the vault probe value error to be before the delete
- don't silently fail on skill role composition extraction in llm nodes
- set -euo pipefail for the temp script in execute_command.sh tool
- added forgotten skill name validation to has_skill to prevent side-channel attacks
- use unique values for the secrets round trip verification
- stop interpolating a line if any errors occur
- added path validation for skill names
- effective_policy unconditionally overwrote skill values for role-like structs
- updated execute_command to not mangle heredocs and also added explicit instructions to the coder and sisyphus agents to use fs_write and fs_patch over execute_command when writing files
- llm nodes accidentally skipped skill_registry::effective_role because I was passing an inline role instead
- updated temperature values for all agents and roles
- added back in require_max_tokens for new Claude models
- skill support also requires function calling to be enabled
- non_tty tests break on some TTY terminals
- skill loading on agents
- forgot to bootstrap skills on REPL startup
- remove now deprecated .skill edit command

### Refactor

- removed redundant skill name validation from has_skill function
- support both CSV and list formats for enabled_tools
- Support both CSV and list formats for enabled_mcp_servers

## v0.5.0 (2026-05-27)

### Feat

- rename Loki to Coyote

### Fix

- bash-based user interactions in agents accidentally regressed in graph implementation
- Claude function calling in agent contexts
- Claude code rate limit error per new Claude changes

## v0.4.0 (2026-05-23)

### Feat

- LLM node failures propgate up
- Added .install remote tab completions to the REPL
- feature complete install remote with category selection
- Support to interactively add secrets to Coyote that are missing from MCP configs when merging
- Added MCP config merging support for remote asset installations
- install remote now writes files to disk
- Created basic install_remote functions
- Created a more comprehensive and immediately useful default config for first runs
- Created an example graph-based agent called deep-research
- Improved coder agent that is now a graph-based agent
- Removed indicatif spinners. The UX just won't stop clobbering for parallel graph nodes
- Added agent variables support for graph agents and improved script executor to use the same environment variables as normal agent tool calling for further flexibility
- Improved UX with colored spinners for parallel graph agents and no clobbering outputs for sub-agents
- created new graph-based deep-research agent
- improved UX for parallel graph execution
- added branch progress tracker for better visualization of parallel graph super-steps
- Removed the jira-helper agent and replaced it with the atlassian role
- created the RenderMode enum to suppress stdout streaming during parallel graph super-steps
- Full support for map node types
- implemented the frontier-based scheduling for the graph executor with simplified state management (gotta love .clone)
- validation support for parallel graph execution; restricted map nodes to only run for nodes without next targets and not supporting chained map nodes
- created the staging area for state merges per super-step and created the built-in reducers (and their application) for the state merge phase of a super step
- scaffolding work for fan-out nodes for parallel branch execution support and stubbed out Map node types
- Coyote can now update itself via .update and --update commands
- added a .edit command for editing the MCP configuration file
- Created a new .install command to install bundled assets on-demand
- migrated llm node validation to graph loading time instead of graph runtime
- ripped out user input timeout scaffolding for approval and input node types; implementation can't be done cleanly
- added additional support for all RAG-configuration fields in RAG nodes
- initial support for RAG nodes in the graph execution system
- implemented structured logging for graph execution
- merged normal agent config and graph agent configs into one file (either/or)
- added structured-output extraction for llm and agent nodes
- created full llm node runtime implementation
- scaffolded together the initial llm node type and its executor
- wired together graph execution and agent graph dispatch
- implemented support for the graph executor
- created the approval node executor and the input node executor for user interaction
- Added initial support for native Coyote agent nodes in the graph-based agent system
- Added direct script invocation support for graph-based agents
- Added graph validation
- Implemented state management for agent graphs
- initial agent graph scaffolding
- add auto-continue support to all contexts
- dynamic tab completions now show the sessions for a given agent instead of only listing global sessions
- legacy SSE support for MCP server configurations
- support http/sse transport types for MCP server configurations so it fully supports claude desktop-style MCP configs
- 99% complete migration to new state structs to get away from God-Config struct; i.e. AppConfig, AppState, and RequestContext
- Automatic runtime customization using shebangs
- Created a demo TypeScript tool and a get_current_weather function in TypeScript
- Updated the Python demo tool to show all possible parameter types and variations
- Added TypeScript tool support using the refactored common ScriptedLanguage trait

### Fix

- Generified the functions usage of script detection for an executable bit on unix systems
- merge required claude code system prompt into instructions
- updated argc argument passing in run-tool and run-agent scripts
- Added additional graph validation for parallel reads and writes with dependencies between nodes states
- bug in next_single method and improved outcome handling for LLM node execution
- inline RAG bug when globbing files by extension without subdirectory globbing
- update the estimate_token_length function to use the standard word count method
- removed unnecessary regenerate logic for sessions and use the same logic for all contexts; prevents a panic on empty message list
- error when users try to start a session on a graph agent
- added on_other field for approval nodes so users can specify an alternative free-text target when none of the options match what they want
- accidentally added back in full agent tools on LLM nodes
- Improve the coder agent's usage of tools
- make the agent__collect escalation-aware so it doesn't freeze on sub-agent escalations
- check for an existing session before starting up MCP servers when switching to a role
- do not switch to agent if a session is active.
- Do not append todo instructions when function calling is disabled
- a bug in the dynamic completions because the crate name is coyote-ai but the binary is named coyote
- bug found by copilot that would create a lock on the PollSender for sse-based MCP servers
- Accidental shadow of temp_file function for Windows function calling
- upgraded to newer rmcp version to get native-tls support
- RagCache was not being used for agent and sub-agent instantiation
- TypeScript function args were being passed as objects rather than direct parameters
- Added in forgotten wrapper scripts for TypeScript tools
- don't shadow variables in binary path handling for Windows
- Tool call improvements for Windows systems

### Refactor

- migrated llm nodes to use Roles to simplify instructions handling and to function like inline roles
- migrated the next_node and apply_state_updates logic for LLM nodes into the LlmExecutor
- fully complete state re-architecting
- Fully ripped out the god Config struct
- Deprecated old Config struct initialization logic
- migrate functions and MCP servers to AppConfig
- Migrate the vault/bare_init logic
- created a single install_builtins free function to remove from Config::init
- partial migration to init in AppConfig
- Extracted common Python parser logic into a common.rs module
- python tools now use tree-sitter queries instead of AST

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
- built-in user interaction tools to remove the need for the list/confirm/etc prompts in prompt tools and to enhance user interactions in Coyote
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
- Created the Sisyphus agent to make Coyote function like Claude Code, Gemini, Codex, etc.
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
- added REPL support for interacting with the Coyote vault
- Integrated gman with Coyote to create a vault and added flags to configure the Coyote vault
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
