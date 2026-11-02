# Credits

## Matt Pocock's Skills

The bundled `diagnosing-bugs`, `codebase-design`, and `grilling` skills, the
`architecture-reviewer` agent, and the code smell baseline in the bundled
`code-review` skill are adapted from
[mattpocock/skills](https://github.com/mattpocock/skills) by Matt Pocock,
licensed under the MIT License. The smell definitions trace back to Martin
Fowler's *Refactoring* (ch. 3); the deep-module vocabulary builds on John
Ousterhout's *A Philosophy of Software Design* and Michael Feathers'
*Working Effectively with Legacy Code*.

## Codex CLI

The full-screen terminal UI (`src/tui/`) borrows its overall shape from the
[Codex CLI](https://github.com/openai/codex) by OpenAI, licensed under the
[Apache License 2.0](https://github.com/openai/codex/blob/main/LICENSE): a
frame scheduler that folds every redraw request between two draws into one,
a streaming reply split into a stable part rendered once per width and a
live tail, a prompt modal that takes the composer's place, and a capture of
command output that never waits on a lingering child. The code is Coyote's
own; only the design was followed.

## AIChat
Coyote originally started as a fork of the fantastic
[AIChat CLI](https://github.com/sigoden/aichat). The initial goal was simply
to fix a bug in how MCP servers worked with AIChat, allowing different MCP
servers to be specified per agent. Since then, Coyote has evolved far beyond
its original scope and grown into a passion project with a life of its own.

Today, Coyote includes first-class MCP server support (for both local and remote
servers), a built-in vault for interpolating secrets in configuration files,
built-in agents and macros, dynamic tab completions, integrated custom
functions (no external `argc` dependency), improved documentation, and much
more with many more ideas planned for the future.

Coyote is now developed and maintained as an independent project. Full credit
for the original foundation goes to the developers of the wonderful
AIChat project.

This project is not affiliated with or endorsed by the AIChat maintainers.

## AIChat

Coyote originally began as a fork of [AIChat CLI](https://github.com/sigoden/aichat),
created and maintained by the AIChat contributors.

While Coyote has since diverged significantly and is now developed as an
independent project, its early foundation and inspiration came from the
AIChat project.

AIChat is licensed under the MIT License. The MIT license text and its
copyright notice are preserved in the [LICENSE-MIT](./LICENSE-MIT) file.

## Licensing

Coyote as a whole is licensed under the GNU Affero General Public License
v3.0 only (AGPL-3.0-only); see [LICENSE](./LICENSE). Substantial portions
derived from AIChat remain under the MIT License (Copyright (c) sigoden),
preserved in [LICENSE-MIT](./LICENSE-MIT). See [NOTICE](./NOTICE) for the
combined-licensing summary.