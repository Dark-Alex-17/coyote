//! Conformance suite for the mesh wire protocol.
//!
//! The halves:
//!
//! - `vectors`: data-driven cases that feed valid, boundary and invalid inputs to this crate's
//!   own codecs (announce, propagation node announce, card, message body, knock body, canonical
//!   text) and assert the receiver action expected for each row. Rust only, every
//!   platform.
//! - `env_vectors`: the same for the request layer: frames, the Envelope, the dispatcher's
//!   stages, refusal and version-refusal decoding. Rust only, every platform.
//! - `link_vectors`: what only shows on a live link (size branches, the inbound caps,
//!   handler slots and timeouts, response correlation, version marks), run over the loopback
//!   fixtures of `request::tests::network`. The table is declared everywhere so its rows are
//!   listed on every platform; the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `share_vectors`: the wire-path grammar and the decoders that reuse it, the share set and
//!   the grant store on a share root built per row, the `/list` and `/fetch` handlers driven
//!   in-process over such a root, the requester's page and reply readers, the versioning
//!   of the stores they read (`WirePath`, `ShareSet`, `GrantStore`, `StoreSchema`,
//!   `ListServe`, `FetchServe`, `FetchClient`). Rust only, every platform.
//! - `access_vectors`: the `/access` request and reply tables, the rate rule over a bare
//!   `MeshSlot`, the LXMF carriage and its routing, the human's decision reply and the
//!   requester's correlation of it, and the envoy's outcome wording (`AccessRequest`,
//!   `AccessReply`, `AccessLxmf`, `Decision`, `Disposition`). Rust only, every
//!   platform.
//! - `live_vectors`: the file-sharing surface as two nodes see it: the list, access, decision
//!   and fetch cycle, access admission, the requester's reading of fetch replies, symlinks
//!   under the share root and the grant a reference attachment lends, run over the loopback
//!   node pair of `request::tests::network`. The table is declared everywhere so its rows are
//!   listed on every platform; the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `crate::repl::mesh_share_vectors`: the REPL's attachment predicate over a share root
//!   built per row (`Attachment`). Defined beside the predicate because its rows build the
//!   request context the mesh module may not name; listed here so the registry gathers it
//!   with the rest. Rust only, every platform.
//! - `interop`: the protocol exercised against the pinned Python Reticulum/LXMF reference,
//!   spawned as a subprocess. Those tests are `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`;
//!   `scripts/mesh-interop/setup.sh` prepares the reference and prints the environment they need.
//! - `netns`: two of this crate's nodes in separate Linux network namespaces reaching each
//!   other through an external `rnsd` relay, the two-host topology on one machine. Also
//!   `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`; `scripts/mesh-netns/setup.sh` (root)
//!   creates the namespaces.
//!
//! Every table row carries a descriptive name, which the failure messages print.
//!
//! Platform note: `interop`, `link_vectors::loopback` and `live_vectors::loopback` are
//! `#[cfg(unix)]` because the Python reference harness and the loopback fixtures they drive are
//! unix-only, not because the mesh is; `netns` is `#[cfg(target_os = "linux")]` because network
//! namespaces are a Linux kernel feature. `live_vectors::loopback` also holds the symlink rows,
//! whose fixtures create symlinks, and one `share_vectors` row branches inline on unix
//! permission bits; every vector table and `listed()` stays ungated, so the same rows are
//! listed on every platform. Product code under `src/mesh/` is never cfg-gated; those test
//! modules and sub-probes are the only exemption. Windows mesh behaviour is covered by the unit
//! tests, the platform-independent vectors and manual verification, not by the Python interop
//! matrix.

mod access_vectors;
mod env_vectors;
mod link_vectors;
mod live_vectors;
mod share_vectors;
mod vectors;

#[cfg(unix)]
mod interop;
#[cfg(target_os = "linux")]
mod netns;

