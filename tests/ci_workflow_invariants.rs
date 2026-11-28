//! Structural checks on `.github/workflows/ci.yaml` and `release.yaml`, read as YAML
//! rather than as text: which jobs gate a merge, the order and conditions of the steps
//! that set up and tear down privileged state, the libtest flags CI passes, where the
//! relay smoke installs to, and the image smoke that gates the Docker Hub push. Comments
//! and step wording are free to change.

mod support;

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
const UNIX_SMOKE: &str = "Mesh Relay Smoke";
const WINDOWS_SMOKE: &str = "Mesh Relay Smoke (Windows)";

fn workflow(file: &str) -> Value {
    let text = support::repo::read(&format!(".github/workflows/{file}"));
    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{file} parses as YAML: {e}"))
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

fn named<'a>(steps: &'a [Value], name: &str) -> &'a Value {
    steps
        .iter()
        .find(|step| step_name(step) == name)
        .unwrap_or_else(|| panic!("no step named {name:?}"))
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
            let tag = words.windows(2).find(|pair| pair[0] == "-t")?[1];
            Some(DockerBuild {
                tag,
                context: words.last()?,
                load: words.contains(&"--load"),
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
    let ci = workflow("ci.yaml");
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
    let ci = workflow("ci.yaml");
    job(&ci, "ci.yaml", "all");
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
/// and skip it whenever that gate fails. It names its runner and carries its own timeout,
/// so a hung run does not hold a runner for the six-hour default.
#[test]
fn informational_jobs_wait_for_nothing_and_run_under_a_timeout() {
    let ci = workflow("ci.yaml");
    for name in INFORMATIONAL_JOBS {
        let job = job(&ci, "ci.yaml", name);
        assert!(
            needs(job).is_empty(),
            "the `{name}` job is informational and runs beside the other jobs, not after them"
        );
        assert!(
            !job["runs-on"].is_null(),
            "the `{name}` job has no `runs-on`"
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
    let ci = workflow("ci.yaml");
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
    let ci = workflow("ci.yaml");
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
    let ci = workflow("ci.yaml");
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

/// The interop job on Linux: the clock starts, the reference is prepared, the namespaces
/// are created as root, the conformance suite runs with the interop tests switched on and
/// un-ignored, the namespaces are removed, and the wall-clock lands in the step summary
/// whatever the suite did.
#[test]
fn the_interop_job_runs_the_suite_between_setup_and_teardown() {
    let ci = workflow("ci.yaml");
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
        let workflow = workflow(file);
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
        let workflow = workflow(file);
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
    let ci = workflow("ci.yaml");
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

/// The release smokes a host-arch build of the image before the multi-platform push, so a
/// broken rnsd layer or entrypoint never reaches Docker Hub. The smoked build is loaded,
/// never pushed, takes the same build-args as the push, is the image the smoke runs, and
/// a failed smoke fails the job. The push step then really pushes, to named tags.
#[test]
fn the_release_smokes_the_image_before_the_push() {
    let release = workflow("release.yaml");
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
        !with["push"].is_null() && with["push"] != Value::Bool(false),
        "the push step must push: {:?}",
        with["push"]
    );
    assert!(
        with["tags"]
            .as_str()
            .is_some_and(|tags| !tags.trim().is_empty()),
        "the push step must name the tags it publishes"
    );
}

/// The merge gate's error tells a contributor where the dependency policy lives, and that
/// anchor has to resolve to a heading in CONTRIBUTING.md.
#[test]
fn the_merge_gate_error_links_a_contributing_heading_that_exists() {
    let ci = workflow("ci.yaml");
    let errors: Vec<&Value> = steps(job(&ci, "ci.yaml", "merge-gates"))
        .iter()
        .filter(|step| run(step).contains("::error::"))
        .collect();
    assert!(
        !errors.is_empty(),
        "no merge-gates step prints an ::error::"
    );
    for step in errors {
        assert!(
            run(step).contains("CONTRIBUTING.md#dependency-policy"),
            "the merge-gates step {:?} must point a contributor at CONTRIBUTING.md#dependency-policy",
            step_name(step)
        );
    }
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
    let ci = workflow("ci.yaml");
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
/// the home directory each smoke installs into.
#[test]
fn the_relay_smokes_install_under_a_per_run_home() {
    let ci = workflow("ci.yaml");
    let job = job(&ci, "ci.yaml", "scripts");
    let steps = steps(job);
    let under_mesh_home = |value: &Value| {
        value
            .as_str()
            .is_some_and(|v| v.contains("runner.temp") && v.contains("mesh-home"))
    };
    for var in INSTALL_DIRS {
        assert!(
            job["env"][var].is_null(),
            "{var} belongs on the smoke steps, not the whole scripts job"
        );
        let setting: Vec<&str> = steps
            .iter()
            .filter(|step| !step["env"][var].is_null())
            .map(step_name)
            .collect();
        assert_eq!(
            setting,
            [UNIX_SMOKE, WINDOWS_SMOKE],
            "{var} must be set on exactly the two relay smoke steps"
        );
        for name in setting {
            let value = &named(steps, name)["env"][var];
            assert!(
                under_mesh_home(value),
                "{var} on {name:?} must sit under runner.temp/mesh-home: {value:?}"
            );
        }
    }
    for (name, var) in [(UNIX_SMOKE, "HOME"), (WINDOWS_SMOKE, "USERPROFILE")] {
        let value = &named(steps, name)["env"][var];
        assert!(
            under_mesh_home(value),
            "{var} on {name:?} must sit under runner.temp/mesh-home: {value:?}"
        );
    }
}

/// The relay runs under both PowerShell 7 and Windows PowerShell 5.1, so the scripts job
/// has a step under each.
#[test]
fn the_scripts_job_runs_both_powershells() {
    let ci = workflow("ci.yaml");
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

/// PSScriptAnalyzer is pinned to an exact version and that version is both installed and
/// imported, so a newer module already on the runner image cannot win and change the rules.
#[test]
fn psscriptanalyzer_is_pinned_and_imported_at_the_pin() {
    let ci = workflow("ci.yaml");
    let step = steps(job(&ci, "ci.yaml", "scripts"))
        .iter()
        .find(|step| !step["env"]["PSSA_VERSION"].is_null())
        .expect("a scripts step sets PSSA_VERSION");
    let version = step["env"]["PSSA_VERSION"]
        .as_str()
        .expect("PSSA_VERSION is a version string");
    assert!(
        version.split('.').count() >= 2
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())),
        "PSSA_VERSION must be an exact version, not a range or wildcard: {version:?}"
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

/// GitHub's pwsh wrapper ends a step with `exit $LASTEXITCODE`, and a Windows relay
/// step's last relay call may be an expected refusal, so each ends with `exit 0`.
#[test]
fn the_windows_relay_steps_end_with_exit_0() {
    let ci = workflow("ci.yaml");
    let relay_steps: Vec<&Value> = steps(job(&ci, "ci.yaml", "scripts"))
        .iter()
        .filter(|step| matches!(step["shell"].as_str(), Some("pwsh" | "powershell")))
        .filter(|step| run(step).contains("mesh-relay.ps1"))
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
