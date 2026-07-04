# Sisyphus

The main coordinator agent for the Coyote coding ecosystem, providing a powerful CLI interface for code generation and
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
- 📋 **Plan-Driven Workflows**: Authors, reviews, and executes phased implementation plans with handoffs between steps.

## Plan-Driven Workflows

For large features, Sisyphus supports a phased workflow backed by a plan repo (`plans/` with `steps/`, `handoffs/`, and
a rolling `NOTES.md`):

1. **Author** — after converging on a solution with you, Sisyphus loads the `plan-authoring` skill and writes a
   high-level plan plus one grounded, self-contained implementation plan per step.
2. **Review** — [Oracle](../oracle/README.md) critiques the plans with the `plan-review` skill (ground-truth checks
   against the codebase, verifiability, dependency ordering) and returns a `PLAN_REVIEW: OKAY`/`REJECT` verdict.
   Rejected plans are fixed before any code is written.
3. **Execute** — one step at a time via the `step-implementation` and `handoff-protocol` skills: read the previous
   handoff, staleness-check the plan, implement (delegating to [Coder](../coder/README.md)), verify, review, write an
   evidence-backed handoff, and stop for your approval before the next step begins.

## Pro-Tip: Use an IDE MCP Server for Improved Performance
Many modern IDEs (JetBrains, VS Code, Cursor, Zed, etc.) expose MCP servers that let LLMs use IDE tools directly. Using
one dramatically improves the performance of coding agents. If you have one, add it to your coyote config (see the
[MCP Server docs](https://github.com/Dark-Alex-17/loki/wiki/MCP-Servers)) and reference it in this agent's `mcp_servers:` list:

```yaml
# ...

mcp_servers:
  - your-ide-mcp-server

global_tools:
  - fs_read.sh
  - fs_grep.sh
  - fs_glob.sh
  - fs_ls.sh
  - web_search_coyote.sh
  - execute_command.sh

# ...
```
