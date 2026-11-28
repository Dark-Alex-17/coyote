//! Structural checks on `.github/workflows/ci.yaml` and `release.yaml`, read as YAML
//! rather than as text: which jobs gate a merge, the order and conditions of the steps
//! that set up and tear down privileged state, the libtest flags CI passes, where the
//! relay smoke installs to, the commands the scripts job and the image smokes run, and
//! the image smoke that gates the Docker Hub push. Comments and step names are free to
//! change: steps are found by what they run and set, and each command check reads only
//! the one step that must carry it. The one display name checked is the `Mesh Interop`
//! job's, which the interop record cites.

mod support;

use std::collections::BTreeSet;

use serde_yaml::{Mapping, Value};

/// Jobs that run on every PR but sit outside branch protection.
const INFORMATIONAL_JOBS: [&str; 2] = ["mesh-interop", "image-smoke"];

const INTEROP_SETUP: &str = "scripts/mesh-interop/setup.sh";
const NETNS_SETUP: &str = "scripts/mesh-netns/setup.sh";
const NETNS_TEARDOWN: &str = "scripts/mesh-netns/teardown.sh";

const IMAGE_SMOKE: &str = "scripts/image-smoke.sh";
const PN_CONTEXT: &str = "deployment/propagation-node";

/// Variables pipx and uv read for where a tool install lands.
const INSTALL_DIRS: [&str; 4] = [
    "PIPX_HOME",
    "PIPX_BIN_DIR",
    "UV_TOOL_DIR",
    "UV_TOOL_BIN_DIR",
];

/// The parsed workflow, or `None` in a tree without `.github`: Cargo.toml excludes it
/// from the published crate, whose tests must not panic over a file it never had.
fn workflow(file: &str) -> Option<Value> {
    if !support::repo::root().join(".github").is_dir() {
        eprintln!("skipping: .github is not part of this source tree");
        return None;
    }
    let text = support::repo::read(&format!(".github/workflows/{file}"));
    Some(serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{file} parses as YAML: {e}")))
}

/// The skip in `workflow` is for the packaged crate, which ships without `.github`. In a
/// git checkout both workflows must be present, or every check here would pass vacuously.
#[test]
fn a_checkout_has_the_workflows_these_checks_read() {
    let root = support::repo::root();
    if !root.join(".git").exists() {
        eprintln!("skipping: not a git checkout");
        return;
    }
    for file in ["ci.yaml", "release.yaml"] {
        let path = root.join(".github").join("workflows").join(file);
        assert!(
            path.is_file(),
            "{} is missing from a git checkout; the workflow checks would skip instead of run",
            path.display()
        );
    }
}

fn jobs<'a>(workflow: &'a Value, file: &str) -> &'a Mapping {
    workflow["jobs"]
        .as_mapping()
        .unwrap_or_else(|| panic!("{file} has a `jobs` map"))
}

fn job<'a>(workflow: &'a Value, file: &str, name: &str) -> &'a Value {
    let job = &workflow["jobs"][name];
    assert!(!job.is_null(), "{file} has no `{name}` job");
    job
}

fn steps(job: &Value) -> &[Value] {
    job["steps"].as_sequence().map_or(&[], Vec::as_slice)
}

fn run(step: &Value) -> &str {
    step["run"].as_str().unwrap_or("")
}

fn step_name(step: &Value) -> &str {
    step["name"].as_str().unwrap_or("?")
}

/// A string field of a step that a check depends on; a missing one fails here rather
/// than letting the check pass against an empty string.
fn field<'a>(step: &'a Value, key: &str) -> &'a str {
    step[key]
        .as_str()
        .unwrap_or_else(|| panic!("the step {:?} has no `{key}`", step_name(step)))
}

fn assert_runs(step: &Value, needle: &str, why: &str) {
    assert!(
        field(step, "run").contains(needle),
        "the step {:?} must run {needle:?}: {why}",
        step_name(step)
    );
}

fn is_powershell(step: &Value) -> bool {
    matches!(step["shell"].as_str(), Some("pwsh" | "powershell"))
}

fn only_position(steps: &[Value], what: &str, matches: impl Fn(&Value) -> bool) -> usize {
    let found: Vec<usize> = (0..steps.len()).filter(|&i| matches(&steps[i])).collect();
    match found[..] {
        [one] => one,
        _ => panic!("expected exactly one step {what}, found {}", found.len()),
    }
}

fn only_step<'a>(steps: &'a [Value], what: &str, matches: impl Fn(&Value) -> bool) -> &'a Value {
    &steps[only_position(steps, what, matches)]
}

/// The Unix relay smoke: the step that points PIPX_HOME somewhere and drives mesh-relay.sh.
fn unix_smoke(steps: &[Value]) -> usize {
    only_position(
        steps,
        "setting PIPX_HOME and running mesh-relay.sh",
        |step| !step["env"]["PIPX_HOME"].is_null() && run(step).contains("mesh-relay.sh"),
    )
}

