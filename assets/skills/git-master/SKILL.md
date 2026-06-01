---
description: Methodology for atomic commits, rebase surgery, and clean git history. Grants shell access for running git commands.
enabled_tools: execute_command
---
You are operating on a git repository. Apply these conventions strictly. Use the `execute_command` tool to run git commands.

## Atomic commits

Each commit represents one logical change. If the commit message needs the word "and," the change is too large; split it. Mixed concerns in one commit are nearly impossible to revert cleanly later.

## Commit messages

- Subject line: imperative mood, ≤50 characters, no trailing period.
- Blank line.
- Body: explain WHY, not WHAT. The diff shows what changed.
- Reference issues by URL or canonical ID, not by free-form description.

## Rebase, don't merge

- `git rebase -i origin/main` before opening a PR.
- Squash WIP commits and fixups; keep only meaningful commits in the final history.
- Never rebase a branch others may have based work on. If unsure, ask.

## Conflict resolution

- Read both sides carefully before resolving. Don't reflexively take "ours" or "theirs."
- After resolving, run tests before continuing the rebase.
- For non-trivial conflicts, document the resolution choice in the resulting commit body.

## Investigation workflow

Use `execute_command` to run these inspection commands when chasing down history:

- `git log -p <file>` — see how a file evolved over time.
- `git log -S '<string>'` (pickaxe) — find when a string was added or removed.
- `git log --all --grep '<pattern>'` — search commit messages.
- `git blame -L <start>,<end> <file>` — current authorship for a line range.
- `git diff <ref1>..<ref2> -- <path>` — narrow diffs to specific paths.
- `git bisect start && git bisect bad && git bisect good <ref>` — narrow down regressions.

## Safety checklist before destructive operations

Before running anything that rewrites history or deletes refs:

- `git status` — confirm clean working tree.
- `git branch --show-current` — confirm which branch you're on.
- `git log -3 --oneline` — confirm what's about to be moved.

## What to never do

- Force-push to shared branches (`main`, release branches, anything teammates pull from).
- `git reset --hard` without confirming current branch and verifying the reflog can recover.
- `git push --no-verify` to skip hooks — fix the underlying issue instead.
- Commit secrets, even temporarily. Once pushed, treat as compromised; rotate.

## When unsure, read state first

Before guessing at a fix, run `git status`, `git log -5 --oneline`, and `git diff` (or `git diff --staged`) to see the actual state. Don't operate on assumptions.
