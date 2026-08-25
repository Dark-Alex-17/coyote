---
description: Post-implementation observability analysis - decide what monitoring, metrics, and alerts the just-implemented code needs, account for what already exists, and either produce concrete alert-as-code changes (routed through the normal implementation pipeline) or a structured recommendations block for the final report / PR description. Advisory by design - it always produces its artifact, never a blocking verdict. Load after implementing changes that add operational surface - new external endpoints, error paths, queues/jobs/crons, or notable state machines. Grants read-only filesystem access for stack detection and coverage inventory.
enabled_tools: fs_read, fs_grep, fs_glob, fs_cat, fs_ls
---
Code was just implemented; you are deciding how anyone will know when it breaks. Logging (see `logging-discipline`) makes failures *inspectable*; this pass makes them *noticed* — metrics, alerts, dashboards. The output is always an artifact (code changes or a recommendations block), never a verdict: observability judgments (thresholds, paging severity) are ultimately human calls, so this lane informs and proposes rather than blocks.

## When this pass applies

The change adds **operational surface**: a new or changed external endpoint, a new error path or failure mode, a new queue consumer/producer, background job, or cron, a new external dependency, a notable state machine, or new metrics. If none of these — pure refactor, UI polish, docs, tests — skip with a one-line note. An observability pass on inert code is budget spent producing nothing.

## Step 1: Detect the observability stack

Establish what this repo HAS before proposing anything:

1. **Metrics emission** — grep for the instrumentation the codebase already uses (a metrics client, OpenTelemetry, statsd-style calls, framework middleware). Note the naming convention of existing metrics.
2. **Alert-as-code** — look for alert/monitor definitions living in the repo: rule files (e.g. Prometheus-style `*.rules.y*ml`), monitor/alert resources in infrastructure-as-code, `alerts/`/`monitoring/` directories, dashboard-as-code. THIS determines your output mode (Step 3).
3. **Existing coverage inventory** — for the paths the change touches, find what already watches them: grep rule files and dashboards for the relevant metric names, service names, and log strings. An alert that already covers the new failure mode means UPDATE or NOTHING, not a duplicate.
4. **Live-lookup hook (optional)** — if the caller configured an agent that can query the live monitoring stack, spawn it to verify the inventory ("what alerts currently cover <service/path>? current thresholds?") instead of trusting repo greps alone. The spawn prompt is that agent's whole context: name the services, metrics, and symptoms to look up, and state that it is read-only reconnaissance. If no such agent is configured, note that the inventory is repo-derived.

## Step 2: Gap analysis

For each new failure mode / operational surface in the change, walk the chain:

1. **Is there a signal?** Does anything (metric, log line, built-in framework metric) even record this failing? No signal → no alert can exist; the first recommendation is the signal itself.
2. **Is there detection on the signal?** An existing alert/monitor that would fire? Check semantics, not just existence — an endpoint-level 5xx alert may already cover your new handler; a queue-depth alert may NOT cover your new consumer's silent skip path.
3. **Is the detection actionable?** Would it fire with enough context to triage (labels, runbook link), at the right urgency?

Classify each gap: **covered** (existing signal + alert suffice), **update** (existing alert needs a label/threshold/scope change), **new** (nothing watches this), or **accepted-blind** (deliberately unwatched — say why, e.g. dev-only tooling).

## Step 3: Produce the artifact (write vs recommend)

The repo's alert-as-code situation decides:

- **Alert-as-code lives in this repo** and the gap warrants coverage → produce the concrete rule/monitor changes (new rules, updated thresholds/labels/scopes) as ordinary code changes, following the existing rule files' conventions exactly. Route them through the caller's NORMAL implementation pipeline — same review gates as any code. An unreviewed alert is a false-page generator.
- **Alerting lives outside the repo** (a UI-managed system, another team's repo), or the decision is judgment-heavy (paging severity, threshold without baseline data) → produce a structured **recommendations block** for the final report / PR description instead. Never attempt to modify external systems.

Mixed outcomes are normal: write the mechanical rule update, recommend the judgment-heavy new pager.

## Output format

Always end with this block (it is the artifact the caller attaches to the report/PR):

```
## Observability

Surface analyzed: <the operational surface this change adds, one line>
Stack: <metrics lib / alert-as-code location or "external-only" / live inventory used: yes|no>

Covered:
- <failure mode> — covered by <existing alert/metric, path or name>

Changes made (via the implementation pipeline):
- <rule file:change> — <what and why>   (or "none")

Recommendations (for humans to action):
- <proposed alert> — signal: <metric/log>, condition: <threshold + rationale or "needs baseline data - start with X and tune">, urgency: <page|ticket>, runbook note: <one line>
- <proposed metric/dashboard addition> — <why anyone would look at it>

Accepted blind spots:
- <what is deliberately unwatched and why>   (or "none")
```

For inapplicable changes the whole block collapses to: `## Observability` / `Not applicable: <one line>`.

## Anti-patterns

- **Alert spam.** Every alert costs attention forever. Page-worthy = a human must act NOW; everything else is a ticket or a dashboard. When in doubt, recommend ticket-urgency and say so.
- **Invented thresholds.** A threshold with no baseline is a guess; either ground it in observed data (existing dashboards, load expectations stated in the change) or mark it explicitly as "start here, tune after N days".
- **Duplicating existing coverage** because you only grepped for one spelling of the metric — inventory first, propose second.
- **Metrics nobody will chart.** Each proposed metric names who would look at it and when. "Might be useful" is not a consumer.
- **Blocking on this pass.** It is advisory: produce the artifact, attach it, move on. The only failure mode is skipping the pass on a change that added operational surface.
- **Touching external alerting systems.** Recommendations only; live systems belong to humans and their change control.
