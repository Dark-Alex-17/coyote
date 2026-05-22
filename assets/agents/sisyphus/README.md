# Sisyphus

The main coordinator agent for the Loki coding ecosystem, providing a powerful CLI interface for code generation and
project management similar to OpenCode, ClaudeCode, Codex, or Gemini CLI.

_Inspired by the Sisyphus and Oracle agents of OpenCode._

Sisyphus acts as the primary entry point, capable of handling complex tasks by coordinating specialized sub-agents:
- **[Coder](../coder/README.md)**: For implementation and file modifications.
- **[Explore](../explore/README.md)**: For codebase understanding and research.
- **[Oracle](../oracle/README.md)**: For architecture and complex reasoning.

## Features

- 🤖 **Coordinator**: Manages multi-step workflows and delegates to specialized agents.
- 💻 **CLI Coding**: Provides a natural language interface for writing and editing code.
- 🔄 **Task Management**: Tracks progress and context across complex operations.
- 🛠️ **Tool Integration**: Seamlessly uses system tools for building, testing, and file manipulation.

## Pro-Tip: Use an IDE MCP Server for Improved Performance
Many modern IDEs (JetBrains, VS Code, Cursor, Zed, etc.) expose MCP servers that let LLMs use IDE tools directly. Using
one dramatically improves the performance of coding agents. If you have one, add it to your loki config (see the
[MCP Server docs](../../../docs/function-calling/MCP-SERVERS.md)) and reference it in this agent's `mcp_servers:` list:

```yaml
# ...

mcp_servers:
  - your-ide-mcp-server

global_tools:
  - fs_read.sh
  - fs_grep.sh
  - fs_glob.sh
  - fs_ls.sh
  - web_search_loki.sh
  - execute_command.sh

# ...
```
