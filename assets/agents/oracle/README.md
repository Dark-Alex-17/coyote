# Oracle

An AI agent specialized in high-level architecture, complex debugging, and design decisions.

This agent is designed to be delegated to by the **[Sisyphus](../sisyphus/README.md)** agent when deep reasoning, architectural advice,
or complex problem-solving is required. Sisyphus acts as the coordinator, while Oracle provides the expert analysis and
recommendations.

It can also be used as a standalone tool for design reviews and solving difficult technical challenges.

## Features

- 🏛️ System architecture and design patterns
- 🐛 Complex debugging and root cause analysis
- ⚖️ Tradeoff analysis and technology selection
- 📝 Code review and best practices advice
- 🧠 Deep reasoning for ambiguous problems

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
