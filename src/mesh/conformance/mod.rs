//! Conformance suite for `docs/mesh/PROTOCOL.md`, keyed by requirement id.
//!
//! The halves:
//!
//! - `vectors`: data-driven cases that feed valid, boundary and invalid inputs to this crate's
//!   own codecs (announce, propagation node announce, card, message body, knock body, canonical
//!   text) and assert the receiver action the spec mandates for each id. Rust only, every
//!   platform.
//! - `env_vectors`: the same for the R3 transport: frames, the Envelope, the dispatcher's
//!   stages, refusal and version-refusal decoding. Rust only, every platform.
//! - `link_vectors`: the ids that only show on a live link (size branches, the inbound caps,
//!   handler slots and timeouts, response correlation, version marks), run over the loopback
//!   fixtures of `r3::tests::network`. The table is declared everywhere so coverage counts it;
//!   the executor is `#[cfg(unix)]` with the fixtures it drives.
//! - `interop`: the protocol exercised against the pinned Python Reticulum/LXMF reference,
//!   spawned as a subprocess. Those tests are `#[ignore]`d and gated on `COYOTE_MESH_INTEROP=1`;
//!   `scripts/mesh-interop/setup.sh` prepares the reference and prints the environment they need.
//!
//! Every table row names the id it exercises, so `grep MESH-ENV-030 src/mesh/conformance` lands
//! on its vectors, and the tests at the bottom of this file check the ids against the spec and
//! report coverage.
//!
//! Platform note: `interop` and `link_vectors::loopback` are `#[cfg(unix)]` because the Python
//! reference harness and the loopback fixtures they drive are unix-only, not because the mesh
//! is. Product code under `src/mesh/` is never cfg-gated; those two, and the planned netns
//! reachability suite, are the only exemption. Windows mesh behaviour is covered by the unit
//! tests, the platform-independent vectors and manual verification, not by the Python interop
//! matrix.

mod env_vectors;
mod link_vectors;
mod vectors;

#[cfg(unix)]
mod interop;

/// Which side of a requirement a vector probes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A conforming input; the receiver accepts it and the decoded fields are checked.
    Valid,
    /// An input at a cap or an edge the spec fixes (exact length, last allowed value).
    Boundary,
    /// An input the spec names as malformed; the mandated refusal or silence is checked.
    Invalid,
}

/// One row of a module's vector table, as the coverage report sees it.
pub(super) struct Listed {
    pub id: &'static str,
    pub kind: Kind,
    pub family: &'static str,
}

#[cfg(test)]
mod tests {
    use super::{Kind, Listed, env_vectors, link_vectors, vectors};
    use crate::mesh::spec_pins::{SPEC, requirement_ids};

    use std::collections::{BTreeMap, BTreeSet};

