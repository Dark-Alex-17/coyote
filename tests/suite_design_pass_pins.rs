//! Pins the design-and-structure review pass in the bundled agent prompts.
//! Each test asserts a few distinctive anchor phrases so a rewrite that
//! drops a rule fails here rather than in a downstream review:
//!
//! - `coder_prompt_pins_structure_rule`: the coder's three-tier structure
//!   rule, the `Design question:` marker, the `design_trail` the fix loop
//!   accumulates for the self-review, the self-review's `design_notes`
//!   reconciliation and its empty-string contract, and the README twin.
//! - `coder_verify_ladder_never_overwrites_the_implementers_summary`: no
//!   script between implement and self_review emits `last_node_output`.
//! - `coder_self_review_fault_carries_the_trail_ask`: self_review falls
//!   back to the fault script, which falls back to end_success, and the
//!   README says an implementer's ASK survives a dead self-review.
//! - `sisyphus_prompt_pins_docs_trigger_and_debris_precedence`: the probe
//!   docs trigger, the mechanical debt grep and the debris retry order.
//! - `orchestrators_carry_the_coder_design_disclosure`: the `Design:` block
//!   carry into Sisyphus, architect and step-runner reports, and the
//!   `Design: ASK` line treated as an open question in every carrier.
//! - `sisyphus_never_self_answers_a_design_scope_escalation` and
//!   `architect_never_self_answers_a_design_scope_escalation`: the Tier 3
//!   question is a hard trigger in both orchestrators, in either policy.
//! - `architect_hygiene_pass_removes_every_surviving_debt_marker`,
//!   `architect_prompt_pins_docs_trigger_and_lint_carve_out`,
//!   `step_runner_pins_mechanical_debt_sweep`, `probe_readme_pins_docs_spawn_arm`:
//!   the hygiene pass, the lint carve-out wording, the step-runner debt
//!   sweep and the probe README's docs arm.

use std::fs;
use std::path::Path;

fn agent_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn assert_anchors(rel: &str, anchors: &[&str]) {
    let text = agent_file(rel);
    for anchor in anchors {
        assert!(text.contains(anchor), "{rel} lost anchor {anchor:?}");
    }
}

