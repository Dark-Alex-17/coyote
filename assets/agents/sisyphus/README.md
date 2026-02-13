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
