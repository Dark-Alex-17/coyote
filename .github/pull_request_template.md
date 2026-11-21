### Description
<!-- What problem is this PR solving? -->

### Alternatives Considered (Optional)
<!-- What alternatives did you consider? Why did you choose this approach? -->

### AI assistance (if any)
<!-- Required if AI assistance was used to write or modify code in this PR. -->
<!-- List tools here -->

### Authorship & Understanding

- [ ] I wrote or heavily modified this code myself. If I didn't, I gave proper attribution to AI assistance above.
- [ ] I understand how it works end-to-end
- [ ] I can maintain this code in the future
- [ ] No undisclosed AI-generated code was used
- [ ] If AI assistance was used, it is documented above

### Merge gates

- [ ] Nothing is pulled from git (this succeeds, POSIX shell:
      `meta=$(cargo metadata --format-version 1 --locked) && ! printf '%s' "$meta" | grep -q '"source":"git+'`).
      Git pins are development-only and must be replaced by a crates.io version pin before merge;
      see [Dependency policy](https://github.com/Dark-Alex-17/coyote/blob/main/CONTRIBUTING.md#dependency-policy).
      The `Merge Gates` CI job runs the same check, so this box is a courtesy and that job is the
      gate: leaving it unticked hides nothing.
