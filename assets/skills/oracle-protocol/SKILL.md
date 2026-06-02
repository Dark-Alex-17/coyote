---
description: Discipline for when and how to consult Oracle - blocking by design, never deliver an answer with Oracle pending, never bypass Oracle for design questions.
---
Oracle is your read-only, high-IQ advisor. Using it correctly is the difference between shipping the right thing slowly and shipping the wrong thing fast.

## When you MUST consult Oracle

Spawn `oracle` (do NOT answer yourself) any time the user asks:

- "How should I..." / "What's the best way to..." — design/approach questions
- "Why does X keep..." / "What's wrong with..." — complex debugging (not simple errors)
- "Should I use X or Y?" — technology or pattern choices
- "How should this be structured?" — architecture and organization
- "Review this" / "What do you think of..." — code/design review
- Tradeoff questions — performance vs readability, complexity vs flexibility
- Multi-component questions — anything spanning 3+ files or modules
- Vague/open-ended — "improve this", "make this better", "clean this up"
- After 2+ failed fix attempts on the same problem — complex debugging

Even if you think you know the answer, Oracle provides deeper, more thorough analysis. The only exception is truly trivial questions about a single file you've already read.

## Oracle is BLOCKING by design

The orchestrator (you) has paused work and CANNOT proceed until Oracle returns. This is intentional. The cost of Oracle's latency is paid so YOU get a thorough, considered answer rather than rushing in a wrong direction.

Therefore:

- **Do NOT implement before Oracle returns** if your implementation depends on Oracle's recommendation.
- **Do NOT deliver the final user-facing answer** while Oracle is still running.
- **Do NOT "time out and continue anyway"** for Oracle-dependent tasks.
- While waiting, do only NON-OVERLAPPING prep work (work that doesn't depend on Oracle's verdict).

## How to consult Oracle effectively

Oracle has not seen the codebase or the conversation. Give it enough context to think:

```
## Question
[The decision you need help with, stated as a question]

## Background
[Why this question matters now. What constraint or trigger raised it.]

## Code context
[Paste the actual snippets from prior exploration — file paths alone are not enough]
- From `path/to/file.ext`:
  <relevant 5-20 lines>

## What you've considered
[Options you've already weighed and their tradeoffs as you see them]

## What I'd love Oracle to evaluate
[Specific aspects: correctness, performance, security, future flexibility, etc.]
```

A well-scoped Oracle consult returns a tighter answer faster.

## After Oracle returns

1. Read the recommendation, reasoning, and risks sections carefully.
2. If the recommendation conflicts with your prior plan, update the plan — do not silently ignore Oracle.
3. Pass Oracle's recommendation (and reasoning) to the implementer (e.g., coder) as CONTEXT in your delegation.
4. If you disagree with Oracle's verdict, raise it with the user before implementing the alternative — don't act unilaterally against Oracle's advice.

## When NOT to consult Oracle

- Simple file operations you can do with direct tools
- First attempt at any fix (try yourself first; consult after 2 failures)
- Questions answerable from code you've already read
- Trivial decisions (variable names in small functions, formatting)
- Things you can infer from existing code patterns

Over-consultation wastes Oracle's budget and slows the work. Reserve Oracle for genuinely hard or load-bearing decisions.

## Anti-patterns (BLOCKING)

- Answering an architecture question yourself "just this once"
- Delivering a user-facing answer while Oracle is still running
- Implementing the obvious approach without consulting Oracle on a tradeoff question
- Ignoring Oracle's recommendation because it's inconvenient
- Polling `agent__collect` on a running Oracle (end your response, wait for notification)
