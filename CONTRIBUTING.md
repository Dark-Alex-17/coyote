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

### Running tests
CI runs the suite with [cargo-nextest](https://nexte.st) as `cargo nextest run --all --profile ci`: the suites that
drive the binary on a pseudo-terminal run in a `pty` test group capped at two at a time, and a failing test from that
group gets one retry (see `.config/nextest.toml`). `cargo test` still works locally.

### Windows cross-check
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

On macOS, with `mingw-w64` from Homebrew, bindgen also needs the mingw sysroot:

```shell
BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="--sysroot=$(brew --prefix mingw-w64)/toolchain-x86_64/x86_64-w64-mingw32 -I$(brew --prefix mingw-w64)/toolchain-x86_64/x86_64-w64-mingw32/include"
```

### Snapshot tests
The TUI's frame tests are [insta](https://insta.rs) snapshots under `src/tui/snapshots/` and `src/tui/tests/snapshots/`. A changed frame fails the
test and writes a `*.snap.new` (or, with `cargo insta` installed, a `*.pending-snap`) beside the old file; git ignores
both. Compare the two, and when the change is intended re-run that test with `INSTA_UPDATE=always` (or install the
reviewer once with `cargo install cargo-insta` and use `cargo insta review`) and commit the updated `.snap`. Never
commit a `.snap.new` or a `.pending-snap`.

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

 ## Authorship Policy

All code in this repository is written and reviewed by humans, or AI-generated where explicitly disclosed.
AI-generated code (e.g., Copilot, ChatGPT, Claude, etc.) is permitted only when explicitly disclosed and
approved. For example: disclosing the use of AI-generated code in a PR description.

Submissions must certify that the contributor understands and can maintain the code they submit.

## Questions? Reach out to me!
If you encounter any questions while developing Coyote, please don't hesitate to reach out to me at 
alex.j.tusa@gmail.com. I'm happy to help contributors in any way I can, regardless of if they're new or experienced!