/// The Windows relay smoke: the PowerShell step that points PIPX_HOME somewhere and
/// drives mesh-relay.ps1.
fn windows_smoke(steps: &[Value]) -> usize {
    only_position(
        steps,
        "setting PIPX_HOME and running mesh-relay.ps1 under PowerShell",
        |step| {
            !step["env"]["PIPX_HOME"].is_null()
                && is_powershell(step)
                && run(step).contains("mesh-relay.ps1")
        },
    )
}

fn needs(job: &Value) -> Vec<&str> {
    match &job["needs"] {
        Value::String(one) => vec![one.as_str()],
        Value::Sequence(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn position(steps: &[Value], what: &str, matches: impl Fn(&Value) -> bool) -> usize {
    steps
        .iter()
        .position(matches)
        .unwrap_or_else(|| panic!("no step {what}"))
}

/// A `docker build` command line from a step's `run`.
struct DockerBuild<'a> {
    tag: &'a str,
    context: &'a str,
    load: bool,
    build_args: Vec<&'a str>,
}

fn docker_builds(steps: &[Value]) -> Vec<DockerBuild<'_>> {
    steps
        .iter()
        .flat_map(|step| run(step).lines())
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            if !words.starts_with(&["docker", "build"]) {
                return None;
            }
            let after = |flag: &str| -> Vec<&str> {
                words
                    .windows(2)
                    .filter(|pair| pair[0] == flag)
                    .map(|pair| pair[1])
                    .collect()
            };
            let tag = *after("-t").first()?;
            Some(DockerBuild {
                tag,
                context: words.last()?,
                load: words.contains(&"--load"),
                build_args: after("--build-arg"),
            })
        })
        .collect()
}

/// The image and the `--pn` image an `image-smoke.sh` command line runs.
fn smoked_images(step: &Value) -> (&str, &str) {
    let line = run(step)
        .lines()
        .find(|line| line.contains(IMAGE_SMOKE))
        .unwrap_or_else(|| panic!("the step {:?} does not run {IMAGE_SMOKE}", step_name(step)));
    let words: Vec<&str> = line.split_whitespace().collect();
    let after = |flag: &str| {
        words
            .windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1])
            .unwrap_or_else(|| panic!("nothing follows {flag} in: {line}"))
    };
    (after(IMAGE_SMOKE), after("--pn"))
}

/// Being outside `needs` is all that makes a job informational. It must not also be
/// `continue-on-error`, on the job or on any step, or a red suite would vanish into a
/// green run and a green README badge.
#[test]
fn informational_jobs_are_not_continue_on_error() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    for name in INFORMATIONAL_JOBS {
        let job = job(&ci, "ci.yaml", name);
        assert!(
            job["continue-on-error"].is_null(),
            "the `{name}` job must not be continue-on-error: a red run has to show as a red job"
        );
        for step in steps(job) {
            assert!(
                step["continue-on-error"].is_null(),
                "the `{name}` step {:?} must not be continue-on-error: a red step has to fail the job",
                step_name(step)
            );
        }
    }
}

/// An informational job is a prerequisite of nothing, the `All` gate included, so a red
/// run never blocks a merge.
#[test]
fn informational_jobs_are_in_no_jobs_needs() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    assert!(
        !ci["jobs"]["all"].is_null(),
        "ci.yaml has no `all` job, the gate the informational jobs must stay out of"
    );
    for (name, other) in jobs(&ci, "ci.yaml") {
        let name = name.as_str().unwrap_or("?");
        for informational in INFORMATIONAL_JOBS {
            assert!(
                !needs(other).contains(&informational),
                "the `{name}` job lists `{informational}` in its needs, which puts an informational job in front of a gate"
            );
        }
    }
}

/// An informational job waits for nothing: a `needs` would serialise it behind a gate
/// and skip it whenever that gate fails. It runs on the Linux runner (the interop
/// namespaces and the image build both need it) and carries its own timeout, so a hung
/// run does not hold a runner for the six-hour default.
#[test]
fn informational_jobs_wait_for_nothing_and_run_under_a_timeout() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    for name in INFORMATIONAL_JOBS {
        let job = job(&ci, "ci.yaml", name);
        assert!(
            needs(job).is_empty(),
            "the `{name}` job is informational and runs beside the other jobs, not after them"
        );
        assert_eq!(
            job["runs-on"].as_str(),
            Some("ubuntu-latest"),
            "the `{name}` job runs on the Linux runner"
        );
        assert!(
            job["timeout-minutes"].as_u64().is_some_and(|m| m > 0),
            "the `{name}` job must set a positive timeout-minutes: {:?}",
            job["timeout-minutes"]
        );
    }
}

/// The image smoke is the only ci.yaml job that uses GITHUB_TOKEN, and only to read the
/// latest release, so widening its scope must show up here.
#[test]
fn the_image_smoke_token_only_reads_contents() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let permissions = job(&ci, "ci.yaml", "image-smoke")["permissions"]
        .as_mapping()
        .expect("the image-smoke job declares a `permissions` map");
    let expected: Mapping = serde_yaml::from_str("contents: read").expect("literal parses");
    assert_eq!(
        permissions, &expected,
        "the image-smoke token must be exactly `contents: read`"
    );
}