    /// The ids that must have at least one vector: every id of the areas whose receiver
    /// actions are fully decidable from bytes, plus every catch-all row of the other areas.
    const REQUIRED_IDS: &[&str] = &[
        "MESH-DEST-001",
        "MESH-DEST-002",
        "MESH-DEST-003",
        "MESH-DEST-004",
        "MESH-DEST-005",
        "MESH-DEST-006",
        "MESH-DEST-007",
        "MESH-DEST-008",
        "MESH-DEST-009",
        "MESH-DEST-010",
        "MESH-ANN-001",
        "MESH-ANN-002",
        "MESH-ANN-003",
        "MESH-ANN-004",
        "MESH-ANN-005",
        "MESH-ANN-006",
        "MESH-ANN-007",
        "MESH-ANN-008",
        "MESH-ANN-009",
        "MESH-ANN-010",
        "MESH-ANN-011",
        "MESH-ANN-012",
        "MESH-ANN-013",
        "MESH-ANN-014",
        "MESH-ANN-015",
        "MESH-ANN-016",
        "MESH-ANN-017",
        "MESH-ANN-018",
        "MESH-ANN-019",
        "MESH-ANN-020",
        "MESH-ANN-021",
        "MESH-ANN-022",
        "MESH-ANN-023",
        "MESH-ANN-024",
        "MESH-ANN-025",
        "MESH-ANN-026",
        "MESH-ANN-027",
        "MESH-ANN-028",
        "MESH-ANN-029",
        "MESH-ANN-030",
        "MESH-ANN-031",
        "MESH-ANN-032",
        "MESH-ANN-033",
        "MESH-ENV-001",
        "MESH-ENV-002",
        "MESH-ENV-003",
        "MESH-ENV-004",
        "MESH-ENV-005",
        "MESH-ENV-006",
        "MESH-ENV-007",
        "MESH-ENV-008",
        "MESH-ENV-009",
        "MESH-ENV-010",
        "MESH-ENV-011",
        "MESH-ENV-012",
        "MESH-ENV-013",
        "MESH-ENV-014",
        "MESH-ENV-015",
        "MESH-ENV-016",
        "MESH-ENV-017",
        "MESH-ENV-018",
        "MESH-ENV-019",
        "MESH-ENV-020",
        "MESH-ENV-021",
        "MESH-ENV-022",
        "MESH-ENV-023",
        "MESH-ENV-024",
        "MESH-ENV-025",
        "MESH-ENV-026",
        "MESH-ENV-027",
        "MESH-ENV-028",
        "MESH-ENV-029",
        "MESH-ENV-030",
        "MESH-ENV-031",
        "MESH-ENV-032",
        "MESH-ENV-033",
        "MESH-ENV-034",
        "MESH-ENV-035",
        "MESH-ENV-036",
        "MESH-ENV-037",
        "MESH-ENV-038",
        "MESH-ENV-039",
        "MESH-ENV-040",
        "MESH-ENV-041",
        "MESH-ENV-042",
        "MESH-ENV-043",
        "MESH-ENV-044",
        "MESH-ENV-045",
        "MESH-ENV-046",
        "MESH-ENV-047",
        "MESH-VER-001",
        "MESH-VER-002",
        "MESH-VER-003",
        "MESH-VER-004",
        "MESH-VER-005",
        "MESH-VER-006",
        "MESH-VER-007",
        "MESH-VER-008",
        "MESH-VER-009",
        "MESH-VER-010",
        "MESH-VER-011",
        "MESH-VER-012",
        "MESH-VER-013",
        "MESH-VER-014",
        "MESH-EXT-001",
        "MESH-EXT-002",
        "MESH-EXT-003",
        "MESH-EXT-004",
        "MESH-EXT-005",
        "MESH-EXT-006",
        "MESH-EXT-007",
        "MESH-EXT-008",
        "MESH-CODE-001",
        "MESH-CODE-002",
        "MESH-KNOCK-002",
        "MESH-KNOCK-018",
        "MESH-KNOCK-022",
        "MESH-KNOCK-024",
        "MESH-STATUS-013",
        "MESH-STATUS-019",
        "MESH-STATUS-022",
        "MESH-STATUS-024",
        "MESH-STATUS-028",
        "MESH-MSG-010",
        "MESH-MSG-017",
        "MESH-MSG-023",
        "MESH-MSG-039",
        "MESH-MSG-048",
        "MESH-MSG-055",
        "MESH-PROP-009",
    ];

    fn all_listed() -> Vec<Listed> {
        let mut listed = vectors::listed();
        listed.extend(env_vectors::listed());
        listed.extend(link_vectors::listed());
        #[cfg(unix)]
        listed.extend(super::interop::listed());
        listed
    }

    fn spec_ids() -> Vec<String> {
        requirement_ids(SPEC).expect("the spec index parses")
    }

    fn area(id: &str) -> &str {
        id.split('-').nth(1).unwrap()
    }

    #[test]
    fn every_vector_id_is_defined_in_the_spec() {
        let spec: BTreeSet<String> = spec_ids().into_iter().collect();
        let unknown: Vec<&str> = all_listed()
            .iter()
            .map(|listed| listed.id)
            .filter(|id| !spec.contains(*id))
            .collect();
        assert_eq!(
            unknown,
            Vec::<&str>::new(),
            "vectors name ids the spec does not define"
        );
        let missing: Vec<&str> = REQUIRED_IDS
            .iter()
            .copied()
            .filter(|id| !spec.contains(*id))
            .collect();
        assert_eq!(
            missing,
            Vec::<&str>::new(),
            "REQUIRED_IDS names ids the spec does not define"
        );
    }

    #[test]
    fn coverage_report() {
        let listed = all_listed();
        let covered: BTreeSet<&str> = listed.iter().map(|listed| listed.id).collect();
        let spec = spec_ids();
        let uncovered: Vec<&str> = spec
            .iter()
            .map(String::as_str)
            .filter(|id| !covered.contains(id))
            .collect();

        let mut per_area: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for id in &spec {
            per_area.entry(area(id)).or_default().0 += 1;
        }
        for id in &covered {
            per_area.entry(area(id)).or_default().1 += 1;
        }
        let mut per_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut per_family: BTreeMap<&str, usize> = BTreeMap::new();
        for row in &listed {
            *per_kind.entry(format!("{:?}", row.kind)).or_default() += 1;
            *per_family.entry(row.family).or_default() += 1;
        }

        println!(
            "conformance vectors: {} rows over {} of {} spec ids",
            listed.len(),
            covered.len(),
            spec.len()
        );
        println!("by kind: {per_kind:?}");
        println!("by family: {per_family:?}");
        println!("by area (ids covered / ids in spec):");
        for (area, (total, covered)) in &per_area {
            println!("  {area}: {covered} / {total}");
        }
        println!("ids without a vector ({}):", uncovered.len());
        for id in &uncovered {
            println!("  {id}");
        }
        assert!(!listed.is_empty(), "no vectors are listed");
        assert!(listed.iter().any(|row| row.kind == Kind::Invalid));
    }

