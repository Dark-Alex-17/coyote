---
description: Check a code change against operational history - past incidents, outages, and on-call fixes - so a review catches regressions of hard-won production lessons. Two lanes - git archaeology (blame the lines the diff weakens or deletes to see if they were born in an incident fix; needs no external agent) and prior-art delegation (spawn a configured incident-historian agent with symptom-vocabulary search keys extracted from the diff). Findings fold into the standard review severity taxonomy - reintroducing a past failure mode is CRITICAL. Grants shell access for git history commands.
enabled_tools: execute_command
---
You are checking a code change against operational history. Code review answers "is this code good?"; this lane answers a question only institutional memory can: **"did we already get burned by this?"** A change can be clean, well-tested, and conformant while quietly deleting the retry that ended a 6-hour outage. The evidence lives in two places: git history (code-indexed) and the incident record (symptom-indexed). Work both.

## When this lane runs

This lane is OPTIONAL and runs only when both hold:

1. **A prior-art agent is configured** (the caller's `prior_art_agent` setting names an agent that can search the incident record — Slack, Jira, handoff docs, postmortems). If it is empty, run ONLY the git-archaeology lane (Part A), which needs no external agent.
2. **The diff touches operationally-relevant surface**: services with on-call history, code that emits alerts/metrics/log lines operators watch, error handling, retries, timeouts, rate limits, queue/batch processing, or config controlling any of these. A docs change or a pure-UI tweak does not need an incident sweep — skip and say so in one line.

## Part A: Git archaeology (code-indexed, always available)

The highest-value catch in this entire lane: **a diff that removes or weakens a line that exists because of a past incident.** Look at what the diff DELETES or LOOSENS — guards, retries, timeouts, limits, locks, ordering, special-case branches with no obvious purpose — and ask where each came from:

```
execute_command --command "git log --oneline -3 -L <start>,<end>:<file>"
execute_command --command "git log --oneline -S '<deleted snippet>' -- <file>"
```

Read the originating commit message (`git show --stat <sha>`). Signals that a line was born in an incident fix:

- Ticket/incident references (INC-, JIRA keys, "postmortem", "outage", "hotfix", "pages", "sev")
- Fix-shaped messages ("prevent X under load", "handle Y race", "bound Z to avoid OOM")
- A commit that touches only this guard, dated near a known incident

**A deleted/weakened line whose origin is an incident fix is a 🔴 CRITICAL finding** — the diff reintroduces a known production failure mode. Cite the line, the originating commit, and its message. If the origin is ordinary feature work, no finding — do not manufacture history.

## Part B: Extract symptom-vocabulary search keys from the diff

The incident record is indexed by what OPERATORS saw, not by file paths. Before delegating, translate the diff into that vocabulary:

1. **Error/log strings** added, changed, or deleted — incidents are found by error strings more than by anything else. A DELETED log line is itself a lead: someone may rely on it for triage.
2. **Metric, alert, and dashboard names** the code emits or the change affects.
3. **Config keys** and their old/new values (timeouts, limits, feature flags).
4. **Service/feature/domain terms** an operator would use ("invoice proration", "webhook retries", "usage export") — not function names.
5. **External dependencies touched** (queues, third-party APIs, databases) — their names appear in incident titles.

Collect 3-8 strong keys. Weak generic keys ("error", "billing") flood the search; skip them.

## Part C: Delegate to the prior-art agent (REVIEW MODE)

Spawn the configured agent. Its normal job is live-incident triage, so the prompt MUST re-scope it — the spawn prompt is its entire context:

```
agent__spawn --agent <prior_art_agent> --prompt "REVIEW MODE — prior-art check for a proposed code change (NOT live triage; nothing is on fire).

## CHANGE SUMMARY
<2-4 sentences: what the diff does, which service/feature, what operational surface it touches>

## SEARCH KEYS
<the Part B keys: error strings, metric/alert names, config keys, feature terms>

## TASK
Search the incident record (handoff docs, Slack, Jira, postmortems) for past incidents matching these keys. For each relevant hit report:
- Reference (ticket/thread/doc section) and date
- What happened and what the resolution was
- Relevance: does this change RISK REINTRODUCING that failure mode, or should it ADOPT a safeguard from that resolution?

Only report incidents with a concrete connection to these search keys. 'The billing system has had incidents' is noise. If nothing relevant exists, say so plainly — a clean result is a valid result.

You are read-only. Do not post, comment, or edit anything."
```

## Folding findings into the review report

Prior-art findings use the SAME severity taxonomy as the rest of the review — no separate verdict:

| Finding | Severity |
|---|---|
| Diff reintroduces a past incident's failure mode (archaeology hit on a deleted guard, or historian match showing this exact pattern caused an incident) | 🔴 CRITICAL — cite the incident/commit |
| Past incident's resolution added a safeguard the new code should mirror but doesn't (sibling code got a fix; this new path lacks it) | 🟡 WARNING |
| Related incident exists; change looks safe but reviewer/author should know the history | 🟢 SUGGESTION — informational, with the reference |
| Historian found nothing relevant | One line in the report: "Prior-art check: no relevant incident history found for <keys>." |

Present these under a dedicated **"Operational history"** section in the final report, each finding citing its incident reference or originating commit.

## Anti-patterns

- Running the incident sweep on every trivial change — it is trigger-gated for a reason; Slack/Jira searches are slow and rate-limited.
- Blocking on vague similarity ("this area had incidents once") — a 🔴 requires a concrete reintroduction path tied to a specific incident or originating commit.
- Searching by file paths or function names — the incident record doesn't know them; translate to symptom vocabulary first.
- Skipping Part A because no prior-art agent is configured — archaeology is local git work and always available.
- Treating a clean historian result as wasted effort — "no prior art" is signal, and it belongs in the report as one line, not zero.
- Manufacturing findings from ordinary-feature-work commits to have something to say. Most deleted lines were not incident fixes.