/// `scripts/mesh-interop/README.md` defers the Linux rows of its verification record to
/// the CI job by this name.
#[test]
fn the_interop_job_keeps_the_name_the_interop_record_defers_to() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    assert_eq!(
        job(&ci, "ci.yaml", "mesh-interop")["name"].as_str(),
        Some("Mesh Interop"),
        "the interop record names the `Mesh Interop` CI job; renaming it leaves the record pointing at nothing"
    );
}

/// The network namespaces are created as root and outlive the job's processes, so every
/// step that removes them runs as root and runs whatever the steps before it did.
#[test]
fn netns_teardown_runs_under_sudo_whatever_the_outcome() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let mut found = 0;
    for (name, job) in jobs(&ci, "ci.yaml") {
        let name = name.as_str().unwrap_or("?");
        for step in steps(job)
            .iter()
            .filter(|step| run(step).contains(NETNS_TEARDOWN))
        {
            found += 1;
            assert_eq!(
                step["if"].as_str().map(str::trim),
                Some("always()"),
                "the `{name}` step {:?} must remove the namespaces even when an earlier step failed",
                step_name(step)
            );
            assert!(
                run(step).trim_start().starts_with("sudo "),
                "the `{name}` step {:?} removes the namespaces, which needs root: {:?}",
                step_name(step),
                run(step)
            );
        }
    }
    assert!(found > 0, "no ci.yaml step runs {NETNS_TEARDOWN}");
}

/// A job that creates the namespaces also removes them; the teardown checks above only
/// see jobs that already have a teardown step.
#[test]
fn every_job_that_creates_namespaces_removes_them() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let mut found = 0;
    for (name, job) in jobs(&ci, "ci.yaml") {
        let name = name.as_str().unwrap_or("?");
        let steps = steps(job);
        if !steps.iter().any(|step| run(step).contains(NETNS_SETUP)) {
            continue;
        }
        found += 1;
        assert!(
            steps.iter().any(|step| run(step).contains(NETNS_TEARDOWN)),
            "the `{name}` job runs {NETNS_SETUP} but never {NETNS_TEARDOWN}"
        );
    }
    assert!(found > 0, "no ci.yaml job runs {NETNS_SETUP}");
}

/// The interop job on Linux: the clock starts, the reference is prepared, the namespaces
/// are created as root, the conformance suite runs with the interop tests switched on and
/// un-ignored, the namespaces are removed, and the wall-clock lands in the step summary
/// whatever the suite did.
#[test]
fn the_interop_job_runs_the_suite_between_setup_and_teardown() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let job = job(&ci, "ci.yaml", "mesh-interop");
    assert_eq!(
        job["runs-on"].as_str(),
        Some("ubuntu-latest"),
        "the Python reference harness and the network namespaces are Linux-only"
    );

    let steps = steps(job);
    let setup = position(steps, &format!("runs {INTEROP_SETUP}"), |step| {
        run(step).contains(INTEROP_SETUP)
    });
    let namespaces = position(steps, &format!("runs {NETNS_SETUP}"), |step| {
        run(step).contains(NETNS_SETUP)
    });
    let test = position(steps, "runs the conformance suite", |step| {
        run(step).contains("cargo test") && run(step).contains("mesh::conformance")
    });
    let teardown = position(steps, "removes the namespaces", |step| {
        run(step).contains(NETNS_TEARDOWN)
    });
    assert!(
        run(&steps[namespaces]).trim_start().starts_with("sudo "),
        "creating the namespaces needs root: {:?}",
        run(&steps[namespaces])
    );
    assert!(
        setup < namespaces && namespaces < test && test < teardown,
        "the reference is prepared, then the namespaces created, then the suite run, then the namespaces removed"
    );

    let command = run(&steps[test]);
    let words: Vec<&str> = command.split_whitespace().collect();
    for word in [
        "COYOTE_MESH_INTEROP=1",
        "mesh::conformance",
        "--include-ignored",
    ] {
        assert!(
            words.contains(&word),
            "the suite step must pass {word}, or the interop tests stay off or ignored: {command}"
        );
    }
    assert!(
        !words.contains(&"--ignored"),
        "`--ignored` would skip every test the other jobs run, and libtest rejects it beside --include-ignored: {command}"
    );

    let summary = position(steps, "writes the step summary", |step| {
        run(step).contains("GITHUB_STEP_SUMMARY")
    });
    assert!(summary > test, "the wall-clock is recorded after the suite");
    assert_eq!(
        steps[summary]["if"].as_str().map(str::trim),
        Some("always()"),
        "the wall-clock step runs even when the suite fails"
    );
    assert!(
        run(&steps[summary]).contains("date +%s"),
        "the summary step reads the clock"
    );
    assert!(
        steps[..setup]
            .iter()
            .any(|step| run(step).contains("date +%s") && run(step).contains("GITHUB_ENV")),
        "the clock starts before the reference is prepared, so the recorded wall-clock covers it too"
    );
}

