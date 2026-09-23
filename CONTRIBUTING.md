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

A branch may carry a `git = ` dependency while it is in development, and only when it is pinned to
an exact `rev` (never a branch or a tag). **No branch may be merged to `main` while `Cargo.toml`
contains one.** Before merging, this must return nothing:

```shell
cargo metadata --format-version 1 | grep '"source":"git+'
```

A literal `grep` over `Cargo.toml` is not good enough: it is sensitive to whitespace and quoting,
and it misses git sources that arrive through `[patch]` or a `.cargo/config.toml` source
replacement. The reason the rule is absolute is that `cargo publish` rejects a dependency that has
a `git` key and no `version` key, and the release workflow publishes to crates.io.

Remove the rest of this subsection together with the pins it describes. The in-flight mesh work
carries two: `reticulum-rs-transport` and `lxmf-wire`, both at
LXMF-rs rev `3ed5932da4420e2dd1b9d36283b0e72a364e3ebe`. The published 0.11.0 of those crates is
fourteen commits behind that revision and is missing APIs the mesh work is built on, so the
registry release cannot be used yet. The pins are therefore interim: the final state is a
crates.io version pin, swapped in once upstream cuts a release containing that revision. That swap
is tracked as TASK-063 in the mesh plan and is a hard merge gate for the mesh pull request.

### Build and test cost

Reference figures, measured 2026-09-23 on Linux aarch64 with 18 cores, debug profile, populated
`target/`. Treat them as an order of magnitude, not a budget: CI runners have far fewer cores, so
expect several times these numbers there.

| Measurement | Before mesh dependencies | After |
| --- | --- | --- |
| `cargo test --all`, test execution only, nothing to compile | 12.9s | 13.0s |
| `cargo test --all` including the recompile the change forces | 64s | 77s |
| Clean rebuild of the 24 dependency crates the change adds | not measured | 8s |
| Of which the bundled SQLite C amalgamation | not measured | 2s |
| `windows-latest` CI leg, total job duration | not measured | TBD, fill from the first CI run |

The suite itself does not get slower; the cost is compile time and binary size.

`reticulum-rs-transport` keeps its default `storage` feature, which brings `rusqlite` with a
bundled SQLite in, so the SQLite C amalgamation is now compiled on every platform rather than none.
`bzip2-sys` adds a second, much smaller C compile. A C toolchain was already required everywhere
for `duckdb`'s bundled build, so this adds compile time and binary size but no new prerequisite.

The Windows figure is the one that matters most, because Windows is where the C compile is
slowest and where nothing previously exercised `rusqlite`. It cannot be measured outside CI from a
non-Windows host: read it off the `windows-latest` leg of the first CI run that includes these
dependencies, and raise it if that leg regresses materially against its previous duration.

 ## Authorship Policy

All code in this repository is written and reviewed by humans. AI-generated code (e.g., Copilot, ChatGPT,
Claude, etc.) is not permitted unless explicitly disclosed and approved.

Submissions must certify that the contributor understands and can maintain the code they submit.

## Questions? Reach out to me!
If you encounter any questions while developing Coyote, please don't hesitate to reach out to me at 
alex.j.tusa@gmail.com. I'm happy to help contributors in any way I can, regardless of if they're new or experienced!
