# Research style notes

These are general principles the `deep-research` agent should keep in
mind regardless of topic. Replace this file with your own notes if you
want to bias retrieval toward your local context.

## What "good research" means here

- **Every factual claim cites a source you actually retrieved.** Never
  fabricate URLs, page titles, authors, or DOIs.
- **Primary sources beat aggregators.** Prefer the original paper, the
  RFC, the standards body, or the manufacturer over a blog summarizing
  them.
- **Corroboration matters where stakes are high.** If a single source
  makes a strong claim, look for a second independent source before
  taking it as established.
- **Disagreement is information, not noise.** If two credible sources
  disagree, report the disagreement and the reasoning on each side.
- **Old does not mean wrong.** A 2014 RFC is still authoritative if no
  newer one has obsoleted it; check before assuming a source is stale.

## Source-tier heuristics

The `vet_sources` node uses these rough tiers to weigh credibility.
The custom tool `classify_source` (see `tools.sh`) implements this
deterministically by hostname / TLD.

- **HIGH:** government domains (`.gov`, `.mil`), academic institutions
  (`.edu`, university subdomains), peer-reviewed journals, standards
  bodies (IETF/RFCs, W3C, ISO, IEEE, NIST), and primary documents from
  the entities being researched (e.g. a vendor's official spec page).
- **PREPRINT:** arXiv, bioRxiv, medRxiv, SSRN. Useful but not yet
  peer-reviewed; treat numeric claims with extra caution.
- **ORGANIZATION:** established nonprofits, standards-adjacent groups,
  industry consortia. Reliable for their stated mission but may have a
  perspective.
- **UNVERIFIED:** general web pages, blogs, news aggregators, social
  media. Useful for leads but should not be the only source for a
  factual claim.

## Common pitfalls to flag in critique

- A claim cited only to a PREPRINT or UNVERIFIED source on a numeric
  or contested point.
- A research-plan question that the findings address only obliquely.
- "Findings" that paraphrase a single source three times rather than
  triangulating.
- Citation collisions where two sources are listed but turn out to
  be the same study reported via different aggregators.
