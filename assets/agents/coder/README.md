# Coder

An AI agent that assists you with your coding tasks.

This agent is designed to be delegated to by the **[Sisyphus](../sisyphus/README.md)** agent to implement code specifications. Sisyphus
acts as the coordinator/architect, while Coder handles the implementation details.

## Features

- 🏗️ Intelligent project structure creation and management
- 🖼️ Convert screenshots into clean, functional code
- 📁 Comprehensive file system operations (create folders, files, read/write files)
- 🧐 Advanced code analysis and improvement suggestions
- 📊 Precise diff-based file editing for controlled code modifications

It can also be used as a standalone tool for direct coding assistance.

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
  # Keep useful read-only tools for reading files in other non-project directories
  - fs_read.sh
  - fs_grep.sh
  - fs_glob.sh
#  - fs_write.sh
#  - fs_patch.sh
  - execute_command.sh

# ...
```