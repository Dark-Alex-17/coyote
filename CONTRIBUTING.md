# Contributing
Contributors are very welcome! **No contribution is too small and all contributions are valued.**

## Rust
You'll need to have the stable Rust toolchain installed in order to develop Coyote.

The Rust toolchain (stable) can be installed via rustup using the following command:

```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

This will install `rustup`, `rustc` and `cargo`. For more information, refer to the [official Rust installation documentation](https://www.rust-lang.org/tools/install).

### Cross-checking the Windows target from Linux

The `cfg(windows)` code can be type-checked and linted from a Linux host through the gnu target.
Add the target (`rustup target add x86_64-pc-windows-gnu`), install `mingw-w64` and `nasm`
(`ring` assembles with `nasm`), and pass the big-object flag that `duckdb`'s bundled C++ needs to
get past `file too big`:

```shell
CXXFLAGS_x86_64_pc_windows_gnu='-Wa,-mbig-obj' CFLAGS_x86_64_pc_windows_gnu='-Wa,-mbig-obj' \
  cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
```

This compiles the Windows-only code and tests under clippy but does not run them, and it is the
gnu ABI rather than the msvc one the release builds use, so it is a type check and nothing more.
The msvc target cannot be built from Linux: its dependency build scripts need a C compiler that
targets Windows. The `windows-latest` CI leg is where the msvc build and the Windows tests run.

## Commitizen
[Commitizen](https://github.com/commitizen-tools/commitizen?tab=readme-ov-file) is a nifty tool that helps us write better commit messages. It ensures that our
commits have a consistent style and makes it easier to generate CHANGELOGS. Additionally,
Commitizen is used to run pre-commit checks to enforce style constraints.

Commit subjects are ASCII (no em dashes, arrows or ellipses); the CHANGELOG is generated from them.

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

`tests/mesh_wiki_docs.rs` lints the project wiki against the source when pointed at a checkout of
it: `COYOTE_WIKI_DIR=../coyote.wiki cargo test --test mesh_wiki_docs`. Without the variable it
prints `skipping:` and passes.

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
dependency graph contains a git source.** Before merging, this must succeed (POSIX shell):

```shell
meta=$(cargo metadata --format-version 1 --locked) && ! printf '%s' "$meta" | grep -q '"source":"git+'
```

The gate is judged by exit status, never by output. Written as a bare pipe into `grep` it would
print nothing both when the graph is clean and when `cargo metadata` fails, and an unresolvable
git dependency is exactly what makes it fail. `--locked` keeps the gate from rewriting the
lockfile it is auditing.

You do not have to remember to run it. The `Merge Gates` job in
[ci.yaml](.github/workflows/ci.yaml) runs the same check on every pull request and fails the run
while a git source is in the graph, so the gate holds whether or not anyone ticks the box in the
pull request template. A development branch that still carries its `rev` pin will show that job
red on purpose; the red clears when the pin is replaced, not by editing the job. The checklist
item and this section are the explanation, CI is the enforcement.

A literal `grep` over `Cargo.toml` is not good enough. It is sensitive to whitespace and quoting,
and it sees only the root manifest: a git source reached through a dependency's own manifest, or
a `[patch]` declared in `.cargo/config.toml`, never appears there. The gate is on the resolved
graph rather than on `cargo publish` succeeding because the two are not the same test: `cargo
publish` rejects a dependency with a `git` key and no `version` key, but one carrying both
publishes happily and then resolves against the registry, which is the silent-downgrade case.

### The Reticulum transport fork

The Reticulum transport crate Coyote depends on is our fork,
[`coyote-rns-transport`](https://github.com/Dark-Alex-17/coyote-rns-transport). Patches to it
live in that repository and are published to crates.io, never as `vendor/` copies or a
`[patch.crates-io]` table: `cargo install coyote-ai` installs from the published package, which
carries neither. `lxmf-wire` and `reticulum-rs-core` stay upstream. A new upstream release is a
rebase and republish in the fork, then a version bump here, and the two mesh crates move
together so that one `reticulum-rs-core` resolves.

### License texts

A dependency under a license whose text this repository does not already ship brings that text
with it; the shipped ones are `LICENSE`, `LICENSE-MIT`, `LICENSE-GPL-2.0` and
`LICENSE-BSD-3-Clause`. [NOTICE](./NOTICE) records which dependency each covers and what remains
open.

## Authorship Policy

All code in this repository is written and reviewed by humans, or AI-generated where explicitly disclosed.
AI-generated code (e.g., Copilot, ChatGPT, Claude, etc.) is permitted only when explicitly disclosed and
approved. For example: disclosing the use of AI-generated code in a PR description.

Submissions must certify that the contributor understands and can maintain the code they submit.

## Questions? Reach out to me!
If you encounter any questions while developing Coyote, please don't hesitate to reach out to me at 
alex.j.tusa@gmail.com. I'm happy to help contributors in any way I can, regardless of if they're new or experienced!
