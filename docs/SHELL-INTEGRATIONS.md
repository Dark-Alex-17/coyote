# Loki Shell Integrations
Loki supports the following integrations with a handful of shell environments to enhance user experience and streamline workflows.

## Tab Completions
### Dynamic
Dynamic tab completions are supported by Loki to assist users in quickly completing commands, options, and arguments.
You can enable it by using the corresponding command for your shell. To enable dynamic tab completions for every
shell session (i.e. persistently), add the corresponding command to your shell's configuration file as indicated:

```shell
# Bash
# (add to: `~/.bashrc`)
source <(COMPLETE=bash loki) 

# Zsh
# (add to: `~/.zshrc`)
source <(COMPLETE=zsh loki)

# Fish
# (add to: `~/.config/fish/config.fish`)
source <(COMPLETE=fish loki | psub)

# Elvish
# (add to: `~/.elvish/rc.elv`)
eval (E:COMPLETE=elvish loki | slurp)

# PowerShell
# (add to: `$PROFILE`)
$env:COMPLETE = "powershell"
loki | Out-String | Invoke-Expression
```

At the time of writing, `nushell` is not yet fully supported for dynamic tab completions due to limitations
in the [`clap`](https://crates.io/crates/clap) crate. However, `nushell` support is being actively developed, and will
be added in a future release.

Progress on this feature can be tracked in the following issue: [Clap Issue #5840](https://github.com/clap-rs/clap/issues/5840).

### Static
Static tab completions (i.e. pre-generated completion scripts that are not context aware) can also be generated using the
`--completions` flag. You can enable static tab completions by using the corresponding commands for your shell. These commands
will enable them for every shell session (i.e. persistently):

```shell
# Bash
echo 'source <(loki --completions bash)' >> ~/.bashrc

# Zsh
echo 'source <(loki --completions zsh)' >> ~/.zshrc

# Fish
echo 'loki --completions fish | source' >> ~/.config/fish/config.fish

# Elvish
echo 'eval (loki --completions elvish | slurp)' >> ~/.elvish/rc.elv

# Nushell
[[ -d ~/.config/nushell/completions ]] || mkdir -p ~/.config/nushell/completions
loki --completions nushell | save -f ~/.config/nushell/completions/loki.nu
echo 'use ~/.config/nushell/completions/cli.nu *' >> ~/.config/nushell/config.nu

# PowerShell
Add-content $PROFILE "loki --completions powershell | Out-String | Invoke-Expression"
```

## Shell Assistant
Loki has an `-e,--execute` flag that allows users to run natural language commands directly from the CLI. It accepts
natural language input and translates it into executable shell commands.

![Shell Assistant Demo](./images/shell_integrations/assistant.gif)

## Intelligent Command Completions
Loki also provides shell scripts that bind `Alt-e` to `loki -e "<current command line>"`, allowing users to generate
commands from natural text directly without invoking the CLI.

For example:

```shell
$ find all typescript files with more than 100 lines<Alt-e>
# Gets replaced with
$ find . -name '*.ts' -type f -exec awk 'NR>100{exit 1}' {} \; -print
```

To use the CLI helper, add the content of the appropriate integration script for your shell to your shell configuration file:
* [Bash Integration](../scripts/shell-integration/bash-integration.sh) (add to: `~/.bashrc`)
* [Zsh Integration](../scripts/shell-integration/zsh-integration.zsh) (add to: `~/.zshrc`)
* [Elvish Integration](../scripts/shell-integration/elvish-integration.elv) (add to: `~/.elvish/rc.elv`)
* [Fish Integration](../scripts/shell-integration/fish-integration.fish) (add to: `~/.config/fish/config.fish`)
* [Nushell Integration](../scripts/shell-integration/nushell-integration.nu) (add to: `~/.config/nushell/config.nu`)
* [PowerShell Integration](../scripts/shell-integration/powershell-integration.ps1) (add to: `$PROFILE`)

## Code Generation
Users can also directly generate code snippets from natural language prompts using the `-c,--code` flag.

![Code Generation Demo](./images/shell_integrations/code-generation.gif)

**Pro Tip:** Pipe the output of the code generation directly into `tee` to ensure the generated code is properly extracted
from any generated Markdown (i.e. remove any triple backticks).
