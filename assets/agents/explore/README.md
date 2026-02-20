# Explore

An AI agent specialized in exploring codebases, finding patterns, and understanding project structures.

This agent is designed to be delegated to by the **[Sisyphus](../sisyphus/README.md)** agent to gather information and context. Sisyphus
acts as the coordinator/architect, while Explore handles the research and discovery phase.

It can also be used as a standalone tool for understanding codebases and finding specific information.

## Features

- 🔍 Deep codebase exploration and pattern matching
- 📂 File system navigation and content analysis
- 🧠 Context gathering for complex tasks
- 🛡️ Read-only operations for safe investigation

## Pro-Tip: Use an IDE MCP Server for Improved Performance
Many modern IDEs now include MCP servers that let LLMs perform operations within the IDE itself and use IDE tools. Using
an IDE's MCP server dramatically improves the performance of coding agents. So if you have an IDE, try adding that MCP
server to your config (see the [MCP Server docs](../../../docs/function-calling/MCP-SERVERS.md) to see how to configure
them), and modify the agent definition to look like this:

```yaml
# ...

mcp_servers:
  - jetbrains # The name of your configured IDE MCP server

global_tools:
  - fs_read.sh
  - fs_grep.sh
  - fs_glob.sh
  - fs_ls.sh
  - web_search_loki.sh

# ...
```
