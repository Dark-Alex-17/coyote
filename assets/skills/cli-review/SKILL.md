---
description: Review the command-line surface contract of a change - exit codes, stdout/stderr channel discipline, help text, non-interactive operation, signal/cleanup behavior, and config precedence. Load when a diff touches argument-parser definitions, the main/entrypoint of a binary, or subcommand modules. Findings fold into the standard code-review severity taxonomy. Grants read-only filesystem access for tracing entrypoints, parsers, and exit paths.
enabled_tools: fs_read, fs_grep, fs_glob, fs_cat, fs_ls
---
You are reviewing a command-line interface. The generic correctness checklist asks "does this command work when a human runs it?"; you ask **"does this command keep its contract with the scripts, pipes, and CI jobs that run it unattended?"** A CLI's real callers are rarely humans at a terminal — they are shell scripts branching on `$?`, pipelines parsing stdout, and cron jobs with no TTY. Most CLI breakage in the wild is not wrong logic; it is a success that exits 1, a diagnostic that corrupts a pipe, or a prompt that hangs a CI job forever.

## When to load this skill

The diff touches ANY of: argument-parser definitions (flag/option/subcommand declarations), the `main`/entrypoint of a binary, or subcommand modules. If the diff is library internals behind an unchanged command surface — unload; this checklist has nothing for you.

## Marker semantics

Every checklist item below carries a severity emoji AND a `[convention]` or `[correctness]` marker; both ride in the finding title so downstream tooling can act on them mechanically. `[convention]` findings are rigor-foldable (the orchestrator may lower them under a relaxed quality bar) and rejectable — but ONLY with cited evidence: a repo convention at file:line, or a recorded plan decision. `[correctness]` is reserved for contract breaks; those findings are neither foldable nor rejectable.

## Linters and mechanized checks

The review orchestrator runs mechanized checks (shell linters, help-text validators); your CONTEXT may already include their output — do not re-derive it. Spend your prose on what linters cannot reach: exit-code semantics on each error path, which stream a message lands on, whether a prompt has an escape hatch. If the repo plausibly warrants a linter config it lacks (shell scripts but no shell linter config), emit a 🟢 `[convention]` finding naming the gap.

## The checklist

Severities below are the production bar. Each item is a context-sensitive question, not an absolute — trace the actual exit paths and output calls before flagging.

### 1. 🔴 `[correctness]` Error paths exiting 0 / success paths exiting non-zero

Trace every exit path the diff adds or modifies: does each failure propagate a non-zero exit code all the way out of `main`, and does success exit 0? The classic bugs: an error that is printed and then falls through to a normal return; a caught exception that logs and continues; a match arm that swallows a `Result`. Scripts branch on `$?` — an inverted exit code silently corrupts every automation built on this command. This is the exit-code contract; it is never foldable and never rejectable.

### 2. 🟡 `[convention]` Diagnostics on stdout corrupting pipeable output

If the command's stdout is (or plausibly will be) piped or parsed — it prints data, JSON, lists, paths — then progress messages, warnings, and diagnostics on stdout corrupt the stream. Do new prints route diagnostics to stderr and reserve stdout for payload? A purely interactive command with no parseable output can be exempt — say so when you rely on that. Which *level and format* diagnostics use is `logging-discipline`'s question; yours is which stream they land on.

### 3. 🟢 `[convention]` Missing or wrong --help for new flags

Does every flag, option, and subcommand the diff adds appear in help output with an accurate description? Check the parser declarations: a flag with no help string, a stale description contradicting new behavior, or a new subcommand missing from the top-level help listing. Help text is the CLI's only discoverable documentation.

### 4. 🟡 `[convention]` Interactive prompt with no non-interactive escape

Does the diff add a prompt (confirmation, password, selection)? Then there must be a non-interactive path: a flag (`--yes`/`--force`-style), an environment variable, or reading from stdin — and ideally the prompt should detect a missing TTY rather than hang. A prompt with no escape hatch deadlocks CI and cron callers. Check what escape idiom the repo's existing prompts use and whether the new one matches.

### 5. 🟡 `[convention]` No signal/cleanup handling for long-running commands with temp state

If the diff adds a long-running command that creates temp files, lockfiles, partial output, or spawns children: what happens on Ctrl-C or SIGTERM? Look for signal handling, cleanup guards (drop/defer/finally/trap), or an idiom the repo already uses. Orphaned locks and half-written files are the finding. Short-lived commands with no temp state are exempt — this item is scoped to commands that hold state long enough for interruption to be a realistic event.

### 6. 🟢 `[convention]` Config precedence violated or undocumented

If the command reads configuration from more than one source, the conventional precedence is flag > environment variable > config file. Does the diff's resolution order honor that — and honor whatever order the repo has already established? A new setting that reads only the file when its siblings accept a flag override, or a precedence order documented nowhere, is the finding. Cite the repo's existing resolution code when flagging a deviation.

## Ground-truth discipline

- READ the full path from error site to process exit — exit-code bugs live in the propagation, not the error site. `fs_grep` for the exit/return conventions the entrypoint uses.
- Check sibling subcommands for the established idioms (stderr usage, prompt escape flags, cleanup guards) — a new subcommand skipping the house pattern is the strongest form of evidence.
- Do not flag hypothetical piping of a command that is documented interactive-only; note the assumption instead.

## What this skill does NOT check

- Whether argument or path inputs are exploitable (injection, traversal, secrets on the command line) → `security-review`.
- Whether the state a command mutates is changed idempotently and atomically under reruns → `transactional-integrity`.
- Log levels, formats, and message register of diagnostics → `logging-discipline` (this skill only checks which stream they use).
- Metrics and alerting for operationally significant commands → `observability-review`.
