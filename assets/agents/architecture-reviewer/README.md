# Architecture Reviewer

An **on-demand architecture improvement scout**. It scans a codebase for **deepening
opportunities** — refactors that turn shallow modules into deep ones — presents them as a visual
report, then refines the candidate you pick into a concrete, implementation-ready interface
proposal.

Two things it is deliberately **not**:

1. **Not a completion gate.** The review stack ([`code-reviewer`](../code-reviewer/README.md),
   [`adversary`](../adversary/README.md), [`security-reviewer`](../security-reviewer/README.md))
   judges *changes* before a task finishes. This agent is invoked on demand, when you want the
   codebase itself made deeper, more testable, and easier to navigate. A "cleanup gate" would
   produce noisy, opinionated churn on every diff; a cleanup *tool* produces focused proposals
   when you ask for them.
2. **Not an implementer.** It proposes; you (or a `coder` you delegate to) implement. Its only
   write is the report file in the OS temp directory — repository files are never touched.

## How it works

Driven by the [`codebase-design`](../../skills/codebase-design/SKILL.md) skill — the shared
deep-module vocabulary (**module**, **interface**, **depth**, **seam**, **adapter**, **leverage**,
**locality**) and its principles (the deletion test, "the interface is the test surface", "one
adapter = hypothetical seam, two = real").

1. **Scope by git history (YAGNI).** Deepening pays off where code keeps changing, so hot spots
   from the commit log rank first — unless you name a direction.
2. **Explore for friction.** Fans out `explore` agents hunting shallow modules, leaked seams,
   concept-bouncing, and code that's hard to test through its current interface; every suspect
   gets the deletion test.
3. **Report candidates.** 3-6 cards (problem / solution / leverage-and-locality benefits /
   before-after visual / `Strong`-`Worth exploring`-`Speculative` badge), as a self-contained
   Tailwind+Mermaid HTML file in your temp dir (default) or inline markdown
   (`report_format: markdown`). Ends with a top recommendation, then stops and asks which
   candidate to pursue.
4. **Refine via design-it-twice.** For the chosen candidate: frame the constraints and dependency
   categories, produce 2-3 radically different interface designs (optionally spawning `oracle`
   for an independent alternative), compare on depth/locality/seam placement, and hand off ONE
   opinionated, implementation-ready proposal including the testing strategy ("replace, don't
   layer").

## Usage

```sh
# Scan the current repo, HTML report
coyote -a architecture-reviewer "Find deepening opportunities"

# Aim it at a pain point, inline report
coyote -a architecture-reviewer --agent-variable report_format markdown \
  "The billing/entitlements code is painful to test - what should be deepened?"
```

Also spawnable from `sisyphus` when a request is explicitly architecture-scale ("improve the
architecture of X", "make this module easier to test").

## Related

- [`codebase-design`](../../skills/codebase-design/SKILL.md) — the vocabulary and principles it runs on.
- [`oracle`](../oracle/README.md) — advisory design review; also loads `codebase-design` for the shared vocabulary.
- [`explore`](../explore/README.md) — the codebase walkers it fans out.

## Credits

Adapted from the `codebase-design` and `improve-codebase-architecture` skills in
[mattpocock/skills](https://github.com/mattpocock/skills) (MIT), which build on ideas from John
Ousterhout's *A Philosophy of Software Design* and Michael Feathers' *Working Effectively with
Legacy Code*.
