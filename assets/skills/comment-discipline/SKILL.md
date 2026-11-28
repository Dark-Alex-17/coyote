---
description: Calibrate comment density and style to the repository's existing conventions before writing code, and cut the comments that do not earn their keep from a finished diff. Detects the repo's comment register (self-documenting / api-documented / comment-heavy) from the sibling files you already read for pattern matching, or from a declared policy in workspace instructions, then dictates when a comment is warranted. Default when signal is weak - write NO comment. The final-diff pass is the closing judgement read orchestrators run before the first review of the finished work. Complements ai-slop-remover (which bans comments that restate code in every register).
---
You are about to write or modify code. LLMs systematically over-comment (narrating every block, restating signatures, banner-ing sections), and that default is wrong in most repositories. Before writing, determine the repo's **comment register** and match it, exactly the way you already match imports, naming, and error handling. Across every register, one test governs each comment you write: the rule below.

## The rule: a comment earns its keep

Code should strive to be self-documenting. If a comment only states what the code does, and that is
easily discernible from reading the code, it is redundant and is removed. A comment earns its place
when it does one of these:

- adds context that cannot be determined from the code: intent, an invariant, an external
  constraint, a workaround and the upstream issue behind it;
- documents functionality that is not visible from the current context because it is used
  elsewhere in the codebase: a contract a distant caller relies on, a value another module reads;
- records the decision behind a choice: why this approach and not the obvious one, in a sentence.
  A ledger of what a run checked belongs in the task log, not the code.

Follow the codebase's existing commenting standards when it has them (Steps 0 to 2 below find
them); otherwise fall back to these guidelines.

## Step 0: Check for a declared policy first

Detection is a heuristic; a repo owner's declaration is ground truth. Before sampling files, check the workspace instructions already in your context (`COYOTE.md` / `AGENTS.md` / `CLAUDE.md`) for a stated comment policy (e.g. a "Comments" or "Style" section). If one exists, obey it and skip detection entirely.

## Step 1: Detect the register (during reads you already do)

Pattern-matching discipline already requires you to find and read 2-3 similar existing files before writing. While reading them, observe:

1. **Density** — roughly what fraction of lines are comments? Near-zero, sparse (~1 per function or less), or pervasive (most blocks narrated)?
2. **Types present** — doc comments on public items (`///`, `/** */`, docstrings)? Inline "why" comments? Section banners (`// ===== Handlers =====`)? Commented-out code (a smell, not a convention — never imitate it)?
3. **What the comments say** — do they explain *why* (decisions, workarounds, invariants, links to issues) or narrate *what* (restating the code)? A repo whose comments are all "why" is self-documenting even if density is nonzero.
4. **Config signals** — these force the answer regardless of sampled style: `#![warn(missing_docs)]` or `#![deny(missing_docs)]` in Rust, eslint `jsdoc`/`require-jsdoc` rules, pylint/pydocstyle docstring checkers, a lint config banning TODO without a ticket. Lint-enforced conventions are mandatory.
5. **TODO/FIXME conventions** — bare `TODO:`, or `TODO(name):`, or ticket-linked `TODO(#123):`? Match the observed form if you must leave one.

Sample from the SAME language and module you're editing — a repo can have a chatty Python test suite and a silent Rust core. The nearest siblings win.

## Step 2: Classify into a register

| Register | You observed | Your rule when writing |
|---|---|---|
| **self-documenting** | Near-zero density; the comments that exist explain decisions, temp fixes, or non-obvious behavior | Comment ONLY for: why a non-obvious approach was chosen, documented workarounds/temp fixes (with issue link if the repo does that), safety/concurrency invariants, regex or math explanations. Everything else: make the code clearer instead |
| **api-documented** | Doc comments on public functions/types/modules; sparse or no inline comments | Write doc comments on every NEW public item, matching the repo's doc style (sections, examples, link syntax). Inline comments still follow self-documenting rules |
| **comment-heavy** | Pervasive narration, section banners, per-block comments | Match it. Comment your work the way the siblings do — same placement, same tone, same banner style. Under-commenting here is a convention violation just like over-commenting elsewhere |

Mixed signals (e.g. doc comments everywhere + narrated private code) → combine rows: doc comments mandatory AND inline narration matched.

## Step 3: The tiebreak

**When the signal is weak or files disagree: write NO comment.** Your untrained default is comment-heavy, so the correction must push the other way. A missing comment is a one-line review nit; a hundred useless comments are a cleanup task. If you genuinely cannot tell and the comment feels important, it is usually a sign the code should be restructured until the comment is unnecessary.

## Invariants that do NOT bend with register

1. **Never restate the code.** `// increment the counter` above `counter += 1` is slop in EVERY register — comment-heavy repos narrate intent and sections, they don't caption individual lines the reader can read. (This is `ai-slop-remover`'s rule; it applies unconditionally.)
2. **Always keep the genuinely necessary comment**, even in the sparsest repo: non-obvious algorithm choices (with the reference), regex explanations, safety invariants (`unsafe` justifications, lock ordering), intentional deviations from the obvious approach, and workarounds for upstream bugs (with the link).
3. **Never leave commented-out code**, regardless of what the repo tolerates.
4. **Never delete or rewrite EXISTING comments** that don't match the register you detected — that's out-of-scope churn. Register calibration governs comments YOU write.
5. **Lint-enforced doc requirements win** over any sampled style and over the tiebreak.

## Anti-patterns

- Narrating your implementation (`// First we parse the config, then...`) in a repo whose functions are bare.
- Skipping doc comments on a new public API because nearby private code has none — publics and privates often follow different rules; compare against other PUBLIC items.
- Writing doc comments that restate the signature (`/// Gets the user. Returns the user.`) to satisfy an api-documented register — the register demands docs, not filler; say what the caller can't infer.
- Section banners in a repo that has none.
- Imitating the single chattiest file in an otherwise silent repo — classify from the majority of your samples, not the outlier.
- Treating this skill as license to argue with a declared policy: COYOTE.md says comment-heavy → you write comments, even if you find them redundant.

## Final-diff pass

Writing-time discipline slips: a long run adds comments one at a time, and each looks harmless in
its own hunk. An orchestrator closing a task or a run therefore reads the FINISHED diff once more,
before its first review of the finished work, and cuts what slipped through.

1. Take the whole diff from the work's base commit to the working tree, with new files made visible
   to the diff first (in git, mark them intent-to-add) so their comments are read, and the run's
   own plan files excluded.
2. Read every comment the diff adds or rewrites (code, doc, YAML and script comments) beside the
   code it sits on, and decide one of three outcomes against the rule above:
   - **keep**: it earns its keep, or it matches a register or declared policy the repo has;
   - **cut**: it restates easily read code, repeats a name or a signature, or narrates the plan or
     the implementation steps;
   - **reduce**: one sentence of real context is buried in narration; keep that sentence.
3. This is a judgement pass, not a pattern match: no grep result decides an outcome, because only a
   reading of the code shows whether a comment tells the reader something new.
4. Scope is the comments the diff added. Existing comments stay, per invariant 4 above.
5. The cuts go through the same gates as any change: the implementer makes them, and the build,
   the tests and the review that follows run over the result. A cut never changes code; an edit
   that does is not a comment cut.
6. Report one line: `Comment hygiene: <n> added comments read, <c> cut, <r> reduced`.