/// `--include-ignored` is a libtest flag: before `--` cargo reads it as its own and fails.
/// libtest also rejects it together with `--ignored`.
#[test]
fn include_ignored_follows_the_double_dash() {
    let mut found = 0;
    for file in ["ci.yaml", "release.yaml"] {
        let Some(workflow) = workflow(file) else {
            return;
        };
        for (name, job) in jobs(&workflow, file) {
            let name = name.as_str().unwrap_or("?");
            for line in steps(job).iter().flat_map(|step| run(step).lines()) {
                let words: Vec<&str> = line.split_whitespace().collect();
                let Some(flag) = words.iter().position(|w| *w == "--include-ignored") else {
                    continue;
                };
                found += 1;
                assert!(
                    words[..flag].contains(&"--"),
                    "{file} `{name}`: --include-ignored must follow `--`: {line}"
                );
                assert!(
                    !words.contains(&"--ignored"),
                    "{file} `{name}`: libtest rejects --ignored together with --include-ignored: {line}"
                );
            }
        }
    }
    assert!(found > 0, "no workflow step passes --include-ignored");
}

/// The GHA cache is repository-scoped and the image is several gigabytes: a layer cache
/// of it would evict the rust-cache entries the `All` matrix depends on. ci.yaml builds
/// with a plain `docker build`, so its `run` is checked as well as any `with`.
#[test]
fn image_builds_export_no_layer_cache() {
    for (file, name) in [
        ("ci.yaml", "image-smoke"),
        ("release.yaml", "publish-sandbox-image"),
    ] {
        let Some(workflow) = workflow(file) else {
            return;
        };
        for step in steps(job(&workflow, file, name)) {
            assert!(
                step["with"]["cache-to"].is_null() && !run(step).contains("cache-to"),
                "{file} `{name}` step {:?} exports a layer cache",
                step_name(step)
            );
        }
    }
}

/// The CI image smoke runs the image and the propagation node the job just built, not
/// whatever an earlier run left under another tag.
#[test]
fn the_ci_image_smoke_runs_the_images_it_built() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "image-smoke"));
    let smoke = position(steps, &format!("runs {IMAGE_SMOKE}"), |step| {
        run(step).contains(IMAGE_SMOKE)
    });
    let (image, pn) = smoked_images(&steps[smoke]);
    let builds = docker_builds(&steps[..smoke]);
    assert!(
        builds.iter().any(|b| b.tag == image && b.context == "."),
        "the smoke runs {image}, which no earlier `docker build ... -t {image} .` produces"
    );
    assert!(
        builds
            .iter()
            .any(|b| b.tag == pn && b.context == PN_CONTEXT),
        "the smoke's --pn runs {pn}, which no earlier `docker build -t {pn} {PN_CONTEXT}` produces"
    );
}

/// The Dockerfile downloads the coyote release named by COYOTE_VERSION, and the latest
/// release is the only one a PR build can download. So the CI smoke resolves that
/// release with the job's token before the build, and the build passes it in.
#[test]
fn the_ci_image_smoke_builds_the_latest_release() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "image-smoke"));
    let resolve = only_position(steps, "running `gh release view`", |step| {
        run(step).contains("gh release view")
    });
    let step = &steps[resolve];
    assert_runs(
        step,
        "gh release view --json tagName -q .tagName | sed 's/^v//'",
        "the Dockerfile prefixes the `v` itself, so the build-arg is the bare version",
    );
    assert!(
        field(step, "run")
            .lines()
            .any(|line| line.contains("coyote_version=") && line.contains("$GITHUB_ENV")),
        "the step {:?} must export coyote_version through GITHUB_ENV for the build step",
        step_name(step)
    );
    assert_eq!(
        step["env"]["GH_TOKEN"].as_str(),
        Some("${{ github.token }}"),
        "`gh` reads the release with the job's read-only token"
    );

    let build = position(steps, "running `docker build ... .`", |step| {
        docker_builds(std::slice::from_ref(step))
            .iter()
            .any(|b| b.context == ".")
    });
    assert!(
        resolve < build,
        "the release is resolved before the image build that needs it"
    );
    assert!(
        docker_builds(std::slice::from_ref(&steps[build]))
            .iter()
            .any(|b| b.context == "."
                && b.build_args.contains(&"COYOTE_VERSION=\"$coyote_version\"")),
        "the image build {:?} must pass --build-arg COYOTE_VERSION=\"$coyote_version\"",
        step_name(&steps[build])
    );
}