    #[test]
    fn every_required_id_has_a_vector() {
        let covered: BTreeSet<&str> = all_listed().iter().map(|listed| listed.id).collect();
        let missing: Vec<&&str> = REQUIRED_IDS
            .iter()
            .filter(|id| !covered.contains(**id))
            .collect();
        assert_eq!(
            missing,
            Vec::<&&str>::new(),
            "required ids without a vector"
        );
    }

    // The minimum-coverage rule reads: "every id in areas DEST, ANN, ENV, VER, EXT, CODE and
    // every catch-all/`any other value` row in KNOCK/STATUS/MSG/PROP". These derive that set
    // from `docs/mesh/PROTOCOL.md` itself so the hand-maintained `REQUIRED_IDS` above cannot
    // silently drift from it.

    /// Areas whose every id must have a vector.
    const FULLY_COVERED_AREAS: &[&str] = &["DEST", "ANN", "ENV", "VER", "EXT", "CODE"];
    /// Areas where only the catch-all table rows must have a vector.
    const CATCH_ALL_AREAS: &[&str] = &["KNOCK", "STATUS", "MSG", "PROP"];

    /// The `**[MESH-AREA-NNN]**` ids on one spec line, in order.
    fn ids_on(line: &str) -> Vec<&str> {
        line.match_indices("**[MESH-")
            .filter_map(|(start, _)| {
                let from = start + "**[".len();
                let end = line[from..].find("]**")?;
                Some(&line[from..from + end])
            })
            .collect()
    }

    /// A table row whose first cell is a catch-all (`any other ...` or `trailing ...`).
    fn is_catch_all_row(line: &str) -> bool {
        let Some(rest) = line.strip_prefix('|') else {
            return false;
        };
        let first = rest
            .split('|')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        first.starts_with("any other") || first.starts_with("trailing")
    }

    fn required_ids_from_the_spec() -> BTreeSet<String> {
        let mut required: BTreeSet<String> = spec_ids()
            .into_iter()
            .filter(|id| FULLY_COVERED_AREAS.contains(&area(id)))
            .collect();
        for line in SPEC.lines().filter(|line| is_catch_all_row(line)) {
            for id in ids_on(line) {
                if CATCH_ALL_AREAS.contains(&area(id)) {
                    required.insert(id.to_string());
                }
            }
        }
        required
    }

    #[test]
    fn the_spec_derived_required_set_is_the_shape_the_ruling_describes() {
        let derived = required_ids_from_the_spec();
        // 114 ids across the six fully covered areas (10+33+47+14+8+2) plus the catch-all rows.
        assert!(
            derived.len() > 114,
            "no catch-all rows were found: {derived:?}"
        );
        assert!(
            derived.contains("MESH-MSG-010"),
            "the `any other key` body row"
        );
        assert!(
            derived.contains("MESH-PROP-009"),
            "the `any other element` PN row"
        );
        assert!(derived.iter().all(|id| {
            FULLY_COVERED_AREAS.contains(&area(id)) || CATCH_ALL_AREAS.contains(&area(id))
        }));
    }

    #[test]
    fn required_ids_lists_every_id_the_ruling_requires() {
        let derived = required_ids_from_the_spec();
        let listed: BTreeSet<&str> = REQUIRED_IDS.iter().copied().collect();
        let omitted: Vec<&str> = derived
            .iter()
            .map(String::as_str)
            .filter(|id| !listed.contains(id))
            .collect();
        assert_eq!(
            omitted,
            Vec::<&str>::new(),
            "catch-all / fully-covered-area ids the ruling requires but REQUIRED_IDS omits"
        );
    }

    #[test]
    fn every_id_the_ruling_requires_has_a_vector() {
        let covered: BTreeSet<&str> = all_listed().iter().map(|listed| listed.id).collect();
        let missing: Vec<String> = required_ids_from_the_spec()
            .into_iter()
            .filter(|id| !covered.contains(id.as_str()))
            .collect();
        assert_eq!(
            missing,
            Vec::<String>::new(),
            "ids the minimum-coverage ruling requires that have no vector"
        );
    }

    // Acceptance (f), pinned on the workflow file rather than on a run of it: a Linux-only
    // `mesh-interop` job prepares the reference with `setup.sh`, then runs the conformance
    // suite with the interop tests un-ignored and switched on, under a `timeout-minutes`,
    // records its wall-clock in the step summary, and is kept out of every job's `needs`
    // (the `All` gate included) with a comment saying so.
    const CI_YAML: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/.github/workflows/ci.yaml"
    ));

    #[test]
    fn the_ci_job_runs_the_interop_suite_the_way_the_ruling_says() {
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

        // setup.sh, then the suite with the interop tests un-ignored and switched on.
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
}