/// Which side of a requirement a vector probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A conforming input; the receiver accepts it and the decoded fields are checked.
    Valid,
    /// An input at a cap or an edge the spec fixes (exact length, last allowed value).
    Boundary,
    /// An input the spec names as malformed; the mandated refusal or silence is checked.
    Invalid,
}

/// One row of a module's vector table, as `all_listed` gathers it.
pub(crate) struct Listed {
    pub name: &'static str,
    pub kind: Kind,
    pub family: &'static str,
}

fn all_listed() -> Vec<Listed> {
    let mut listed = vectors::listed();
    listed.extend(env_vectors::listed());
    listed.extend(link_vectors::listed());
    listed.extend(share_vectors::listed());
    listed.extend(access_vectors::listed());
    listed.extend(live_vectors::listed());
    listed.extend(crate::repl::mesh_share_vectors::listed());
    listed
}

#[cfg(test)]
mod tests {
    use super::{Kind, all_listed};
    use crate::mesh::request::{
        ACCESS_PATH, FETCH_PATH, KNOCK_PATH, LIST_PATH, MESSAGE_PATH, STATUS_PATH,
    };

    use std::path::Path;

    #[test]
    fn every_listed_row_has_a_name_a_kind_and_a_family() {
        let listed = all_listed();
        assert!(!listed.is_empty(), "no vectors are listed");
        assert!(
            listed.iter().any(|row| row.kind == Kind::Invalid),
            "no Invalid vector is listed"
        );
        if let Some(row) = listed.iter().find(|row| row.family.is_empty()) {
            panic!("a vector row has an empty family: {}", row.name);
        }
        if let Some(row) = listed.iter().find(|row| row.name.is_empty()) {
            panic!("a {} vector row has an empty name", row.family);
        }
    }

    /// The attachment family is defined beside the REPL predicate it drives; this runs it
    /// with the rest of the conformance slice so `cargo test mesh::conformance` covers every
    /// listed family.
    #[test]
    #[serial_test::serial]
    fn the_attachment_family_runs_with_the_conformance_slice() {
        crate::repl::mesh_share_vectors::run_all();
    }