/// The release smokes a host-arch build of the image before the multi-platform push, so a
/// broken rnsd layer or entrypoint never reaches Docker Hub. The smoked build is loaded,
/// never pushed, takes the release version as the push does, is the image the smoke
/// runs, and a failed smoke fails the job. The push step then really pushes, to the
/// `latest` tag and the release's own tag.
#[test]
fn the_release_smokes_the_image_before_the_push() {
    let Some(release) = workflow("release.yaml") else {
        return;
    };
    let job = job(&release, "release.yaml", "publish-sandbox-image");
    assert!(
        job["continue-on-error"].is_null(),
        "a failed smoke must fail the release, not be noted and pushed anyway"
    );
    let steps = steps(job);
    for step in steps {
        assert!(
            step["continue-on-error"].is_null(),
            "the step {:?} must not be continue-on-error: the push would run past a failed smoke",
            step_name(step)
        );
    }

    let smoke = position(steps, &format!("runs {IMAGE_SMOKE}"), |step| {
        run(step).contains(IMAGE_SMOKE)
    });
    let push = position(steps, "pushes for linux/arm64", |step| {
        step["with"]["platforms"]
            .as_str()
            .is_some_and(|platforms| platforms.contains("linux/arm64"))
    });
    assert!(
        smoke < push,
        "the smoke must precede the multi-platform push: a smoke after the push guards nothing"
    );

    let builds: Vec<&Value> = steps[..smoke]
        .iter()
        .filter(|step| {
            step["uses"]
                .as_str()
                .is_some_and(|uses| uses.starts_with("docker/build-push-action"))
        })
        .collect();
    assert!(
        !builds.is_empty(),
        "the image is built with docker/build-push-action before the smoke"
    );
    for build in builds.iter().copied().chain([&steps[push]]) {
        assert!(
            build["with"]["build-args"]
                .as_str()
                .is_some_and(|args| args.contains("COYOTE_VERSION=${{ env.version }}")),
            "the build {:?} must pass build-args COYOTE_VERSION=${{{{ env.version }}}}, or it \
             bakes in a coyote other than the one this release publishes: {:?}",
            step_name(build),
            build["with"]["build-args"]
        );
    }
    for build in &builds {
        let with = &build["with"];
        assert_eq!(
            with["push"],
            Value::Bool(false),
            "the smoke build {:?} must not push: only the smoked image may reach Docker Hub",
            step_name(build)
        );
        assert_eq!(
            with["load"],
            Value::Bool(true),
            "the smoke build {:?} must be loaded into the daemon, or the smoke runs a stale image",
            step_name(build)
        );
        assert_eq!(
            with["platforms"].as_str(),
            Some("linux/amd64"),
            "the smoke build {:?} is host-arch only, so the smoke never executes under QEMU",
            step_name(build)
        );
        assert_eq!(
            with["build-args"],
            steps[push]["with"]["build-args"],
            "the smoke build {:?} and the push take the same build-args, so the smoked image is the pushed image",
            step_name(build)
        );
    }

    let (image, pn) = smoked_images(&steps[smoke]);
    assert!(
        builds
            .iter()
            .any(|build| build["with"]["tags"].as_str() == Some(image)),
        "the smoke runs {image}, which is not the tag of the smoke build"
    );
    assert!(
        docker_builds(&steps[..smoke])
            .iter()
            .any(|b| b.tag == pn && b.context == PN_CONTEXT && b.load),
        "the smoke's --pn runs {pn}, which no earlier `docker build --load -t {pn} {PN_CONTEXT}` produces; \
         without --load a buildx container builder leaves it outside the daemon"
    );

    let with = &steps[push]["with"];
    assert!(
        match &with["push"] {
            Value::Bool(push) => *push,
            Value::String(expr) => expr.contains("env.ACT != 'true'"),
            _ => false,
        },
        "the push step must push, outside local `act` runs only: {:?}",
        with["push"]
    );
    let tags: BTreeSet<&str> = with["tags"]
        .as_str()
        .unwrap_or("")
        .split([',', '\n'])
        .map(str::trim)
        .collect();
    for tag in [
        "darkalex17/coyote:latest",
        "darkalex17/coyote:v${{ env.version }}",
    ] {
        assert!(
            tags.contains(tag),
            "the push step must publish {tag}: {tags:?}"
        );
    }
}

/// The dependency gate's error tells a contributor where the dependency policy lives, and
/// that anchor has to resolve to a heading in CONTRIBUTING.md.
#[test]
fn the_merge_gate_error_links_a_contributing_heading_that_exists() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let gate = steps(job(&ci, "ci.yaml", "merge-gates"))
        .iter()
        .find(|step| run(step).contains("cargo metadata") && run(step).contains("git+"))
        .expect("a merge-gates step checks `cargo metadata` for git+ sources");
    assert!(
        run(gate)
            .lines()
            .any(|line| line.contains("::error::")
                && line.contains("CONTRIBUTING.md#dependency-policy")),
        "the dependency gate {:?} must print an ::error:: pointing at CONTRIBUTING.md#dependency-policy",
        step_name(gate)
    );
    let contributing = support::repo::read("CONTRIBUTING.md");
    assert!(
        contributing
            .lines()
            .any(|line| line.trim_end() == "## Dependency policy"),
        "CONTRIBUTING.md must keep the `## Dependency policy` heading the merge gate's error links to"
    );
}

/// The relay smoke runs on every runner family the relay supports.
#[test]
fn the_scripts_job_runs_on_every_runner_family() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let os = job(&ci, "ci.yaml", "scripts")["strategy"]["matrix"]["os"]
        .as_sequence()
        .expect("the scripts job has a `strategy.matrix.os` list");
    for runner in ["ubuntu-latest", "macos-latest", "windows-latest"] {
        assert!(
            os.iter().any(|entry| entry.as_str() == Some(runner)),
            "the scripts matrix lacks {runner}: {os:?}"
        );
    }
}

