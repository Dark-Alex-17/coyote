---
# Everything in this section is optional
name: <role-name>                 # The name of the role
model: openai:gpt-4o              # The model to use for this role
temperature: 0.2                  # The temperature to use for this role when querying the model
top_p: null                       # The top_p to use for this role when querying the model
enabled_tools: fs_ls,fs_cat       # A comma-separated list of tools to enable for this role
use_mcp_servers: github,gitmcp    # A comma-separated list of MCP servers to enable for this role
---
You are an expert at doing things. This is where I would write the instructions for the role.