    // The CI job, pinned on the workflow file rather than on a run of it: a Linux-only
    // `mesh-interop` job prepares the reference with `setup.sh`, then runs the conformance
    // suite with the interop tests un-ignored and switched on, under a `timeout-minutes`,
    // records its wall-clock in the step summary, and is kept out of every job's `needs`
    // (the `All` gate included) with a comment saying so. Being outside `needs` is the whole
    // of "informational": the job must NOT also be `continue-on-error`, or a red suite would
    // leave the run and the README badge green. The same job creates the network namespaces
    // for the `netns` suite before the tests and removes them after, whatever the outcome.
    const CI_YAML: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yaml"
    ));

    #[test]
    fn the_ci_job_runs_the_interop_suite_on_linux_with_ignored_tests_and_no_continue_on_error() {
        use serde_yaml::Value;

        let workflow: Value = serde_yaml::from_str(CI_YAML).expect("ci.yaml parses");
        let jobs = workflow["jobs"].as_mapping().expect("jobs is a map");
        let job = &workflow["jobs"]["mesh-interop"];
        assert!(!job.is_null(), "no `mesh-interop` job in ci.yaml");

        assert_eq!(job["runs-on"].as_str(), Some("ubuntu-latest"), "Linux-only");
        assert!(
            job["timeout-minutes"].as_u64().is_some_and(|m| m > 0),
            "timeout-minutes is set: {:?}",
            job["timeout-minutes"]
        );
        assert!(
            job["continue-on-error"].is_null(),
            "the job must not be continue-on-error; a red interop suite has to be a red job: {:?}",
            job["continue-on-error"]
        );

        // Not a prerequisite of anything, `all` included.
        assert!(!workflow["jobs"]["all"].is_null(), "the `all` gate exists");
        for (name, other) in jobs {
            let needs: Vec<&str> = match &other["needs"] {
                Value::String(one) => vec![one.as_str()],
                Value::Sequence(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            assert!(
                !needs.contains(&"mesh-interop"),
                "{} lists mesh-interop in its needs",
                name.as_str().unwrap_or("?")
            );
        }
        // ... and the comment above the job says so.
        let lines: Vec<&str> = CI_YAML.lines().collect();
        let at = lines
            .iter()
            .position(|line| line.trim_end() == "  mesh-interop:")
            .expect("the job key is at the two-space indent");
        let comment: String = lines[..at]
            .iter()
            .rev()
            .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
            .filter(|line| line.trim_start().starts_with('#'))
            .map(|line| line.trim_start().trim_start_matches('#').trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            comment.contains("`All`")
                && (comment.contains("not part") || comment.contains("needs")),
            "the comment above the job must say it is not in the `All` gate: {comment:?}"
        );
        assert!(
            comment.contains("CAP_NET_ADMIN") || comment.contains("sudo"),
            "the comment above the job must state the privilege the namespaces need: {comment:?}"
        );

        // The interop setup.sh, then the namespaces, then the suite with the ignored tests
        // un-ignored and switched on, then the namespaces removed whatever the suite did.
        let steps = job["steps"].as_sequence().expect("steps");
        let runs: Vec<(usize, &str)> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, step)| step["run"].as_str().map(|run| (i, run)))
            .collect();
        let setup = runs
            .iter()
            .find(|(_, run)| run.contains("scripts/mesh-interop/setup.sh"))
            .map(|(i, _)| *i)
            .expect("a step runs scripts/mesh-interop/setup.sh");
        let (test, command) = runs
            .iter()
            .find(|(_, run)| run.contains("cargo test"))
            .map(|(i, run)| (*i, *run))
            .expect("a step runs cargo test");
        assert!(setup < test, "setup.sh runs before the suite");
        let (namespaces, namespaces_run) = runs
            .iter()
            .find(|(_, run)| run.contains("scripts/mesh-netns/setup.sh"))
            .map(|(i, run)| (*i, *run))
            .expect("a step runs scripts/mesh-netns/setup.sh");
        assert!(
            namespaces_run.trim_start().starts_with("sudo "),
            "creating the namespaces needs root: {namespaces_run:?}"
        );
        assert!(
            setup < namespaces && namespaces < test,
            "the namespaces are created after the reference and before the suite"
        );
        let (teardown, teardown_step) = steps
            .iter()
            .enumerate()
            .find(|(_, step)| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| run.contains("scripts/mesh-netns/teardown.sh"))
            })
            .expect("a step runs scripts/mesh-netns/teardown.sh");
        assert!(
            teardown_step["run"]
                .as_str()
                .is_some_and(|run| run.trim_start().starts_with("sudo ")),
            "removing the namespaces needs root: {:?}",
            teardown_step["run"]
        );
        assert!(
            teardown > test,
            "the namespaces are removed after the suite"
        );
        assert_eq!(
            teardown_step["if"].as_str().map(str::trim),
            Some("always()"),
            "the namespaces are removed even when the suite fails"
        );
        for (i, step) in steps.iter().enumerate() {
            assert!(
                step["continue-on-error"].is_null(),
                "step {i} ({:?}) must not be continue-on-error either: {:?}",
                step["name"].as_str().unwrap_or("?"),
                step["continue-on-error"]
            );
        }
        let words: Vec<&str> = command.split_whitespace().collect();
        assert!(words.contains(&"COYOTE_MESH_INTEROP=1"), "{command}");
        assert!(words.contains(&"mesh::conformance"), "{command}");
        assert!(words.contains(&"--include-ignored"), "{command}");
        assert!(
            !words.contains(&"--ignored"),
            "libtest rejects --ignored together with --include-ignored: {command}"
        );
        let dashes = words
            .iter()
            .position(|w| *w == "--")
            .expect("libtest args follow --");
        assert!(
            words[dashes..].contains(&"--include-ignored"),
            "--include-ignored is a libtest flag and must follow `--`: {command}"
        );

        // Wall-clock recorded in the step summary, whatever the suite did.
        let summary = steps
            .iter()
            .enumerate()
            .find(|(_, step)| {
                step["run"]
                    .as_str()
                    .is_some_and(|run| run.contains("GITHUB_STEP_SUMMARY"))
            })
            .expect("a step writes the step summary");
        assert!(
            summary.0 > test,
            "the wall-clock is recorded after the suite"
        );
        assert_eq!(
            summary.1["if"].as_str().map(str::trim),
            Some("always()"),
            "the wall-clock step runs even when the suite fails"
        );
        let started_before_setup = steps[..setup].iter().any(|step| {
            step["run"]
                .as_str()
                .is_some_and(|run| run.contains("date +%s") && run.contains("GITHUB_ENV"))
        });
        assert!(
            started_before_setup,
            "the clock starts before setup.sh so the recorded wall-clock covers the reference too"
        );
        assert!(
            summary.1["run"].as_str().unwrap().contains("date +%s"),
            "the summary step reads the clock"
        );
    }

    // The reference peer's served paths and the harness README's account of them, pinned
    // on the files alone so the check runs on every platform, not only where the Python
    // reference can be spawned.
    const HARNESS_README: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-interop/README.md"
    ));
    const REFERENCE_PEER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/mesh-interop/reference_peer.py"
    ));

    #[test]
    fn the_reference_peer_serves_exactly_the_paths_the_readme_says_it_does() {
        let gap = format!(
            "The reference peer does not implement `{KNOCK_PATH}`, `{LIST_PATH}`, `{FETCH_PATH}` or `{ACCESS_PATH}`;"
        );
        let unwrapped = HARNESS_README
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            unwrapped.contains(&gap),
            "scripts/mesh-interop/README.md no longer says {gap:?}"
        );
        // The README must also say what does cover those paths: the Rust-only vector
        // modules, each of which must exist and list rows.
        let covered_by = "the file-sharing paths are covered by Rust-only in-process conformance vectors in `src/mesh/conformance/share_vectors.rs`, `access_vectors.rs` and `live_vectors.rs`";
        assert!(
            unwrapped.contains(covered_by),
            "scripts/mesh-interop/README.md no longer says {covered_by:?}"
        );
        for (module, listed) in [
            ("share_vectors.rs", super::share_vectors::listed().len()),
            ("access_vectors.rs", super::access_vectors::listed().len()),
            ("live_vectors.rs", super::live_vectors::listed().len()),
        ] {
            assert!(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/mesh/conformance")
                    .join(module)
                    .is_file()
                    && listed > 0,
                "the README names {module} as a vector module, which must exist and list rows"
            );
        }

        let mut lines = REFERENCE_PEER.lines();
        let mut registered = Vec::new();
        while let Some(line) = lines.next() {
            if !line.contains(".register_request_handler(") {
                continue;
            }
            let args = lines
                .next()
                .unwrap_or_else(|| panic!("{line:?} has no argument line"));
            let (_, after_quote) = args
                .split_once('"')
                .unwrap_or_else(|| panic!("{args:?} does not open a path literal"));
            let (path, _) = after_quote
                .split_once('"')
                .unwrap_or_else(|| panic!("{args:?} does not close its path literal"));
            registered.push(path);
        }
        assert_eq!(
            registered,
            [STATUS_PATH, MESSAGE_PATH],
            "scripts/mesh-interop/reference_peer.py registers a different set of request paths"
        );
        for path in registered {
            assert!(
                unwrapped.contains(&format!("`{path}`")),
                "scripts/mesh-interop/README.md does not name the served path `{path}`"
            );
        }
    }
}