/// The ubuntu image exports PIPX_HOME=/opt/pipx image-wide, so a relay install that is
/// not pointed elsewhere leaks into the runner image. Each install directory is set on
/// exactly the Unix and the Windows smoke step, under the per-run mesh home, and so is
/// the home directory each smoke installs into. The smokes are the steps that set
/// PIPX_HOME and drive mesh-relay.sh, or mesh-relay.ps1 under PowerShell.
#[test]
fn the_relay_smokes_install_under_a_per_run_home() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let job = job(&ci, "ci.yaml", "scripts");
    let steps = steps(job);
    let unix = unix_smoke(steps);
    let windows = windows_smoke(steps);
    let smokes = BTreeSet::from([unix, windows]);
    let under_mesh_home = |value: &Value| {
        value
            .as_str()
            .is_some_and(|v| v.contains("${{ runner.temp }}") && v.contains("mesh-home"))
    };
    for var in INSTALL_DIRS {
        assert!(
            job["env"][var].is_null(),
            "{var} belongs on the smoke steps, not the whole scripts job"
        );
        let setting: BTreeSet<usize> = (0..steps.len())
            .filter(|&i| !steps[i]["env"][var].is_null())
            .collect();
        assert_eq!(
            setting,
            smokes,
            "{var} must be set on exactly the two relay smoke steps, {:?} and {:?}",
            step_name(&steps[unix]),
            step_name(&steps[windows])
        );
        for i in setting {
            let value = &steps[i]["env"][var];
            assert!(
                under_mesh_home(value),
                "{var} on {:?} must sit under ${{{{ runner.temp }}}}/mesh-home: {value:?}",
                step_name(&steps[i])
            );
        }
    }
    for (i, var) in [(unix, "HOME"), (windows, "USERPROFILE")] {
        let value = &steps[i]["env"][var];
        assert!(
            under_mesh_home(value),
            "{var} on {:?} must sit under ${{{{ runner.temp }}}}/mesh-home: {value:?}",
            step_name(&steps[i])
        );
    }
}

/// The relay runs under both PowerShell 7 and Windows PowerShell 5.1, so the scripts job
/// has a step under each.
#[test]
fn the_scripts_job_runs_both_powershells() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    for shell in ["pwsh", "powershell"] {
        assert!(
            steps
                .iter()
                .any(|step| step["shell"].as_str() == Some(shell)),
            "no scripts step runs under `shell: {shell}`"
        );
    }
}

/// shellcheck reads every shell script the repo ships, at the strictest severity
/// (`-S style`): the top-level scripts, the ones a directory deeper, and the propagation
/// node's entrypoint. It runs on the Linux leg, the one runner image that carries it.
#[test]
fn shellcheck_covers_every_shell_script_on_linux() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let step = only_step(steps, "running shellcheck", |step| {
        run(step).contains("shellcheck")
    });
    assert_eq!(
        field(step, "if").trim(),
        "runner.os == 'Linux'",
        "the step {:?} runs on the Linux leg",
        step_name(step)
    );
    let checks: Vec<Vec<&str>> = field(step, "run")
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|words| words.first() == Some(&"shellcheck") && !words.contains(&"--version"))
        .collect();
    assert!(
        !checks.is_empty(),
        "the step {:?} never runs shellcheck on a file",
        step_name(step)
    );
    for words in &checks {
        assert!(
            words.windows(2).any(|pair| pair == ["-S", "style"]),
            "shellcheck must run at `-S style` so style findings fail the lane: {}",
            words.join(" ")
        );
    }
    for target in [
        "scripts/*.sh",
        "scripts/*/*.sh",
        "deployment/propagation-node/entrypoint.sh",
    ] {
        assert!(
            checks.iter().any(|words| words.contains(&target)),
            "shellcheck must cover {target}"
        );
    }
}

/// PSScriptAnalyzer reads every tracked .ps1, wherever it sits, rather than a fixed
/// directory; an empty list is an error rather than a vacuous pass; warnings count; and
/// any finding fails the step instead of scrolling past in the log.
#[test]
fn psscriptanalyzer_analyses_every_tracked_ps1_and_fails_on_findings() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let step = only_step(steps, "running Invoke-ScriptAnalyzer", |step| {
        run(step).contains("Invoke-ScriptAnalyzer")
    });
    assert_eq!(
        field(step, "shell"),
        "pwsh",
        "the analyser is a pwsh module"
    );
    for (needle, why) in [
        (
            "$files = @(git ls-files '*.ps1')",
            "the files are enumerated from the index",
        ),
        (
            "if (-not $files) { throw",
            "an empty file list must fail rather than analyse nothing",
        ),
        (
            "$files | ForEach-Object { Invoke-ScriptAnalyzer -Path $_ -Severity Warning,Error }",
            "every enumerated .ps1 is analysed at Warning,Error",
        ),
    ] {
        assert_runs(step, needle, why);
    }
    assert!(
        !field(step, "run").contains("-Path scripts"),
        "the analyser must not be narrowed back to a fixed directory"
    );
    assert!(
        field(step, "run")
            .lines()
            .any(|line| line.trim() == "if ($r) { exit 1 }"),
        "a PSScriptAnalyzer finding must fail the step {:?}",
        step_name(step)
    );
}

