# Domain Reviewer

A **domain-slice review leader**. Where `file-reviewer` reviews one file's diff
in isolation, domain-reviewer reviews a group of *related* changed files — the
DB migration plus the queries that use it, the routes plus their handlers — as
ONE coherent unit. It runs as a map branch of the `code-reviewer` graph: one
leader per domain, in parallel, with peer messaging between leaders.

## Why it exists

The strongest recurring review misses are **cross-file-within-domain** defects:
seed data drifting from canonical enums, a guard fixed in one of N twin paths,
a migration renaming a column the query layer still uses. Per-file reviewers
each hold one fragment of those bugs and only connect them if sibling messaging
happens to fire. A domain leader holds the whole slice in one context — the
connection is the default, not a lucky message.

## How it works

1. Fetches its slice's diff itself (`get_diff --files <its files>`).
2. **Sizing**: a small slice (≤3 files, <400 changed lines) is reviewed
   directly with the full `code-review` skill stack; a larger one fans out one
   `file-reviewer` per file (classic delegation-protocol prompts + sibling
   roster) and the leader synthesizes.
3. **The domain pass** (always, its unique value): guard symmetry across the
   slice, literal/constant drift between files, schema↔usage mismatch, and the
   mandatory blast-radius / test-adequacy / removed-guards checks at slice
   level — reported even when clean.
4. **Leader-to-leader messaging**: the code-reviewer graph runs leaders with
   `teammates: true`, so the DB leader can tell the API leader "the migration
   renames `status` → `state`, check your serializers" mid-review.
5. Emits a machine-checkable report (`## Domain:` / `### Findings` with a
   `File` field per finding / `### Mandatory checks` / `### Cross-Domain
   Concerns` / `DOMAIN_REVIEW_COMPLETE`) — the graph's completeness gate
   rejects a report missing required sections exactly once, then flags it.

## Notes

- Read-only; severities per the standard 🔴/🟡/🟢/💡 mapping; never rewrites a
  file-reviewer's severity; dedups identical findings (aspect skill's copy wins).
- Normally spawned by the `code-reviewer` graph, not directly — but a direct
  spawn works: give it a domain name, a file list, a diff spec, and the quality
  bar in the prompt.
