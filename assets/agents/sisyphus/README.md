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
Many modern IDEs now include MCP servers that let LLMs perform operations within the IDE itself and use IDE tools. Using
an IDE's MCP server dramatically improves the performance of coding agents. So if you have an IDE, try adding that MCP
server to your config (see the [MCP Server docs](../../../docs/function-calling/MCP-SERVERS.md) to see how to configure
them), and modify the agent definition to look like this:

```yaml
# ...

mcp_servers:
  - jetbrains

global_tools:
  - fs_read.sh
  - fs_grep.sh
  - fs_glob.sh
  - fs_ls.sh
  - web_search_loki.sh
  - execute_command.sh

# ...
```