/// PSScriptAnalyzer is pinned to an exact version and that version is both installed and
/// imported, so a newer module already on the runner image cannot win and change the rules.
/// One step owns the pin, so two can never drift apart, and moving it is a reviewed bump.
#[test]
fn psscriptanalyzer_is_pinned_and_imported_at_the_pin() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let step = only_step(steps, "setting PSSA_VERSION", |step| {
        !step["env"]["PSSA_VERSION"].is_null()
    });
    let pinned = &step["env"]["PSSA_VERSION"];
    let version = pinned.as_str().unwrap_or_else(|| {
        panic!(
            "PSSA_VERSION must be an exact version string such as 1.25.0 (a bare 1.26 parses \
             as a YAML float, not the version written): {pinned:?}"
        )
    });
    assert!(
        version.split('.').count() >= 2
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
        "PSSA_VERSION must be an exact version, not a range or wildcard: {version:?}"
    );
    assert_eq!(
        version, "1.25.0",
        "PSScriptAnalyzer stays at 1.25.0 so a new rule is a reviewed bump, not a red PR; \
         moving the pin means updating this test in the same change"
    );
    for cmdlet in ["Install-Module", "Import-Module"] {
        assert!(
            run(step).lines().any(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words.starts_with(&[cmdlet, "PSScriptAnalyzer"])
                    && words
                        .windows(2)
                        .any(|pair| pair == ["-RequiredVersion", "$env:PSSA_VERSION"])
            }),
            "the {:?} step must run `{cmdlet} PSScriptAnalyzer -RequiredVersion $env:PSSA_VERSION`",
            step_name(step)
        );
    }
}

/// macOS ships bash 3.2 as /bin/bash, the bash the README one-liners land in on a Mac, so
/// the macOS leg runs both the relay and the installer's --help probe under it rather
/// than a newer bash from the image's PATH.
#[test]
fn macos_runs_the_relay_and_the_installer_under_bin_bash() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    assert_runs(
        &steps[unix_smoke(steps)],
        "if [ \"$RUNNER_OS\" = macOS ]; then relay=(/bin/bash scripts/mesh-relay.sh); fi",
        "the macOS relay smoke runs under /bin/bash 3.2",
    );
    let probe = only_step(steps, "probing install_coyote.sh --help", |step| {
        run(step).contains("scripts/install_coyote.sh --help")
    });
    assert_eq!(
        field(probe, "if").trim(),
        "runner.os != 'Windows'",
        "the installer probe {:?} runs on the macOS leg",
        step_name(probe)
    );
    for needle in [
        "if [ \"$RUNNER_OS\" = macOS ]; then bash=/bin/bash; fi",
        "\"$bash\" scripts/install_coyote.sh --help",
    ] {
        assert_runs(
            probe,
            needle,
            "the macOS installer probe runs under /bin/bash 3.2",
        );
    }
}

/// The 5.1 step drives mesh-relay.ps1 itself under Windows PowerShell, not just a module
/// load, and keeps a native command's stderr from becoming a terminating error so the
/// exit code it checks is the relay's.
#[test]
fn windows_powershell_5_1_drives_the_relay() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let step = only_step(steps, "under `shell: powershell`", |step| {
        step["shell"].as_str() == Some("powershell")
    });
    assert_runs(
        step,
        "powershell -NoProfile -ExecutionPolicy Bypass -File scripts/mesh-relay.ps1 -NoService -DryRun -AllowRoot",
        "the 5.1 step runs the relay script under Windows PowerShell",
    );
    assert!(
        field(step, "run")
            .lines()
            .any(|line| line.trim() == "$ErrorActionPreference = 'Continue'"),
        "the 5.1 step {:?} must set $ErrorActionPreference = 'Continue'; $LASTEXITCODE is the oracle",
        step_name(step)
    );
}

/// On Windows the bash relay refuses Git Bash with exit 2; the README sends Windows users
/// to the PowerShell relay instead.
#[test]
fn git_bash_is_refused_with_exit_2() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let step = only_step(steps, "running mesh-relay.sh under `shell: bash`", |step| {
        step["shell"].as_str() == Some("bash") && run(step).contains("scripts/mesh-relay.sh")
    });
    assert_eq!(
        field(step, "if").trim(),
        "runner.os == 'Windows'",
        "the Git Bash refusal {:?} runs on the Windows leg",
        step_name(step)
    );
    assert_runs(
        step,
        "bash scripts/mesh-relay.sh --no-service --dry-run || rc=$?; [ \"$rc\" -eq 2 ]",
        "the bash relay must refuse Git Bash with exit 2",
    );
}

/// The pwsh dry run plans a Scheduled Task and must not register one.
#[test]
fn the_pwsh_dry_run_registers_no_scheduled_task() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    assert_runs(
        &steps[windows_smoke(steps)],
        "Get-ScheduledTask -TaskName 'Coyote rnsd' -ErrorAction SilentlyContinue)) { throw",
        "the dry run must be proven to register no Scheduled Task",
    );
}

/// A non-default rns version passed to the relay lands in the dry-run plan, on both shells.
#[test]
fn a_non_default_version_lands_in_both_dry_run_plans() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let why = "a non-default version must land in the dry-run plan";
    let unix = &steps[unix_smoke(steps)];
    assert_runs(unix, "--version 1.5.1 --no-service --dry-run", why);
    assert_runs(unix, "grep -q 'rns==1.5.1'", why);
    let windows = &steps[windows_smoke(steps)];
    assert_runs(windows, "'-Version', '1.5.1', '-NoService', '-DryRun'", why);
    assert_runs(windows, "'rns==1\\.5\\.1'", why);
}