#[test]
fn coder_prompt_pins_structure_rule() {
    assert_anchors(
        "coder/graph.yaml",
        &[
            "11. Structure follows the repo:",
            "Tier 1: a pattern this codebase already uses; reach",
            "for it and cite the precedent file:line. Tier 2: no precedent,",
            "inside the new code plus minor changes to that new code's own",
            "call sites; introduce it, even in a repo that has never used it,",
            "and keep it visible: the plan summary names it under a `Design:`",
            "touches; that text is what the approval gate sees), and if the",
            "tier, do it and report it the same way under a `Design:` heading",
            "in your summary. On the success path the orchestrator reads the",
            "self-review's `design_notes`, which reconciles your `Design:`",
            "heading against the diff; your summary alone does not reach it,",
            "has to find. Tier 3: adoption would reach beyond that (restructuring",
            "call-site changes count as minor); STOP and call `user__select`;",
            "the question text starts with `Design question:` and names the",
            "pattern, the files it would touch and a recommended option; then",
            "wait for the answer. If the question errors or goes unanswered,",
            "do not adopt the pattern: finish at tier 1 or delete the",
            "complexity, and record `Design: ASK -- <question>` in your",
            "summary so the self-review carries it and the orchestrator relays",
            "it. Never widen the change on your own. Deleting the complexity",
            "beats structuring it. Repeat the `Design:` heading (or the",
            "`Design: ASK` line) on every pass: a fix-loop re-entry writes a",
            "fresh summary, and the self-review reads the latest one first and",
            "the earlier passes after it.",
            "end `plan_summary` with",
            "a `Design:` clause naming the pattern, the files it lives in and the",
            "description: \"1-3 sentences summarizing what will be done, ending with a `Design:` clause when the approach introduces a pattern new to this repo\"",
            "- Complexity a pattern this repo already uses would remove (name the",
            "repo precedent; set review_clean=false for this only when the",
            "precedent is local to the touched file; otherwise do not report",
            "it, it is a suggestion, not a blocker)",
            "Fill `design_notes` with every pattern the diff introduces that the",
            "touched), or leave it empty. Reconcile it against the implementer's",
            "summary: carry every pattern declared under its `Design:` heading",
            "(confirm it, amend the files or call sites to what the diff shows,",
            "or drop it with a reason), carry the `ASK -- <question>` text (what",
            "follows `Design: `) verbatim, one line, so the rendered line is",
            "exactly `Design: ASK -- <question>`, and add any pattern the diff",
            "shows that the implementer did not declare. A heading or ASK line",
            "from an earlier implement pass counts the same as one in the latest",
            "summary. It is a disclosure, not a flag: it never sets\n      review_clean=false by itself. The code-review skill's own\n      `Design: DO/DEFER/DON'T/n/a` verdict line goes in review_notes or\n      your own note, never in design_notes. When there is nothing to\n      disclose, `design_notes` is an empty string, never null.",
            "- Missing abstractions over two similar blocks, or patterns the repo",
            "      ## Implementer's summary\n      {{last_node_output}}\n\n      ## Every implement pass, oldest first (the fix loop re-enters implement)\n      {{design_trail}}\n\n      Read each file's changed region.",
            "  design_notes: ''",
            "  design_trail: ''",
            "        design_notes:\n          type: string\n          description: \"Patterns the diff introduces that the repo did not already use, one per line: pattern, files, call sites touched, reconciled against the implementer's `Design:` heading. Empty when none: an empty string, never null.\"\n      required: [review_clean, review_notes, design_notes]",
            "      Tests: passed\n      Design: {{design_notes}}",
        ],
    );
    assert_anchors(
        "coder/README.md",
        &[
            "Its `Design:` line carries the self-review's `design_notes`",
            "empty when the change only followed precedent or when",
            "the self-review did not run. It may also carry a `Design: ASK -- <question>`",
            "line: a restructure beyond the task's scope the implementer could not get",
            "answered, for the orchestrator to put to the user. An ASK the implementer",
            "recorded survives even when the self-review did not run: `self_review_fault`",
            "copies the `ASK --` lines from the implement passes into `design_notes`.",
        ],
    );
    let coder = agent_file("coder/graph.yaml");
    let implement = coder
        .split("  implement:\n")
        .nth(1)
        .and_then(|rest| rest.split("  verify_build:\n").next())
        .expect("coder implement node runs from its heading to verify_build");
    assert!(
        implement.contains("      last_node_output: '{{output}}'\n"),
        "the implement node must publish its summary as last_node_output for the self-review to reconcile"
    );
    assert!(
        implement.contains("      design_trail: \"{{design_trail}}\\n{{output}}\"\n"),
        "the implement node must append its summary to design_trail so a `Design:` heading survives a fix-loop re-entry"
    );
    assert_anchors(
        "coder/graph.yaml",
        &[
            "curly quotes, and the rarer look-alikes: horizontal bar,",
            "non-breaking hyphen, minus and almost-equal signs) in comments",
            "a gate the lint cannot recognise, that comment goes on the",
            "gate's code line (the `if` of the env check, the `fn` line under",
            "the orchestrator derives the debt list mechanically from",
            "so a recognised marker without that comment comes back",
        ],
    );
    let tools = implement
        .split("    tools:\n")
        .nth(1)
        .and_then(|rest| rest.split("    max_iterations:").next())
        .expect("coder implement node declares tools");
    assert!(
        tools.contains("      - user__select\n"),
        "the implement node must opt into user__select so a tier-3 question can reach the user: {tools}"
    );
    let rule_8 = coder
        .split("8. Debt has an owner:")
        .nth(1)
        .and_then(|rest| rest.split("9. Decisions have a home:").next())
        .expect("coder rule 8 runs from its heading to rule 9");
    assert!(
        !rule_8.to_lowercase().contains("sisyphus"),
        "rule 8 names the orchestrator, not one of them: {rule_8}"
    );
}

#[test]
fn coder_verify_ladder_never_overwrites_the_implementers_summary() {
    let coder = agent_file("coder/graph.yaml");
    for node in ["verify_build", "verify_tests", "fix_loop_gate"] {
        let body = coder
            .split(&format!("\n  {node}:\n"))
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .unwrap_or_else(|| panic!("coder declares {node}"));
        assert!(
            body.contains(&format!("script: scripts/{node}.sh")),
            "the {node} slice runs to the blank line after its script: {body}"
        );
        assert!(
            !body.contains("last_node_output"),
            "{node} must not overwrite the implementer's summary before self_review reads it: {body}"
        );
        let script = agent_file(&format!("coder/scripts/{node}.sh"));
        assert!(
            !script.contains("last_node_output"),
            "{node}.sh must not emit the implementer's summary key"
        );
    }
}

