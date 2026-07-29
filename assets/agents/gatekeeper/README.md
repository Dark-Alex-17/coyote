# Gatekeeper

A **plan self-containedness gate**. Audits a plan against the "sealed container" standard before it
is finalized:

> A context-free LLM implementer must be able to execute the plan using ONLY what is on the page —
> every question it will hit mid-implementation is either **answered inline** or **delegated via a
> verified pointer** to the exact code/docs where the answer lives.

Where [`plan-review`](../../skills/plan-review/SKILL.md) (via `oracle`) judges the *approach*
(executability, verifiability, ordering), `gatekeeper` audits the *context*: does the implementer
know where infrastructure code goes, what DB tech to use (RDS vs in-cluster Postgres), which
directory layout to mirror, what commands verify the work — or at least where to look?

## The three review gates

| Gate | Agent | Question | When |
|------|-------|----------|------|
| Self-containedness | `gatekeeper` | "Can a context-free LLM implement from this file alone?" | Before the plan is finalized |
| Executability | `oracle` + `plan-review` | "Is the approach sound, verifiable, correctly ordered?" | Before the plan is promoted |
| Conformance | [`adversary`](../adversary/README.md) | "Is the built code what the plan asked for?" | After implementation |

## How it audits

Driven by the [`plan-gatekeeping`](../../skills/plan-gatekeeping/SKILL.md) skill:

1. Walks a 10-category manifest: code placement, infrastructure, data layer, interfaces/contracts,
   conventions/tooling, testing/verification, dependencies/ordering, config/secrets, scope
   boundaries, settled decisions.
2. For each category: answered inline, delegated via pointer, or **missing**.
3. **Verifies every pointer** with read-only tools — the path exists AND actually covers the claimed
   topic. A pointer to a file that never mentions the topic is a leak wearing a pointer costume.
4. Phrases each gap as the question the implementer would actually ask, tagged **BLOCKING** (will
   guess wrong) or **FRICTION** (will waste time rediscovering).

## Verdict (blocking)

```
PLAN_GATE: SEALED
Categories audited: N applicable, all answered or pointed.
```

```
PLAN_GATE: LEAKY
Missing questions (N):
1. [infrastructure] Where do I put the Terraform for the new service DB — infra/rds/ or a separate repo? — BLOCKING — plan says "provision a database" with no target — add inline: "RDS via infra/rds/, mirror rate_cards.tf"
Broken pointers (if any):
- "see docs/db.md for conventions" — path missing
```

`LEAKY` blocks finalization. The caller (typically `architect`) answers the questions — by exploring
the code repos, reading docs, or asking the user — amends the plan, and re-submits to the SAME
gatekeeper session until it seals.

## Usage

Spawned by `architect` during design-doc decomposition (Phase B/C), before the `oracle` plan-review:

```sh
agent__spawn --agent gatekeeper --prompt "Audit this plan for self-containedness. Return SEALED/LEAKY.

Plan: <plans_dir>/PLAN-<slug>.md
Target project: <project_dir>"
```

Ad-hoc use against any plan file:

```sh
coyote -a gatekeeper --agent-variable project_dir ~/code/my-service \
  "Audit plans/PLAN-my-feature.md for self-containedness"
```

## Related

- [`plan-gatekeeping`](../../skills/plan-gatekeeping/SKILL.md) — the manifest + methodology it runs on.
- [`architect`](../architect/README.md) — the orchestrator that gates plans through it.
- [`adversary`](../adversary/README.md) — the post-implementation conformance counterpart.