/// The venv fallback is proven on a PATH without uv or pipx and with neither config
/// variable set, under a throwaway Roaming profile that the smoke step has not already
/// installed into (there a working rnsd.cmd would win as the `present` rung).
#[test]
fn the_venv_fallback_runs_without_uv_pipx_or_config_variables() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let step = only_step(
        steps,
        "setting APPDATA and running mesh-relay.ps1 under PowerShell",
        |step| {
            !step["env"]["APPDATA"].is_null()
                && is_powershell(step)
                && run(step).contains("mesh-relay.ps1")
        },
    );
    let appdata = step["env"]["APPDATA"]
        .as_str()
        .expect("APPDATA is a path string");
    assert!(
        appdata == "${{ runner.temp }}\\mesh-venv-home\\AppData\\Roaming",
        "APPDATA must be a throwaway Roaming profile under ${{{{ runner.temp }}}}\\mesh-venv-home: {appdata:?}"
    );
    for (needle, why) in [
        (
            "Remove-Item Env:COYOTE_CONFIG_DIR, Env:XDG_CONFIG_HOME",
            "the venv plan must be made with both config variables unset so APPDATA is the one left",
        ),
        (
            "Join-Path $_ 'uv.exe')) -and -not (Test-Path (Join-Path $_ 'pipx.exe'))",
            "every PATH entry holding uv or pipx must be stripped so the venv rung is reached",
        ),
        (
            "Install: py -3 -m venv",
            "the plan must pick `py -3`, the rung before `python`",
        ),
        (
            "\\\\AppData\\\\Roaming\\\\coyote\\\\mesh\\\\rns-venv",
            "the plan must place the venv under %APPDATA%\\coyote\\mesh\\rns-venv",
        ),
    ] {
        assert_runs(step, needle, why);
    }
}

/// The Unix and the Windows smoke check the same results of the relay: the dry run
/// changes nothing, the installed rnsd runs, a re-run leaves its own config alone and
/// prints no stanzas, a foreign config stays byte-identical while the stanzas to merge
/// are printed, and root is refused. Each check is looked for in the smoke step that
/// must make it.
#[test]
fn both_relay_smokes_check_the_same_results() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let steps = steps(job(&ci, "ci.yaml", "scripts"));
    let unix = &steps[unix_smoke(steps)];
    let windows = &steps[windows_smoke(steps)];
    for (what, unix_needle, windows_needle) in [
        (
            "dry run",
            "--no-service --dry-run",
            "'-NoService', '-DryRun'",
        ),
        (
            "dry run changes nothing",
            "[ ! -e \"$HOME/.reticulum\" ]",
            "dry run created .reticulum",
        ),
        (
            "install + rnsd --version",
            "\"$BIN_DIR/rnsd\" --version",
            "'rnsd.cmd') --version",
        ),
        (
            "re-run hash unchanged",
            "shasum -a 256",
            "Get-FileHash -Algorithm SHA256",
        ),
        (
            "re-run reports the existing config",
            "grep -qF '(already there); not touched'",
            ".Contains('(already there); not touched')",
        ),
        (
            "re-run on the script's own config prints no stanzas",
            "grep -qF 'Make sure it carries these settings'",
            ".Contains('Make sure it carries these settings')",
        ),
        (
            "pre-existing config byte-identical",
            "cmp \"$RUNNER_TEMP/expected-config\"",
            "an existing config was modified",
        ),
        (
            "stanzas printed",
            "grep -q '\\[\\[Team Relay\\]\\]'",
            "stanzas lack [[Team Relay]]",
        ),
        (
            "root refusal",
            "sudo -E env HOME=\"$HOME\"",
            "Administrator is refused without -AllowRoot",
        ),
    ] {
        assert_runs(unix, unix_needle, what);
        assert_runs(windows, windows_needle, what);
    }
}

/// GitHub's pwsh wrapper ends a step with `exit $LASTEXITCODE`, and a Windows relay
/// step's last relay call may be an expected refusal, so each ends with `exit 0`.
#[test]
fn the_windows_relay_steps_end_with_exit_0() {
    let Some(ci) = workflow("ci.yaml") else {
        return;
    };
    let relay_steps: Vec<&Value> = steps(job(&ci, "ci.yaml", "scripts"))
        .iter()
        .filter(|step| is_powershell(step) && run(step).contains("mesh-relay.ps1"))
        .collect();
    assert!(
        relay_steps.len() >= 3,
        "the smoke, the roaming-profile venv plan and the Windows PowerShell 5.1 run are \
         PowerShell steps that run mesh-relay.ps1; found {}",
        relay_steps.len()
    );
    for step in relay_steps {
        let last = run(step)
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty() && !line.starts_with('#'));
        assert_eq!(
            last,
            Some("exit 0"),
            "the {:?} step must end with `exit 0` so a relay's last exit code cannot fail it",
            step_name(step)
        );
    }
}