#[test]
fn coder_self_review_fault_carries_the_trail_ask() {
    let coder = agent_file("coder/graph.yaml");
    let review = coder
        .split("\n  self_review:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n  self_review_fault:\n").next())
        .expect("coder self_review node runs from its heading to self_review_fault");
    assert!(
        review.contains("\n    fallback: self_review_fault\n    next: route_review_result\n"),
        "a dead self_review must land on the fault script, not straight on end_success: {review}"
    );
    let fault = coder
        .split("\n  self_review_fault:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("coder declares self_review_fault");
    assert!(
        fault.contains("script: scripts/self_review_fault.sh"),
        "{fault}"
    );
    assert!(
        fault.contains("\n    fallback: end_failure"),
        "a dead fault script fails the run closed rather than looping or passing: {fault}"
    );
    assert!(
        !fault.contains("next:"),
        "the fault script routes by `_next` alone: {fault}"
    );
    let script = agent_file("coder/scripts/self_review_fault.sh");
    for anchor in [
        "startswith(\"Design: ASK -- \")",
        "ltrimstr(\"Design: \")",
        "\"_next\": \"end_success\"",
    ] {
        assert!(
            script.contains(anchor),
            "self_review_fault.sh lost its contract fragment {anchor:?}"
        );
    }
    assert!(
        !script.contains("\"design_trail\""),
        "self_review_fault.sh reads the trail but must never emit the key"
    );
    assert_anchors(
        "coder/README.md",
        &[
            "self_review -. \"review died\" .-> self_review_fault",
            "self_review_fault{\"self_review_fault<br/>script\"} --> end_success",
        ],
    );
}

#[test]
fn sisyphus_prompt_pins_docs_trigger_and_debris_precedence() {
    assert_anchors(
        "sisyphus/instructions.md",
        &[
            "4. The change adds or rewrites **consumer-facing docs** for such a surface",
            "README install/usage",
            "the probe walks the page",
            "as a first-time consumer",
            "`git ls-files --others --exclude-standard -z | xargs -0 -r git add -N -- && git diff --no-color --no-ext-diff <base> | grep -nE '^\\+.*(#!?\\[(allow|expect)\\(|cfg_attr\\(|todo!|unimplemented!|#\\[ignore)'`",
            "ls-files --others --exclude-standard covers every file",
            "the coder created, listed or not, while .gitignore keeps build artefacts out; grep exiting 1",
            "with no lines is the no-debt result (the command runs under pipefail, so the tool reports a",
            "non-zero exit; that is not an error); `-n` numbers the diff output, so resolve each hit to",
            "its `<file:line>` with the `fs_grep` tool: the hit text after the `+`, regex metacharacters",
            "escaped, is its pattern (never paste diff text into a shell string); keep only matches in",
            "files the diff touched)",
            "plus any env-gated or opt-in test it added. A `cfg_attr(` hit that carries neither `allow`",
            "nor `expect` is not debt; drop it.",
            "for an env- or cfg-gated test, a gate the lint cannot recognise, the comment goes on the gate's code line",
            "(the one carve-out: a task id on the same line as a debt marker)",
            "killed, its module unresolvable, wrote a malformed verdict, or the diff spec itself unusable);",
            "the install first, then escalate.",
            "Order of operations: for `debris`, that single conditional re-run",
            "counts as the \"Retry the review again\" option",
            "Every other lane (and `pipeline`)",
            "goes straight to the options, except when findings co-occur on the same output (the",
            "fix-then-re-run rule below applies first).",
        ],
    );
}

