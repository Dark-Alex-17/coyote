//! Pins the implementation-debris rules in the bundled review and planning
//! skills: comments that earn their keep, ASCII punctuation, no planning
//! artefacts, stranger-readable names, owned debt, docs voice, and a home
//! for decisions. Each test asserts a few distinctive anchor phrases so
//! a rewrite that drops a rule fails here rather than in a downstream review.

use std::fs;
use std::path::Path;

fn skill(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/skills")
        .join(name)
        .join("SKILL.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn assert_anchors(name: &str, anchors: &[&str]) {
    let text = skill(name);
    for anchor in anchors {
        assert!(
            text.contains(anchor),
            "{name}/SKILL.md lost anchor {anchor:?}"
        );
    }
}

#[test]
fn code_review_skill_pins_debris_rules() {
    assert_anchors(
        "code-review",
        &[
            "## 7. Comments and documentation",
            "Check this section on EVERY diff regardless of where the review stopped",
            "`phase N`, `step N` and bare `round N` have too many legitimate uses to lint",
            "`T-a`, `bake-off`, `hand-off`, `\"usage probe:\"`",
            "**Earns its keep**",
            "**Human register, ASCII punctuation**",
            "curly quotes",
            "section signs",
            "section signs, multiplication and comparison symbols,",
            "horizontal bar, non-breaking hyphen, minus and",
            "almost-equal signs.",
            "exempts it from BOTH lint rules",
            "**No planning artefacts**",
            r#""hand-off", "bake-off""#,
            "PUBLISHED specification",
            "**Names a stranger can read**",
            "**Debt has an owner**",
            "`#[allow(...)]`, `#[expect(...)]`",
            "any `cfg_attr(...)` carrying an allow or expect, `todo!()`, `unimplemented!()`, `#[ignore]`",
            "a gate the lint cannot recognise,",
            "the owner comment goes on the gate's code line (the `if` of the env check, the `fn` line under",
            "lint blocks at every rigor; the `[convention]` tag governs only your own findings",
            "otherwise (a standalone review) grep the added lines",
            "a glyph that IS the point",
            "the one sanctioned form",
            "are not patterns to match: a new em dash beside old ones is a hit",
            "**Docs voice**",
            "reads as a guide, not a task report",
            "**Decisions have a home**",
        ],
    );
}

#[test]
fn code_review_skill_pins_design_pass() {
    assert_anchors(
        "code-review",
        &[
            "## 8. Design and structure",
            "a function past ~60 lines",
            "dispatches on a type tag or a string",
            "hand-rolled state machine",
            "kept in sync by a comment",
            "It runs whenever a trigger fires, regardless of where the",
            "review stopped (the same standing section 7 has, with the same `[convention]` rigor fold)",
            "prose, docs or tests-only diffs, and any diff no trigger fires on: skip with a one-line note",
            "`Design: n/a, no new complexity`",
            "make the NEW code simpler to READ? Prefer one",
            "ALREADY ESTABLISHED IN THIS REPO (tier 1 below): `fs_grep` for the precedent first and cite it",
            "`file:line`; with no precedent, fall through rule 1's tiers.",
            "- early-return guards in place of nested conditionals;",
            "- a small record (dataclass, struct, named tuple)",
            "- a shared helper or template for twins kept in sync by comment;",
            "- table-driven dispatch, or a dict/map of callables",
            "- a pipeline of pure transforms",
            "- an explicit state enum for a hand-rolled state machine.",
            "Phrase a tier-1 finding as \"this repo already does X at <file:line>; the new code could use the",
            "same shape\". A tier-2 finding's title starts \"new to this repo: <pattern>\" and its body names the",
            "new files the pattern would live in and the call sites it touches.",
            "**Repo precedent first.**",
            "- Tier 1, precedent: the repo already uses the pattern. Prefer it and cite the precedent",
            "- Tier 2, new to this repo: no precedent, but a tried-and-true pattern (Strategy, Abstract",
            "new code plus minor changes to that new code's own call sites. Acceptable to introduce, even",
            "says \"new to this repo\" so the author and the user see it is an introduction, and it may",
            "only target code that is NEW in this diff, never a restructure of existing code.",
            "- Tier 3, reach beyond: adoption would restructure code unrelated to the change, or you cannot",
            "tell whether the call-site changes count as minor. Never a \"do it\" finding: write",
            "`Design: ASK -- <question>` so the author puts the question to the user. Never guess.",
            "This scope-creep guard is binding",
            "**Always a suggestion.**",
            "is a 🟢 `[convention]` suggestion, never",
            "higher, and so is a tier-2 introduction, unless the complexity hides a real defect",
            "Never demand an abstraction to unify two mildly similar blocks",
            "**A pinned contract is a LEAVE.**",
            "forces it (exact output text, a call budget, a public signature callers depend on)",
            "**DO** (repo precedent exists, or a",
            "new-to-repo pattern contained in the new code; say which), **ASK** (tier 3, whether the reach",
            "beyond the change is known or you are unsure it is minor; the user decides), **DEFER** (the user",
            "answered an ASK with \"later\", or a follow-up task already owns the restructure; it gets its own",
            "follow-up), **DON'T** (no gain). DEFER, DON'T and ASK carry no finding.",
            "`Design: DO -- <file:line precedent>`",
            "`Design: DO -- new to this repo: <pattern>, <files>`",
            "`Design: DEFER -- <who owns it>`",
            "`Design: DON'T -- <why>`",
            "`Design: ASK -- <question>`",
            "- Complexity a pattern this repo already uses, or a tier-2 pattern contained in the new code,",
            "  would remove: cited to its precedent, or titled \"new to this repo\".",
            "- Patterns, with or without repo precedent, whose adoption means restructuring existing code",
            "  (tier 3: ask, never do); abstractions over two similar blocks.",
        ],
    );
}

#[test]
fn plan_authoring_skill_pins_debris_rules() {
    assert_anchors(
        "plan-authoring",
        &[
            "## Names a stranger can read",
            "| Artefact | Lifetime | Removed by |",
            "never in CONTRIBUTING, README or code comments",
            "never under `examples/` as if they were examples",
            "that supersedes them, and that task's acceptance criteria include deleting the scaffolding",
            "A scaffolding task with no superseding task named",
            "Decisions recorded in CONTRIBUTING or README",
        ],
    );
}

#[test]
fn plan_review_skill_pins_debris_rules() {
    assert_anchors(
        "plan-review",
        &[
            "### 7. Names, artefacts and scaffolding",
            "`## Added artefacts` table",
            "PLAN-<slug>.md, Added artefacts --",
            "Do scaffolding tasks (\"add deps ahead of use\"",
            "Are decisions recorded in the plan itself",
            "PLAN_REVIEW: OKAY",
            "PLAN_REVIEW: REJECT",
        ],
    );
}

#[test]
fn task_tracking_skill_pins_debris_rules() {
    assert_anchors(
        "task-tracking",
        &[
            "## Debt with an owner",
            "`#[allow(...)]`, `#[expect(...)]` (outer or inner `#![...]` form), any `cfg_attr(...)` carrying an allow or expect, `todo!()`, `unimplemented!()`, `#[ignore]`, an env-gated or opt-in test",
            "names the owning TASK on the same line as the marker (for an env- or cfg-gated test, a gate the lint cannot recognise, on the gate's code line: the `if` of the env check, the `fn` line under the `#[cfg(...)]`). The orchestrator then:",
            "`debt-assigned` entry in BOTH tasks' `log.md`",
            "the list under `## Debt with an owner`",
            "## Where decisions go",
            "Further labels the orchestrators write: `decision` and `design-decision` (see `## Where decisions go`), `debt-assigned` (see `## Debt with an owner`)",
            "Two labels: a `decision` entry for a decision the run made (what was chosen, what was checked, why), and a `design-decision` entry for a pattern new to this repo that a coder introduced inside the new code",
            "rolls into the PR body's `## Design decisions`",
        ],
    );
}

#[test]
fn design_session_skill_pins_debris_rules() {
    assert_anchors(
        "design-session",
        &[
            "the home for recorded decisions and evidence of what was checked (alongside `## Alternatives considered` and the task logs)",
            "never CONTRIBUTING, README or code comments",
            "## Added artefacts",
            "| Artefact | Lifetime | Removed by |",
        ],
    );
}

#[test]
fn usage_pattern_testing_skill_pins_debris_rules() {
    assert_anchors(
        "usage-pattern-testing",
        &[
            "### Phase 3b: consumer-facing docs",
            "no consumer-facing docs for one (Phase 3b)",
            "| Reads as a guide, not a task report |",
            "section signs, multiplication and comparison symbols, curly quotes",
            "curly quotes, and the rarer look-alikes: horizontal bar, non-breaking hyphen, minus and almost-equal signs.",
            "N/A and never",
            "`<page:section>` in place of `<test file:entry>`",
            "USAGE_PROBE: INCONCLUSIVE",
            "Non-blocking observations: <optional: stale tests to update, recipe gaps, docs-voice items>",
            "Non-blocking observations: <optional: docs-voice items, recipe gaps>",
        ],
    );
}
