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

## LXMF-rs (Reticulum and LXMF)

Coyote depends on the `reticulum-rs-transport` and `lxmf-wire` crates from
[LXMF-rs](https://github.com/FreeTAKTeam/LXMF-rs) by FreeTAKTeam. Those crates
are a Rust implementation of the Reticulum Network Stack and the LXMF
messaging format, both originally designed and implemented in Python by Mark
Qvist ([markqvist/Reticulum](https://github.com/markqvist/Reticulum),
[markqvist/LXMF](https://github.com/markqvist/LXMF)).

LXMF-rs is dual-licensed `EPL-2.0 OR GPL-2.0-or-later`, with the Secondary
Licenses Notice offering GPL-2.0-or-later written into its LICENSE file.
Coyote relies on the GPL-2.0-or-later arm, the only arm compatible with
AGPL-3.0-only. Two obligations follow for Coyote: shipping a copy of the GPL
text, and settling the licence expression for the combined work, neither of
which is done yet. [NOTICE](./NOTICE) records both.

Enabling `reticulum-rs-transport` brings `rusqlite` with a bundled SQLite
amalgamation (MIT; SQLite itself is public domain) and `bzip2`/`bzip2-sys`
with a bundled libbzip2 (MIT here; libbzip2 is BSD-style) in transitively.

## windows-sys

On Windows targets Coyote links against
[`windows-sys`](https://github.com/microsoft/windows-rs), Copyright (c)
Microsoft Corporation, distributed under `MIT OR Apache-2.0` and used here
under the MIT License.

## Licensing

Coyote as a whole is licensed under the GNU Affero General Public License
v3.0 only (AGPL-3.0-only); see [LICENSE](./LICENSE). Substantial portions
derived from AIChat remain under the MIT License (Copyright (c) sigoden),
preserved in [LICENSE-MIT](./LICENSE-MIT). See [NOTICE](./NOTICE) for the
combined-licensing summary.

The LXMF-rs dependencies add GPL-2.0-or-later code to that mix. The licence
expression for the resulting combined work has not been settled and the
`license` field in `Cargo.toml` has not been changed; see [NOTICE](./NOTICE).