### Merge gates

- [ ] Nothing is pulled from git (this succeeds:
      `cargo metadata --format-version 1 --locked > /tmp/meta.json && ! grep -q '"source":"git+' /tmp/meta.json`).
      Git pins are development-only and must be replaced by a crates.io version pin before merge;
      see [Dependency policy](https://github.com/Dark-Alex-17/coyote/blob/main/CONTRIBUTING.md#dependency-policy).

### AI assistance (if any):
- List tools here and files touched by them

### Authorship & Understanding

- [ ] I wrote or heavily modified this code myself
- [ ] I understand how it works end-to-end
- [ ] I can maintain this code in the future
- [ ] No undisclosed AI-generated code was used
- [ ] If AI assistance was used, it is documented below

