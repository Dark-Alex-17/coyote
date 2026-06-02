---
############################################
## Everything in this section is optional ##
############################################

# Role Configuration
name: <role-name>                     # The name of the role
model: openai:gpt-4o                  # The model to use for this role
temperature: 0.2                      # The temperature to use for this role when querying the model
top_p: 0                              # The top_p to use for this role when querying the model
enabled_tools: fs_ls,fs_cat           # A comma-separated list of tools to enable for this role
enabled_mcp_servers: github,gitmcp    # A comma-separated list of MCP servers to enable for this role
skills_enabled: true                  # Master switch for skills in this role (default: inherit from global).
                                      # Skills also require `function_calling_support: true` in the global config.
enabled_skills: git-master,ai-slop-remover  # Comma-separated list of skills available when this role is active.
                                      # Must be a subset of global `visible_skills`. Omit to inherit the global default.
prompt: null                          # A custom prompt to use for this role that will immediately query
                                      # the model for output instead of using the instructions below
# Auto-Continue (Todo System)
# The auto-continue system provides built-in task tracking for improved reliability.
# When enabled, the model can create todo lists and the system will automatically
# prompt it to continue when incomplete tasks remain.
# See the [Todo System documentation](https://github.com/Dark-Alex-17/coyote/wiki/TODO-System) for more information
auto_continue: false                  # Enable automatic continuation when incomplete todos remain (default: false)
max_auto_continues: 10                # Maximum number of automatic continuations before stopping (default: 10)
inject_todo_instructions: true        # Inject default todo tool usage instructions into the system prompt (default: true)
continuation_prompt: null             # Custom prompt used when auto-continuing. If null, uses built-in default
---
You are an expert at doing things. This is where you write the instructions for the role.
