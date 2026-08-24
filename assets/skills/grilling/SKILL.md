---
description: Interview the user relentlessly about a plan, decision, or design until shared understanding is reached. Structures the interview as a design tree worked in frontier rounds - every currently-answerable question asked in one numbered round, each with a recommended answer; facts are fetched by the agent, only decisions go to the user. Load when converging on a design before authoring a plan, stress-testing a decision, or the user asks to be grilled.
---
Interview the user relentlessly until you reach a shared understanding. Map the topic as a **design tree**: every decision branches into the decisions that hang off it. Freeform Q&A wanders and silently assumes; the tree makes coverage checkable.

## Frontier rounds

Work the tree in **rounds**. The **frontier** is every decision whose prerequisites are already settled: the questions you can ask *now* without guessing at answers you haven't heard yet. Ask the WHOLE frontier in one round — numbered, each with your recommended answer. Then wait for the user's answers before the next round.

Format a round like so:

```
❓ **Q1 - <question title>**: <question body; may be several paragraphs, may offer lettered choices>

➡️ <your recommended answer, with the one-line reason>

---

❓ **Q2 - <question title>**: <question body>

➡️ <your recommended answer>
```

Rules of the round:

- A question whose answer depends on another question still open in THIS round belongs to a **later** round, not this one. No stacked hypotheticals.
- Recommendations are mandatory. "What do you want?" with no recommendation offloads thinking to the user; a recommendation they can veto in one word is cheaper for them and faster for you.
- Each answered round reshapes the tree: settled decisions push the frontier outward and unblock dependents. Recompute the frontier and ask the next round.
- Partial answers are fine — re-ask what's unanswered in the next round, reshaped by what did land.

## Facts are your job; decisions are the user's

Never ask the user for anything you could look up yourself. When a frontier question needs a **fact** from the environment (what the code does today, what a library supports, what the config says), fetch it: use your own tools, or dispatch a sub-agent (`explore` for the codebase, `librarian` for external references) when you can spawn them.

Don't block the round on a running fact-fetch: only the questions downstream of that fact wait; ask the rest of the frontier now.

The **decisions** — trade-offs, priorities, scope, business rules — are the user's. Put each one to them and wait. Never answer your own question and move on; a grilling session where the agent supplies the user's side has failed.

## Interaction surface

The round format above is chat text — use it whenever a round has more than one question. Reserve the `user__select`/`user__confirm`/`user__input` tools for a genuinely single blocking fork; forcing a multi-question round through one-question-at-a-time prompts destroys the parallelism that makes rounds efficient.

## Completion

The session is done when the frontier is empty: every branch of the design tree visited, nothing left silently assumed. Then:

1. Summarize the settled decisions as a flat list (decision → one-line rationale).
2. Ask the user to confirm the shared understanding.
3. Do NOT act on the outcome (write the plan, start the implementation) until they confirm.

Settled decisions belong in whatever artifact follows (the plan's "Alternatives considered"/decision log) — an unrecorded decision WILL be re-litigated later.

## Anti-patterns

- Asking one question per message when five are independently answerable — that's a slow-motion round.
- Asking questions the codebase answers — grep first, ask never.
- Questions without recommendations.
- Stacked hypotheticals ("if we go with A, then for the storage would you...") — that's a later round.
- Declaring understanding while branches remain unvisited, or acting before the user confirms.
- Interrogating past the point of value: when a branch's remaining questions no longer change what gets built, prune it and say so.