#[test]
fn orchestrators_carry_the_coder_design_disclosure() {
    assert_anchors(
        "sisyphus/instructions.md",
        &[
            "- A `Design:` block in the coder's output naming a pattern new to this repo",
            "carried verbatim into your final report, and into the PR body when you author one, under a",
            "`Design decisions:` heading (the PR body's `## Design decisions` section), so the user sees the",
            "introduction. The block is the `Design:` line of the CODER_COMPLETE block and every continuation",
            "line under it; the `Plan:` line's `Design:` clause is the pre-implementation disclosure and the",
            "final `Design:` block wins when they differ. An empty `Design:` line is dropped. A `Design:`",
            "block line beginning `ASK --` is an open question, not a decision: put it to the user with",
            "`user__select` (the same hard trigger as the live escalation; never self-answered) before the",
            "final report, phrasing the question so it begins `Design question:` and quotes the `ASK --`",
            "`<question> -> <answer>` under `Design decisions:`. An answer that adopts the pattern is WORK,",
            "not a record: the code shipped without it, so resume the SAME coder session to introduce it",
            "a pattern that actually landed is written under `Design decisions:` as a decision, the rest",
            "Carry a STEP_COMPLETE `Design decisions:` block into the final report under the same heading, and treat a `Design: ASK -- <question>` line in its `step_summary` the same way as a coder `Design: ASK` line: put it to the user as a `Design question:`, record `<question> -> <answer>`, and treat an adopting answer as work for the next step. An empty block is dropped.",
        ],
    );
    assert_anchors(
        "architect/instructions.md",
        &[
            "copy each line under the Sisyphus report's",
            "`Design decisions:` heading (a pattern new to this repo the coder introduced inside the new",
            "code, carried from its `Design:` block) as a `design-decision` line into this task's `log.md`",
            "(`task-tracking`'s `## Where decisions go` names the label) so Phase F can roll it into the",
            "PR body's `## Design decisions`. A line under that heading beginning `ASK --` is an open",
            "question, never written as a `design-decision`: put it to the user with `user__select` (the",
            "same hard trigger as the live escalation; never self-answered), phrased to begin",
            "`Design question:` and quoting the line verbatim, and log `<question> -> <answer>` as the",
            "`design-decision` line; an answer that adopts the pattern is work, so resume the task's coder",
            "(or create a follow-up task) before marking it; set `status: complete`.",
            "- a **`## Design decisions`** section: one line per `design-decision` entry collected from the",
        ],
    );
    assert_anchors(
        "architect/README.md",
        &["body: task checklist + Hygiene pass + Design decisions + Follow-up / manual actions"],
    );
    assert_anchors(
        "step-runner/graph.yaml",
        &[
            "A non-empty `Design:` block in the \"Coder result\" below (the `Design:`",
            "line of the CODER_COMPLETE block and every continuation line under it:",
            "into Notes for next step, summarised in `step_summary`, and returned",
            "as `design_decisions` so the user sees the introduction at the",
            "`design_decisions` empty. A `Design:` block line beginning `ASK --`",
            "is an open question, not a decision: it goes into `step_summary`",
            "verbatim as its own `Design: ASK -- <question>` line for the approval",
            "gate and stays out of `design_decisions` until the user has answered it.",
            "  design_decisions: ''",
            "      required: [step_summary, design_decisions]",
            "      Downstream plan updates:\n      {{downstream_updates}}\n\n      Design decisions:\n      {{design_decisions}}\n",
        ],
    );
    assert_anchors(
        "step-runner/README.md",
        &[
            "The coder's `Design:` block (a pattern new to this repo it introduced inside",
            "`Design decisions:` heading, empty when the coder followed precedent; a",
            "`Design: ASK` line is surfaced as a question at the approval gate instead.",
        ],
    );
}

#[test]
fn sisyphus_never_self_answers_a_design_scope_escalation() {
    assert_anchors(
        "sisyphus/instructions.md",
        &[
            "A child's escalation that begins `Design question:` or asks whether to restructure code",
            "outside the task's scope (the coder's Tier 3 `Design` question) is a hard trigger too:",
            "always escalate it.",
            "names the cause, a repo convention), take the recommended option. A design/scope escalation",
            "is excluded: the coder's own recommended option for a restructure beyond its scope never",
            "counts as evidence, and step 1 already sent it to the user.",
            "A child escalation that begins `Design question:` or asks whether to restructure code outside",
            "the task's scope (the coder's Tier 3 `Design` question: a pattern whose adoption would reach",
            "beyond the new code) is never answered from context or by its own recommended option, in either",
            "policy: relay it to the user and pass the answer back.",
        ],
    );
}

#[test]
fn architect_never_self_answers_a_design_scope_escalation() {
    assert_anchors(
        "architect/instructions.md",
        &[
            "A child's escalation that begins `Design question:` or asks whether to restructure code",
            "outside the task's scope (the coder's Tier 3 `Design` question) is a hard trigger too:",
            "always escalate it.",
            "on concrete evidence (a reviewer's proposed fix, a repo convention), take it. A design/scope",
            "escalation is excluded: the coder's own recommended option for a restructure beyond its",
            "scope never counts as evidence, and step 1 already sent it to the user.",
            "A child escalation that begins `Design question:` or asks whether to restructure code outside",
            "the task's scope (the coder's Tier 3 `Design` question: a pattern whose adoption would reach",
            "beyond the new code) is never answered from context or by its own recommended option, in either",
            "policy: relay it to the user and pass the answer back.",
        ],
    );
}

