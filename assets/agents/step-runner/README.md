# Step-Runner

A graph-based agent that executes **one step** of a phased implementation
plan, with the step protocol from the `step-implementation` skill enforced
as graph edges rather than prose. Designed to be delegated to by
**[Sisyphus](../sisyphus/README.md)**; delegates implementation to
**[Coder](../coder/README.md)** and independent review to
**[code-reviewer](../code-reviewer/README.md)**.

It expects a plan repo authored per the `plan-authoring` skill:

```
plans/
  steps/NN-<slug>.md    # step plans with frontmatter (step/title/depends_on/status)
  handoffs/NN-<slug>.md # written by this agent, validated by a deterministic gate
  NOTES.md              # rolling durable facts
```

## Workflow

```
resolve_step (script)         locate plan + previous handoff, check depends_on,
        ↓                     mark plan in-progress   [→ gate_blocked if deps unsatisfied]
orient (llm, read-only)       merge handoff directives + staleness-check the plan
        ↓
route_staleness (script)      major deviation → gate_deviation (approval)
        ↓
implement (agent → coder)     coder runs its own build/test/self-review fix-loop
        ↓
route_coder_result (script)   COMPLETE → verify | REJECTED / FAILED → end
        ↓
verify_format_lint (script)   format BEFORE evidence, then lint
verify_build (script)         step-level build/typecheck
verify_tests (script)         FULL test suite
        ↓                     [failures → fix_loop_gate, back-edge to implement]
edge_case_sweep (llm)         missed edge cases; annotate downstream plans
        ↓                     (Edge cases sections ONLY - scope changes become proposals)
route_sweep (script)          5+ files or architectural boundary → independent_review
independent_review (agent)    code-reviewer; 🔴 findings loop back to implement (bounded)
        ↓
write_handoff (llm)           evidence-backed handoff per handoff-protocol + NOTES.md
check_handoff (script)        deterministic schema gate; marks plan status complete
        ↓
gate_user_review (approval)   HARD STOP - approve, or send revision comments
        ↓                     (revisions loop through implement → verify → handoff again)
end_success / end_blocked / end_rejected / end_failure
```

End nodes emit sentinel outcomes for the caller:

- `STEP_COMPLETE` — step implemented, verified, handoff written, user approved.
- `STEP_BLOCKED` — `depends_on` unsatisfied and the user declined to proceed.
- `STEP_REJECTED` — user aborted at the deviation gate, or the coder's plan
  was rejected at its approval gate.
- `STEP_FAILED` — coder failed, the step-level fix budget was exhausted, or
  the handoff failed validation twice.

## Usage

```sh
# From the project root: run the next in-progress/pending step
coyote -a step-runner "Execute the next step"

# A specific step (also parsed from the prompt: "execute step 3")
coyote -a step-runner --agent-variable step 3 "Execute step 3"

# Plan repo somewhere else
coyote -a step-runner --agent-variable plans_dir docs/plans "Execute the next step"
```

**Invoke from the project root.** The coder sub-agent resolves its own
`project_dir` from the invocation directory; overriding `project_dir` here
does not propagate to the spawned coder.

## Tuning

`graph.yaml` `initial_state` exposes:

- `max_fix_attempts` (default `2`) — step-level fix budget (the coder has
  its own internal budget of 3).
- `max_review_attempts` (default `1`) — bounded 🔴-finding fix loops after
  independent review.

Environment overrides honored by the script nodes:

- `FORMAT_CMD` / `LINT_CMD` — formatting and linting (otherwise a per-type
  heuristic formats, and linting defers to the build/check command).
- `BUILD_CMD` / `TEST_CMD` — skip project-type detection (same as coder).
- `STEP_AUTOAPPROVE=1` — bypass the deviation gate (non-interactive runs).
- `STEP_SKIP_REVIEW=1` — never spawn the independent reviewer.

The final user approval gate is never bypassed by an environment variable -
it is the point of the workflow.
