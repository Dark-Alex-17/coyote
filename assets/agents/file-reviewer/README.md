# File Reviewer

A specialized worker agent that reviews a single file's diff for bugs, style issues, and cross-cutting concerns.

This agent is designed to be spawned by the **[Code Reviewer](../code-reviewer/README.md)** agent. It focuses deeply on 
one file while communicating with sibling agents to catch issues that span multiple files.

## Features

- 🔍 **Deep Analysis**: Focuses on bugs, logic errors, security issues, and style problems in a single file.
- 🗣️ **Teammate Communication**: Sends and receives alerts to/from sibling reviewers about interface or dependency 
  changes.
- 🎯 **Targeted Reading**: Reads only relevant context around changed lines to stay efficient.
- 🏷️ **Structured Findings**: Categorizes issues by severity (🔴 Critical, 🟡 Warning, 🟢 Suggestion, 💡 Nitpick).

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

# ...
```

