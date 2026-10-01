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
mesh crates and BSD 3-Clause for the dalek crates and `subtle`, are not in this repository. Add
them, or drop the dependencies, before cutting a release that contains them. The second, settling
the license expression for the combined work, is the license owner's call and is tracked separately.

### Small single-purpose dependencies

`unicode-normalization` exists for one call: the `nfc` rule of `WirePath::parse` in
`src/mesh/wire_path.rs`, which refuses a wire path that is not already NFC rather than
normalising it. Any later dependency of this kind is added on the same terms: one caller,
named here.

### Dependency build-cost records

A point-in-time record for the LXMF-rs and windows-sys dependency addition of 2026-09, not a
standing benchmark. The Windows rows were filled from the first green CI run that included those
dependencies; the table is history now, not a target to hold later runs to. Local figures measured
2026-09-23 on Linux aarch64 with 18 cores, debug profile, populated `target/`. Treat them as an
order of magnitude, not a budget: CI runners have far fewer cores, so expect several times these
numbers there.

The matching functional record, which release of the mesh crates the conformance, interop and fuzz
suites were last run against and with what result, lives in `scripts/mesh-interop/README.md`.

| Measurement | Before mesh dependencies | After |
| --- | --- | --- |
| `cargo test --all`, test execution only, nothing to compile | 12.9s | 13.0s |
| `cargo test --all` including the recompile the change forces | 64s | 77s |
| Clean rebuild of the 24 dependency crates the change adds | n/a | 8s |
| Of which the bundled SQLite C amalgamation | n/a | 2s |
| `windows-latest` CI leg, total job duration | 9m13s (warm caches) | 1h02m41s (cold caches, see breakdown below) |
| `windows-latest` CI leg, `Test` step only | 5m02s (compile included, warm cache) | 3m13s (incremental after a full compile) |

The suite itself does not get slower, and what the table measures is compile time: compile time
now, binary size once the mesh code lands. Nothing under `src/` consumes these crates yet, so the
unreferenced code is elided and today's binary is effectively the size it was before. Binary size
becomes a real cost with the first code that links them in, and is not measured above.

`reticulum-rs-transport` keeps its default `storage` feature, which brings `rusqlite` with a
bundled SQLite in, so the SQLite C amalgamation is now compiled on every platform rather than none.
`bzip2-sys` comes in regardless of that feature and adds a second, much smaller C compile on
Windows and anywhere pkg-config finds no system libbz2. A C toolchain was already required
everywhere for `duckdb`'s bundled build, so this adds compile time but no new prerequisite.

The Windows figure was the one that mattered most, because Windows is where the C compile is
slowest and where nothing previously exercised `rusqlite`. It cannot be measured outside CI from a
non-Windows host, so it was read off the `windows-latest` leg of the first green CI run that
included these dependencies, on head `dc10206` of PR #32, 2026-09-30. The "before" column is the
`main` run of 2026-09-26 (`36275883120`) with warm caches.

Verdict: acceptable. The 1h02m41s wall is not the cost of the mesh dependencies. It breaks down as
rust-cache restore 3m09s, `Install DuckDB Extensions` 17m18s, `Test` 3m13s, Clippy and Format
about 2m, rust-cache save 27m58s. The two large items are one-time cache effects that any
`Cargo.lock` change incurs: the DuckDB step is `cargo test --all duckdb`, a full workspace compile
that missed its cache on this branch and takes 0s on `main` where it hits, and the save is the
first write of a populated `target/` under the new cache key, not re-incurred while the key is
stable. The comparable number is the `Test` step, 3m13s against 5m02s on `main`; the branch's step
was incremental after the DuckDB compile while `main`'s carried the compile itself, so the
comparison is not exact, but it is not slower. The bundled SQLite compile sits inside the 17m18s
full-workspace compile and cannot be isolated on CI; the 2s local figure above is the only
measurement of it. The escape hatch, turning the `storage` feature off, stays documented in the
mesh plan and was not needed.

On the same run `cargo build` and `cargo test --all` were green under `-D warnings` on all three
OSes: `ubuntu-latest` 7m28s, `macos-latest` 10m16s, `windows-latest` as above, with `Merge Gates`
and `Mesh Interop` green alongside. PR #32's Windows leg was the first msvc compile of the
`cfg(windows)` call-site test in `tests/mesh_dependencies.rs` and of `src/utils/windows_acl.rs`;
on this run both compiled and the test passed.

The eight-target release matrix in `.github/workflows/release.yaml`, four of whose legs build
through `cross` including the musl targets, is not exercised before merge. `duckdb`'s
bundled build already forces a C toolchain onto those images, so the two new C compiles should
follow, but the first release is where that is actually tested.

### What was checked for the windows-sys feature list, and what was not

The `windows-sys` entry in `Cargo.toml` names six Win32 features and the calls or types each is
carried for; the call sites are in `src/utils/windows_acl.rs`, the one module that holds the
crate's file-security FFI; the exception is `OpenProcess`, which the test-only `pid_alive` in
`src/testing.rs` uses for process liveness. This is the record of what that list rests on,
checked the same way from Linux aarch64 on 2026-09-23 and again on 2026-09-29 when the sixth
feature was added. It goes when the audit it describes stops mattering.

Checked, and reproducible:

```shell
cargo tree --target x86_64-pc-windows-msvc -e features -i windows-sys@0.61.2
```

exits 0: the `cfg(windows)` graph resolves, and the 42 `windows-sys` features it enables include
all six audited ones. The version in the spec is not optional; four `windows-sys` majors are in
the graph (0.52.0, 0.59.0, 0.60.2, 0.61.2) and a bare `-i windows-sys` exits 101 with
`specification 'windows-sys' is ambiguous`. Most of those 42 features come from other crates —
`mio`, `socket2`, `schannel` and `dirs-sys` each enable their own — so the list read off that tree
is a superset of ours and no substitute for the manifest. The count was 42 both before and after
the sixth feature, `Win32_System_SystemServices`, went in: `os_info` and `rpassword` already
enable it in the msvc graph, so it adds no compiled surface.

A feature name that does not exist upstream cannot survive *any* build, on any platform. Adding
`Win32_Bogus_DoesNotExist` to the list makes resolution fail closed at exit 101 with `package
'coyote-ai' depends on 'windows-sys' with feature 'Win32_Bogus_DoesNotExist' but 'windows-sys'
does not have that feature`: identically with `--target x86_64-pc-windows-msvc` and with no
`--target` at all, because feature names are checked when the graph resolves and not when the
`cfg` is compiled. Every `cargo test` run on every CI leg therefore already proves these six
names exist in windows-sys 0.61. From the other side, windows-sys 0.61.2 declares all six in its
own manifest, and each item the list is carried for is in the module the list claims: `LocalFree`
and `HLOCAL` in `Win32/Foundation`, `ACL` in `Win32/Security`,
`ConvertStringSecurityDescriptorToSecurityDescriptorW`, `GetSecurityInfo` and
`ConvertSidToStringSidW` in `Win32/Security/Authorization`, `CreateFileW` and
`GetVolumeInformationByHandleW` in `Win32/Storage/FileSystem`, `FILE_PERSISTENT_ACLS` and
`ACCESS_ALLOWED_ACE_TYPE` in `Win32/System/SystemServices`,
`OpenProcessToken`, `GetCurrentProcess` and `OpenProcess` in `Win32/System/Threading`. The
`cfg(windows)` test in `tests/mesh_dependencies.rs` names those items, so the windows-latest leg
checks the feature-to-call mapping itself instead of the manifest text that claims it.

**Not** checked from the Linux host, and left to CI: anything that compiles for the msvc target,
or links or runs for any Windows target. `cargo check --target x86_64-pc-windows-msvc` cannot run
from a Linux host here at all. It exits 101 inside dependency build scripts, long before reaching
this crate, because the host C compiler cannot target Windows:
`cc: error: unrecognized command-line option '-m64'`, from `ring`'s `cc-rs` invocation, with
`rusqlite`, `bzip2-sys` and `duckdb` against the same wall. Whether the windows tests pass, and
whether `cargo build` and `cargo test --all` pass on `macos-latest` and `windows-latest`, were
answered by the first green CI run that included these dependencies, recorded under
[Dependency build-cost records](#dependency-build-cost-records) above: yes on all three.

The gnu target does compile from Linux, which is how the `cfg(windows)` code was type-checked and
linted before that run. With the target added (`rustup target add x86_64-pc-windows-gnu`),
`mingw-w64` and `nasm` installed (`ring` assembles with `nasm`), and the big-object flag that
`duckdb`'s bundled C++ needs to get past `file too big`:

```shell
CXXFLAGS_x86_64_pc_windows_gnu='-Wa,-mbig-obj' CFLAGS_x86_64_pc_windows_gnu='-Wa,-mbig-obj' \
  cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
```

exits 0 with warnings denied and compiles `src/utils/windows_acl.rs`, the identity-key call sites
and the windows-only tests under clippy. It does not run them, and it is the gnu ABI rather than
the msvc one the release builds use, so it is a type check and nothing more.

### Platform coverage of the pty tests

The pty tests in `tests/pty_repl.rs` are unix-only. Not because `expectrl` lacks a Windows
backend (it has one, ConPTY), but because the harness drives a raw pty: it sizes the window
through `ptyprocess` via `get_process_mut()`, answers the cursor position handshake itself, and
asserts on the raw VT byte stream reedline paints. ConPTY re-renders that stream through its own
emulator, so those assertions would not transfer. `expectrl` therefore sits under
`[target.'cfg(unix)'.dev-dependencies]` and the file opens with `#![cfg(unix)]`, so the
prompt-integrity assertions are not exercised on windows-latest. That lane still compiles
`examples/pty-reedline-target.rs` and the printer wiring in `src/repl/printer.rs` under
`-D warnings`.

 ## Authorship Policy

All code in this repository is written and reviewed by humans, or AI-generated where explicitly disclosed.
AI-generated code (e.g., Copilot, ChatGPT, Claude, etc.) is permitted only when explicitly disclosed and
approved. For example: disclosing the use of AI-generated code in a PR description.

Submissions must certify that the contributor understands and can maintain the code they submit.

## Questions? Reach out to me!
If you encounter any questions while developing Coyote, please don't hesitate to reach out to me at 
alex.j.tusa@gmail.com. I'm happy to help contributors in any way I can, regardless of if they're new or experienced!