#[test]
fn architect_hygiene_pass_removes_every_surviving_debt_marker() {
    assert_anchors(
        "architect/instructions.md",
        &[
            "the debt markers named in the code-review skill (every owning task is complete by the time",
            "this pass runs, since the hygiene task is blocked by all of them, so a surviving marker,",
            "owner named or not, is REMOVED -- Phase F executes no further tasks, so re-owning is not",
        ],
    );
    assert!(
        !agent_file("architect/instructions.md").contains("still pending is NOT a hit"),
        "the hygiene brief must not describe a pending-owner arm it can never reach"
    );
}

#[test]
fn architect_prompt_pins_docs_trigger_and_lint_carve_out() {
    assert_anchors(
        "architect/instructions.md",
        &[
            "Spawn it too when the task adds or rewrites consumer-facing docs for such a",
            "README install/usage sections, wiki pages, docs/ guides",
            "the probe walks the page as a first-time consumer",
            "The debris lint spares a task id beside the",
            "`cfg_attr(`, `todo!`, `unimplemented!`, `#[ignore`); for a gate the lint cannot recognise",
            "the `fn` line under the `#[cfg(...)]`",
            "which the vocabulary rule never",
            "unresolvable, wrote a malformed verdict, or the diff spec itself unusable); re-run the",
            "gauntlet once only if the cause looks transient, otherwise fix the `Diff:` line or the",
            "install first, then escalate.",
            "Order of operations: for `debris`, that single conditional re-run (or the `Diff:`/install",
            "fix) comes FIRST and counts as the \"Retry the review again\" option, and only if the line",
            "goes straight to the options, except when findings co-occur on the same output (the",
            "fix-then-re-run rule below applies first).",
        ],
    );
    assert!(
        !agent_file("architect/instructions.md").contains("spares exactly that form"),
        "the architect must not over-claim what the debris lint recognises"
    );
    assert_anchors(
        "architect/README.md",
        &[
            "The debris lint spares a task id only beside the markers",
            "it knows (`#[allow(`, `#![allow(`, `#[expect(`, `#![expect(`, `cfg_attr(`, `todo!`, `unimplemented!`,",
            "`#[ignore`); for a gate it cannot recognise (an env check, a `#[cfg(...)]`) the owner comment goes",
            "on the gate's code line (the `if` of the env check, the `fn` line under the `#[cfg(...)]`).",
        ],
    );
}

#[test]
fn step_runner_pins_mechanical_debt_sweep() {
    assert_anchors(
        "step-runner/graph.yaml",
        &[
            "5. Debt sweep. The coder does not report debt",
            "step's diff (the coder leaves its edits uncommitted):",
            "`git -C {{project_dir}} ls-files --others --exclude-standard -z | xargs -0 -r git -C {{project_dir}} add -N -- && git -C {{project_dir}} diff --no-color --no-ext-diff <base, normally HEAD> | ",
            "grep -nE '^\\+.*(#!?\\[(allow|expect)\\(|cfg_attr\\(|todo!|unimplemented!|#\\[ignore)'",
            "(the `add -N` puts the coder's new files into the diff; ls-files",
            "--others --exclude-standard covers every file the coder created,",
            "listed or not, while .gitignore keeps build artefacts out; grep",
            "exiting 1 with no lines is the no-debt result (the command runs",
            "under pipefail, so the tool reports a non-zero exit; that is not",
            "an error); `-n` numbers the diff output, so resolve each hit to its",
            "`<file:line>` with the `fs_grep` tool: the hit text after the `+`,",
            "regex metacharacters escaped, is its pattern (never paste diff text",
            "into a shell string); keep only matches in files the diff touched)",
            "plus any env-gated or opt-in test it added. A `cfg_attr(` hit that",
            "carries neither `allow` nor `expect` is not debt; drop it.",
            "under a `Debt left behind:` heading",
            "A `Debt left behind:` list in \"Edge cases discovered\" below is copied",
        ],
    );
    assert_anchors(
        "step-runner/README.md",
        &[
            "derives the step's debt list mechanically",
            "than trusting the coder's summary: after `git ls-files --others",
            "--exclude-standard -z | xargs -0 -r git add -N --` (so every file the coder",
            "created counts, listed or not, while `.gitignore` keeps build artefacts out)",
            "`#![expect(`, `cfg_attr(`, `todo!`, `unimplemented!` and `#[ignore` (a",
            "`cfg_attr(` hit carrying neither `allow` nor `expect` is dropped), plus any",
            "`Debt left behind:` in the handoff's Notes for next step",
        ],
    );
}

#[test]
fn probe_readme_pins_docs_spawn_arm() {
    assert_anchors(
        "probe/README.md",
        &[
            "when the change touches consumer-facing surface, or adds or",
            "rewrites consumer-facing docs for such a surface and a local-run recipe exists",
        ],
    );
}
