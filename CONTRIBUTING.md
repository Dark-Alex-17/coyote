# Contributing
Contributors are very welcome! **No contribution is too small and all contributions are valued.**

## Rust
You'll need to have the stable Rust toolchain installed in order to develop Coyote.

The Rust toolchain (stable) can be installed via rustup using the following command:

```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

This will install `rustup`, `rustc` and `cargo`. For more information, refer to the [official Rust installation documentation](https://www.rust-lang.org/tools/install).

## Commitizen
[Commitizen](https://github.com/commitizen-tools/commitizen?tab=readme-ov-file) is a nifty tool that helps us write better commit messages. It ensures that our
commits have a consistent style and makes it easier to generate CHANGELOGS. Additionally,
Commitizen is used to run pre-commit checks to enforce style constraints.

To install `commitizen` and the `pre-commit` prerequisite, run the following command:

```shell
python3 -m pip install commitizen pre-commit
```

### Commitizen Quick Guide
To see an example commit to get an idea for the Commitizen style, run:

```shell
cz example
```

To see the allowed types of commits and their descriptions, run:

```shell
cz info
```

If you'd like to create a commit using Commitizen with an interactive prompt to help you get
comfortable with the style, use:

```shell
cz commit
```

## Setup workspace

1. Clone this repo
2. Run `cargo test` to set up hooks
3. Make changes
4. Run the application using `just run` or `just run`
   - Install `just` (`cargo install just`) if you haven't already to use the [justfile](./justfile) in this project.
5. Commit changes. This will trigger pre-commit hooks that will run format, test and lint. If there are errors or 
   warnings from Clippy, please fix them.
6. Push your code to a new branch named after the feature/bug/etc. you're adding. This will trigger pre-push hooks that 
   will run lint and test.
7. Create a PR

### CI/CD Testing with Act
If you also are planning on testing out your changes before pushing them with [Act](https://github.com/nektos/act), you will need to set up `act`,
`docker`, and configure your local system to run different architectures:

1. Install `docker` by following the instructions on the [official Docker installation page](https://docs.docker.com/get-docker/).
2. Install `act` by following the instructions on the [official Act installation page](https://nektosact.com/installation/index.html).
3. Install `binfmt` on your system once so that `act` can run the correct architecture for the CI/CD workflows.
   You can do this by running:
   ```shell
   sudo docker run --rm --privileged tonistiigi/binfmt --install all
   ```

Then, you can run workflows locally without having to commit and see if the GitHub action passes or fails.

**For example**: To test the [release.yml](.github/workflows/release.yaml) workflow locally, you can run:

```shell
act -W .github/workflows/release.yml --input_type bump=minor
```

## Dependency policy

### No git dependencies at merge

A branch may carry a git dependency while it is in development, and only when it is pinned to an
exact `rev` (never a branch or a tag). **No branch may be merged to `main` while the resolved
dependency graph contains a git source.** Before merging, this must return nothing:

```shell
cargo metadata --format-version 1 --locked > /tmp/meta.json && ! grep -q '"source":"git+' /tmp/meta.json
```

Check the exit status, not just the output: written as a bare pipe into `grep`, the gate also
prints nothing when `cargo metadata` itself fails, and an unresolvable git dependency is exactly
what makes it fail. `--locked` keeps the gate from rewriting the lockfile it is auditing.

A literal `grep` over `Cargo.toml` is not good enough. It is sensitive to whitespace and quoting,
and it sees only the root manifest: a git source reached through a dependency's own manifest, or
a `[patch]` declared in `.cargo/config.toml`, never appears there. The gate is on the resolved
graph rather than on `cargo publish` succeeding because the two are not the same test: `cargo
publish` rejects a dependency with a `git` key and no `version` key, but one carrying both
publishes happily and then resolves against the registry, which is the silent-downgrade case.

### Outstanding license obligations

Coyote must not ship a release while a release-blocking obligation recorded in
[NOTICE](./NOTICE) is unmet. NOTICE records two obligations today, of which one is
release-blocking: the license texts the distributed binary relies on, GPL-2.0-or-later for the
mesh crates and BSD 3-Clause for the dalek crates, are not in this repository. Add them, or drop
the dependencies, before cutting a release that contains them. The second, settling the license
expression for the combined work, is the license owner's call and is tracked separately.

### The in-flight mesh pins

Everything in this subsection goes when the pins it describes go. The mesh work
carries two: `reticulum-rs-transport` and `lxmf-wire`, both at
LXMF-rs rev `3ed5932da4420e2dd1b9d36283b0e72a364e3ebe`. The published 0.11.0 of those crates is
fourteen commits behind that revision and lacks the delivery-stamp calls re-exported at
`lxmf_core::stamp` (`generate_stamp`, `validate_stamp`, `ticket_stamp`), which the mesh work is
built on; it carries only the propagation-stamp half. Note that the pinned revision also calls
itself 0.11.0, so the replacement must be a release strictly newer than 0.11.0 that carries
those calls: pinning
`version = "0.11.0"` would satisfy the gate above while silently reverting to the release this
paragraph rejects. Both crates have to move together, too: they share `reticulum-rs-core`, and a
registry crate alongside a git one duplicates it. The pins are therefore interim, the final state
is a crates.io version pin,
and that swap is a hard merge gate for the mesh pull request. It is tracked internally as
TASK-063 in the mesh plan.

### Build and test cost of the mesh pins

A point-in-time record for the LXMF-rs and windows-sys dependency addition of 2026-09, not a
standing benchmark. Fill the Windows row from the first CI run that includes those dependencies,
then leave the table as history; if it is still empty when the pins are retired, delete the row.
Figures measured 2026-09-23 on Linux aarch64 with 18 cores, debug profile, populated
`target/`. Treat them as an order of magnitude, not a budget: CI runners have far fewer cores, so
expect several times these numbers there.

| Measurement | Before mesh dependencies | After |
| --- | --- | --- |
| `cargo test --all`, test execution only, nothing to compile | 12.9s | 13.0s |
| `cargo test --all` including the recompile the change forces | 64s | 77s |
| Clean rebuild of the 24 dependency crates the change adds | n/a | 8s |
| Of which the bundled SQLite C amalgamation | n/a | 2s |
| `windows-latest` CI leg, total job duration | not measured | TBD, fill from the first CI run |

The suite itself does not get slower; the cost is compile time and binary size.

`reticulum-rs-transport` keeps its default `storage` feature, which brings `rusqlite` with a
bundled SQLite in, so the SQLite C amalgamation is now compiled on every platform rather than none.
`bzip2-sys` comes in regardless of that feature and adds a second, much smaller C compile on
Windows and anywhere pkg-config finds no system libbz2. A C toolchain was already required
everywhere for `duckdb`'s bundled build, so this adds compile time and binary size but no new
prerequisite.

The Windows figure is the one that matters most, because Windows is where the C compile is
slowest and where nothing previously exercised `rusqlite`. It cannot be measured outside CI from a
non-Windows host: read it off the `windows-latest` leg of the first CI run that includes these
dependencies, and raise it if that leg regresses materially against its previous duration.

The eight-target release matrix in `.github/workflows/release.yaml`, four of whose legs build
through `cross` including the musl targets, is not exercised before merge either. `duckdb`'s
bundled build already forces a C toolchain onto those images, so the two new C compiles should
follow, but the first release is where that is actually tested.

 ## Authorship Policy

All code in this repository is written and reviewed by humans. AI-generated code (e.g., Copilot, ChatGPT,
Claude, etc.) is not permitted unless explicitly disclosed and approved.

Submissions must certify that the contributor understands and can maintain the code they submit.

## Questions? Reach out to me!
If you encounter any questions while developing Coyote, please don't hesitate to reach out to me at 
alex.j.tusa@gmail.com. I'm happy to help contributors in any way I can, regardless of if they're new or experienced!
