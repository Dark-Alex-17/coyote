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

### Outstanding license obligations

Coyote must not ship a release while a release-blocking obligation recorded in
[NOTICE](./NOTICE) is unmet. NOTICE leaves two obligations outstanding today, of which one is
release-blocking: the license texts the distributed binary relies on, GPL-2.0-or-later for the
mesh crates and BSD 3-Clause for the dalek crates, are not in this repository. Add them, or drop
the dependencies, before cutting a release that contains them. The second, settling the license
expression for the combined work, is the license owner's call and is tracked separately.

### The in-flight mesh pins

Everything in this subsection goes when the pins it describes go. The mesh work carries two:
`reticulum-rs-transport` and `lxmf-wire`, both at LXMF-rs rev
`3ed5932da4420e2dd1b9d36283b0e72a364e3ebe`. The published 0.11.0 of those crates is fourteen
commits behind that revision and lacks the delivery-stamp calls re-exported at
`lxmf_core::stamp` (`generate_stamp`, `validate_stamp`, `ticket_stamp`), which the mesh work is
built on; it carries only the propagation-stamp half. Note that the pinned revision also calls
itself 0.11.0, so the replacement must be a release strictly newer than 0.11.0 that carries those
calls: pinning `version = "0.11.0"` would satisfy the gate above while silently reverting to the
release this paragraph rejects. Both crates have to move together, too: they share
`reticulum-rs-core`, and a registry crate alongside a git one duplicates it. The pins are
therefore interim, the final state is a crates.io version pin, and that swap is a hard merge gate
for the mesh pull request. It is tracked internally as TASK-063 in the mesh plan.

### Dependency build-cost records

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

The suite itself does not get slower, and what the table measures is compile time: compile time
now, binary size once the mesh code lands. Nothing under `src/` consumes these crates yet, so the
unreferenced code is elided and today's binary is effectively the size it was before. Binary size
becomes a real cost with the first code that links them in, and is not measured above.

`reticulum-rs-transport` keeps its default `storage` feature, which brings `rusqlite` with a
bundled SQLite in, so the SQLite C amalgamation is now compiled on every platform rather than none.
`bzip2-sys` comes in regardless of that feature and adds a second, much smaller C compile on
Windows and anywhere pkg-config finds no system libbz2. A C toolchain was already required
everywhere for `duckdb`'s bundled build, so this adds compile time but no new prerequisite.

The Windows figure is the one that matters most, because Windows is where the C compile is
slowest and where nothing previously exercised `rusqlite`. It cannot be measured outside CI from a
non-Windows host: read it off the `windows-latest` leg of the first CI run that includes these
dependencies, and raise it if that leg regresses materially against its previous duration.

The eight-target release matrix in `.github/workflows/release.yaml`, four of whose legs build
through `cross` including the musl targets, is not exercised before merge either. `duckdb`'s
bundled build already forces a C toolchain onto those images, so the two new C compiles should
follow, but the first release is where that is actually tested.

### What was checked for the windows-sys feature list, and what was not

The `windows-sys` entry in `Cargo.toml` names five Win32 features and one call or type each, and
the call sites land later with the identity-key work. This is the record of what that list rests
on, checked 2026-09-23 from Linux aarch64. It goes when the audit it describes stops mattering.

Checked, and reproducible:

```shell
cargo tree --target x86_64-pc-windows-msvc -e features -i windows-sys@0.61.2
```

exits 0: the `cfg(windows)` graph resolves, and the 40 `windows-sys` features it enables include
all five audited ones. The version in the spec is not optional; four `windows-sys` majors are in
the graph (0.52.0, 0.59.0, 0.60.2, 0.61.2) and a bare `-i windows-sys` exits 101 with
`specification 'windows-sys' is ambiguous`. Most of those 40 features come from other crates —
`mio`, `socket2`, `schannel` and `dirs-sys` each enable their own — so the list read off that tree
is a superset of ours and no substitute for the manifest.

A feature name that does not exist upstream cannot survive *any* build, on any platform. Adding
`Win32_Bogus_DoesNotExist` to the list makes resolution fail closed at exit 101 with `package
'coyote-ai' depends on 'windows-sys' with feature 'Win32_Bogus_DoesNotExist' but 'windows-sys'
does not have that feature`: identically with `--target x86_64-pc-windows-msvc` and with no
`--target` at all, because feature names are checked when the graph resolves and not when the
`cfg` is compiled. Every `cargo test` run on every CI leg therefore already proves these five
names exist in windows-sys 0.61. From the other side, windows-sys 0.61.2 declares all five in its
own manifest, and each item the list is carried for is in the module the list claims: `LocalFree`
and `HLOCAL` in `Win32/Foundation`, `ACL` in `Win32/Security`,
`ConvertStringSecurityDescriptorToSecurityDescriptorW` in `Win32/Security/Authorization`,
`CreateFileW` in `Win32/Storage/FileSystem`, `OpenProcess` in `Win32/System/Threading`. The
`cfg(windows)` test in `tests/mesh_dependencies.rs` names those items, so the windows-latest leg
checks the feature-to-call mapping itself instead of the manifest text that claims it.

**Not** checked, and unverified until the first CI run: anything that compiles or links for a
Windows target. `cargo check --target x86_64-pc-windows-msvc` cannot run from a Linux host here at
all. It exits 101 inside dependency build scripts, long before reaching this crate, because the
host C compiler cannot target Windows: `cc: error: unrecognized command-line option '-m64'`, from
`ring`'s `cc-rs` invocation, with `rusqlite`, `bzip2-sys` and `duckdb` against the same wall.
Whether these five features suffice for the identity-key call sites, and whether `cargo build` and
`cargo test --all` pass on `macos-latest` and `windows-latest`, are answered by the first CI run
that includes these dependencies and by nothing before it.

 ## Authorship Policy

All code in this repository is written and reviewed by humans. AI-generated code (e.g., Copilot, ChatGPT,
Claude, etc.) is not permitted unless explicitly disclosed and approved.

Submissions must certify that the contributor understands and can maintain the code they submit.

## Questions? Reach out to me!
If you encounter any questions while developing Coyote, please don't hesitate to reach out to me at 
alex.j.tusa@gmail.com. I'm happy to help contributors in any way I can, regardless of if they're new or experienced!
