//! Pins the implementation-debris rules in the bundled agent prompts: the
//! coder's comment/debt/decision/naming rules and matching self-review
//! flags, Sisyphus's debt hand-back and debris-lane handling, the
//! architect's run-closing hygiene pass, and the probe's docs walkthrough.
//! Each test asserts a few distinctive anchor phrases so a rewrite that
//! drops a rule fails here rather than in a downstream review.

use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

fn asset_file(dir: &str, rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(rel);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn agent_file(rel: &str) -> String {
    asset_file("assets/agents", rel)
}

fn skill_file(rel: &str) -> String {
    asset_file("assets/skills", rel)
}

fn assert_anchors(rel: &str, anchors: &[&str]) {
    let text = agent_file(rel);
    for anchor in anchors {
        assert!(text.contains(anchor), "{rel} lost anchor {anchor:?}");
    }
}

/// The fenced verdict template that opens with `sentinel`, up to its closing
/// fence.
fn verdict_block<'a>(text: &'a str, sentinel: &str) -> &'a str {
    text.split(sentinel)
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .unwrap_or_else(|| panic!("no fenced {sentinel} template"))
}

#[test]
fn coder_prompt_pins_debris_rules() {
    assert_anchors(
        "coder/graph.yaml",
        &[
            "7. Comments say what the code cannot",
            "8. Debt has an owner",
            "9. Decisions have a home",
            "10. Names a stranger can read",
            "holds even where the surrounding",
            "`debris-exempt` on a line exempts it",
            "are not style: flag them regardless",
        ],
    );
    assert_anchors(
        "coder/graph.yaml",
        &[
            "- Non-ASCII punctuation or planning-artefact vocabulary",
            "- Decisions or check-ledgers written into CONTRIBUTING or README",
            "- Unowned debt markers",
            "- Plan-coined identifiers that only make sense with the plan open",
        ],
    );
    assert!(
        !agent_file("coder/graph.yaml").contains("DEBT:"),
        "coder must not narrate a DEBT: section: end_success renders a fixed template"
    );
}

#[test]
fn sisyphus_prompt_pins_debris_rules() {
    assert_anchors(
        "sisyphus/config.yaml",
        &[
            "### Debt and decisions after a coder returns",
            "- Leave a same-line owning-task comment on every debt marker",
            "derive the list from its diff:",
            "A `debris` lane blocker",
            "the lint's `debris-exempt` token",
            "under a `Debt left behind:` heading",
            "a `Debris exemptions:` heading",
            "grep -nE '^\\+.*(#!?\\[(allow|expect)\\(|cfg_attr\\(|todo!|unimplemented!|#\\[ignore)'",
        ],
    );
    assert!(
        !agent_file("sisyphus/config.yaml").contains("DEBT:"),
        "sisyphus must not ask for a DEBT: summary channel the coder graph cannot deliver"
    );
}

#[test]
fn architect_prompt_pins_debris_rules() {
    assert_anchors(
        "architect/config.yaml",
        &[
            "2. **Run-closing hygiene pass.**",
            "`TASK-NNN-run-hygiene`",
            "`HYGIENE:` heading (file:line, what, justification)",
            "- a **`## Hygiene pass`** section",
            "task id on the SAME LINE as a debt marker",
            "`python3 <agents dir>/.shared/debris.py --exclude <plans rel> {{project_dir}} $base`",
            "to that spawn's `verification_commands` variable",
            "marked with the lint's `debris-exempt` token",
            "paste the Sisyphus `HYGIENE:` list VERBATIM",
            "if Sisyphus reported a `Debt left behind:` list",
            "report's `Debris exemptions:` list",
        ],
    );
    assert_anchors(
        "architect/README.md",
        &[
            "- **Run-closing hygiene pass**",
            "`## Hygiene pass` section",
            "carve-out: a task id on the same line as a debt marker",
        ],
    );
}

/// The run-closing hygiene pass is only sound when every sweep and the lint
/// diff the same run base: the acceptance text has the architect resolve
/// `$base` ONCE from merge-base and STOP when it is empty (an empty base makes
/// the lint fall back to the worktree and print a vacuous clean). Pins that
/// contract, the `..` rule for the plans exclude, the environment-fault
/// reading of a `debris: error:` tail, and the two gauntlet-spawn sentences --
/// and checks that no command inside the pass re-derives the base inline,
/// which would bypass the STOP guard.
#[test]
fn usage_probe_architect_hygiene_pass_resolves_the_run_base_once_and_reads_lint_errors_as_faults() {
    let config = agent_file("architect/config.yaml");
    let start = config
        .find("2. **Run-closing hygiene pass.**")
        .expect("architect config has the Phase F hygiene step");
    let end = config[start..]
        .find("3. **Roll up follow-ups")
        .map(|i| start + i)
        .expect("the hygiene step is followed by the PR roll-up step");
    let pass = &config[start..end];
    let folded = pass.split_whitespace().collect::<Vec<_>>().join(" ");

    for anchor in [
        "`base=$(git -C {{project_dir}} merge-base {{base_branch}} HEAD)` -- and STOP if it is empty",
        "the flag is omitted when that result is `.` or starts with `..`",
        "whose tail starts `debris: error:` is an environment fault",
        "\"These two are the ONLY criteria:\"",
    ] {
        assert!(pass.contains(anchor), "hygiene pass lost anchor {anchor:?}");
    }
    assert!(
        folded.contains("under the heading \"HYGIENE list (context, not criteria):\""),
        "the gauntlet spawn must paste the HYGIENE list under the context heading"
    );
    assert!(
        folded.contains("over `$base`, the RUN base, not the hygiene BASE SHA"),
        "the run-wide lint in verification_commands must name the RUN base, not the hygiene BASE SHA"
    );
    // os.path.relpath against the git toplevel is what makes `<plans rel>` a
    // plain repo-relative literal the CLI accepts, on every platform.
    assert!(
        pass.contains(
            "print(os.path.relpath(os.path.realpath(p), os.path.realpath(sys.argv[3])))' \"{{plans_dir}}\" \"{{project_dir}}\" \"$(git -C {{project_dir}} rev-parse --show-toplevel)\"`"
        ),
        "the plans exclude must be derived relative to the repository toplevel"
    );
    // Resolved ONCE: the only merge-base invocation in the pass is the `base=`
    // assignment that carries the STOP guard. A sweep that re-runs
    // `$(git merge-base ...)` inline has no guard -- an empty result turns
    // `git diff ..HEAD` into an empty diff that reads as zero hits.
    let merge_base_sites: Vec<&str> = pass
        .lines()
        .filter(|l| l.contains("merge-base"))
        .map(str::trim)
        .collect();
    assert_eq!(
        merge_base_sites.len(),
        1,
        "the run base is resolved once (the guarded `base=` assignment); every other \
         sweep command must diff over `$base`: {merge_base_sites:#?}"
    );
    assert!(
        !pass.contains("$(git merge-base"),
        "no sweep command inside the hygiene pass may re-derive the base inline"
    );
    // The brief, criteria and verification_commands run in fresh shells, so
    // the SHA must be spelled out: a literal `$base` token there expands empty.
    assert!(
        pass.contains("write the resolved SHA literally wherever `$base`"),
        "the architect must substitute the resolved SHA for the `$base` token before sending anything"
    );
    // The lint anchors its diff at the repo root with `:(top,literal,exclude)`;
    // the token sweep must use the same anchoring or the two commands cover
    // different trees when {{project_dir}} is a subdirectory.
    let anchored_sweeps = pass
        .matches("$base..HEAD -- :/ \":(top,literal,exclude)<plans rel>\"")
        .count();
    assert!(
        anchored_sweeps >= 2,
        "both the whole-diff sweep and the debris-exempt token sweep must share the lint's \
         repo-root anchoring and literal exclude (found {anchored_sweeps})"
    );
}

/// The probe's docs walkthrough is specified three times (prompt, README,
/// skill) and the acceptance criteria require the two load-bearing contracts
/// to read the same in all three: when a page counts as "unusable" (two or
/// more voice items failed AND a step could not be followed), and that a
/// docs finding without a backing test cites `<page:section>` where the
/// FAIL format asks for the test file. Rewording one copy must fail here.
#[test]
fn usage_probe_the_docs_walkthrough_contract_reads_the_same_in_all_three_probe_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = [
        ("assets/agents/probe/config.yaml", "USAGE_PROBE: FAIL"),
        ("assets/agents/probe/README.md", "`FAIL`"),
        (
            "assets/skills/usage-pattern-testing/SKILL.md",
            "USAGE_PROBE: FAIL",
        ),
    ];
    for (rel, fail_contract) in files {
        let raw = fs::read_to_string(root.join(rel))
            .unwrap_or_else(|e| panic!("read {rel}: {e}"))
            .replace("\r\n", "\n");
        // The prompt wraps at 100 columns and the skill capitalises AND; the
        // contract is the sentence, not its layout.
        let folded = raw
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        assert!(
            folded.contains("two or more voice items failed and a step could not be followed"),
            "{rel} does not state the unusable-page rule in the shared wording"
        );
        assert!(
            raw.contains("<page:section>"),
            "{rel} does not name `<page:section>` as the slot for a docs finding"
        );
        // `<page:section>` is stated NEXT TO the FAIL contract, not only in a
        // docs paragraph a reader of the verdict format never reaches.
        let lines: Vec<&str> = raw.lines().collect();
        let near_fail = lines.iter().enumerate().any(|(i, l)| {
            l.contains("<page:section>")
                && lines[i.saturating_sub(8)..(i + 8).min(lines.len())]
                    .iter()
                    .any(|n| n.contains(fail_contract))
        });
        assert!(
            near_fail,
            "{rel}: `<page:section>` must appear within 8 lines of the FAIL contract ({fail_contract})"
        );
        for sentinel in [
            "USAGE_PROBE: PASS",
            "USAGE_PROBE: FAIL",
            "USAGE_PROBE: INCONCLUSIVE",
        ] {
            assert!(raw.contains(sentinel), "{rel} lost the sentinel {sentinel}");
        }
    }
    // The checklist the three files point at lives in exactly one place.
    let skill =
        fs::read_to_string(root.join("assets/skills/usage-pattern-testing/SKILL.md")).unwrap();
    assert!(skill.contains("### Phase 3b: consumer-facing docs"));
    for rel in [
        "assets/agents/probe/config.yaml",
        "assets/agents/probe/README.md",
    ] {
        let text = fs::read_to_string(root.join(rel)).unwrap();
        assert!(
            text.contains("`usage-pattern-testing`"),
            "{rel} must point at the skill for the checklist rather than restate it as a second source"
        );
    }
}

/// An instruction to call `fs_grep` is only executable where the tool is
/// granted. The step-runner sweep node's `tools:` allowlist is the exact
/// enabled set for that node, and a graph tool must also be in
/// `global_tools`, so both grants are pinned beside the prose that needs them.
#[test]
fn step_runner_sweep_node_is_granted_the_fs_grep_tool_its_instructions_name() {
    let graph = agent_file("step-runner/graph.yaml");
    let globals = graph
        .split("\nglobal_tools:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .expect("step-runner graph declares global_tools");
    assert!(
        globals.contains("  - fs_grep.sh\n"),
        "step-runner global_tools lost fs_grep.sh: {globals}"
    );

    let node_start = graph
        .find("5. Debt sweep.")
        .expect("step-runner graph keeps the debt sweep step");
    let tools_block = graph[node_start..]
        .split("\n    tools:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n    max_iterations:").next())
        .expect("the sweep node declares a tools block");
    assert!(
        tools_block.contains("      - fs_grep\n"),
        "the sweep node's tools allowlist lost fs_grep: {tools_block}"
    );
    assert!(
        graph[node_start..]
            .split("\n    tools:\n")
            .next()
            .is_some_and(|prose| prose.contains("with the `fs_grep` tool")),
        "the fs_grep grant must sit beside the prose that calls for it"
    );
}

/// The attributes that follow the `#[folder = ...]` line of an embed.
fn embed_attrs(source: &str, folder: &str) -> Vec<String> {
    let folder_line = format!("#[folder = \"{folder}\"]");
    let at = source
        .lines()
        .position(|l| l.trim() == folder_line)
        .unwrap_or_else(|| panic!("no embed names {folder}"));
    assert_eq!(
        source.matches(&folder_line).count(),
        1,
        "exactly one embed covers {folder}, so its exclude guards every script under it"
    );
    source
        .lines()
        .skip(at + 1)
        .take_while(|l| l.trim_start().starts_with("#["))
        .map(|l| l.trim().to_string())
        .collect()
}

/// The bundled agents and functions ship Python that the installer copies
/// verbatim, so a `__pycache__` left by a local run must neither be embedded
/// into the binary (both embeds exclude it) nor land in a commit.
#[test]
fn usage_probe_python_bytecode_is_excluded_from_both_asset_embeds_and_ignored_by_git() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (source, folder) in [
        ("src/config/agent.rs", "assets/agents/"),
        ("src/function/mod.rs", "assets/functions/"),
    ] {
        let text = fs::read_to_string(root.join(source)).unwrap();
        let attrs = embed_attrs(&text, folder);
        assert!(
            attrs
                .iter()
                .any(|a| a == "#[exclude = \"**/__pycache__/*\"]"),
            "the {folder} embed in {source} must exclude bytecode at every depth: {attrs:?}"
        );
    }

    let gitignore = fs::read_to_string(root.join(".gitignore")).unwrap();
    assert!(
        gitignore.lines().any(|l| l.trim() == "__pycache__/"),
        ".gitignore must ignore __pycache__/ so a local lint run never stages bytecode"
    );
}

/// The human names in the shared lint's `UNICODE_NEEDLES` table, parsed from
/// the module source so the pin follows the table.
fn unicode_needle_names() -> Vec<String> {
    let shared = agent_file(".shared/debris.py");
    let table = shared
        .split("UNICODE_NEEDLES = {\n")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("debris.py defines UNICODE_NEEDLES");
    let names: Vec<String> = table
        .lines()
        .filter(|line| line.trim_start().starts_with("\"\\u"))
        .map(|line| {
            line.split("(\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_else(|| panic!("needle row without a name: {line}"))
                .to_string()
        })
        .collect();
    assert!(
        names.len() >= 20,
        "UNICODE_NEEDLES parse found only {} entries: {names:?}",
        names.len()
    );
    names
}

/// The family word every banned-glyph sentence must carry for a needle,
/// keyed by the needle's human name in `UNICODE_NEEDLES`.
fn glyph_family(name: &str) -> &'static str {
    match name {
        "em dash" | "en dash" => "dashes",
        "horizontal bar" => "horizontal bar",
        "non-breaking hyphen" => "non-breaking hyphen",
        "minus sign" => "minus",
        "horizontal ellipsis" => "ellipsis",
        n if n.ends_with("arrow") => "arrows",
        "section sign" => "section signs",
        "almost equal to" => "almost-equal",
        "multiplication sign"
        | "less-than or equal to"
        | "greater-than or equal to"
        | "not equal to" => "multiplication and comparison symbols",
        n if n.ends_with("quotation mark") => "curly quotes",
        other => panic!("UNICODE_NEEDLES gained {other:?}; map it to a glyph family here"),
    }
}

/// The banned-glyph sentences that must agree with the lint, whitespace
/// folded: the code-review skill's Human register bullet, the coder
/// prompt's rule 7, and the usage-pattern-testing docs probe row.
fn banned_glyph_sentences() -> Vec<(&'static str, String)> {
    let skill = skill_file("code-review/SKILL.md");
    let bullet_start = skill
        .find("- **Human register, ASCII punctuation**")
        .expect("code-review skill has the Human register bullet");
    let bullet = skill[bullet_start..]
        .split("\n- **")
        .next()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let skill_banned = bullet
        .split("Banned: ")
        .nth(1)
        .and_then(|rest| rest.split(". ").next())
        .expect("the Human register bullet carries a `Banned:` sentence")
        .to_string();

    let coder = agent_file("coder/graph.yaml")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let coder_banned = coder
        .split("ASCII punctuation only (banned: ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("coder rule 7 carries a parenthesised banned-glyph list")
        .to_string();

    let docs = skill_file("usage-pattern-testing/SKILL.md");
    let docs_banned = docs
        .lines()
        .find(|l| l.starts_with("| ASCII punctuation |"))
        .expect("usage-pattern-testing has the ASCII punctuation probe row")
        .to_string();

    vec![
        ("code-review/SKILL.md", skill_banned),
        ("coder/graph.yaml rule 7", coder_banned),
        ("usage-pattern-testing/SKILL.md", docs_banned),
    ]
}

/// Every glyph family the lint's `UNICODE_NEEDLES` table reports is named
/// in each prose twin of the rule (the code-review skill's Human register
/// bullet, the coder prompt's rule 7, the usage-pattern-testing docs
/// probe row), so a reviewer or author without the lint in context bans
/// the same characters the lint does.
#[test]
fn every_banned_glyph_sentence_names_every_glyph_family_the_debris_lint_reports() {
    let families: std::collections::BTreeSet<&str> = unicode_needle_names()
        .iter()
        .map(|name| glyph_family(name))
        .collect();
    assert!(
        families.len() >= 10,
        "the family mapping collapsed to {families:?}"
    );
    for (twin, banned) in banned_glyph_sentences() {
        for family in &families {
            assert!(
                banned.contains(family),
                "{twin}: the banned-glyph sentence lacks {family:?}: {banned}"
            );
        }
    }
}

/// The debt grep Sisyphus hands back on and the step-runner closes a step
/// with is one regex, byte for byte, and the shared lint's `DEBT_MARKERS`
/// spares task ids on the same markers it names.
#[test]
fn debt_grep_regex_is_byte_identical_in_sisyphus_step_runner_and_debris_markers() {
    const DEBT_GREP: &str =
        r"grep -nE '^\+.*(#!?\[(allow|expect)\(|cfg_attr\(|todo!|unimplemented!|#\[ignore)'";
    for rel in ["sisyphus/config.yaml", "step-runner/graph.yaml"] {
        let text = agent_file(rel);
        assert_eq!(
            text.matches(DEBT_GREP).count(),
            1,
            "{rel} must carry the debt grep exactly once, byte-identical"
        );
    }

    let shared = agent_file(".shared/debris.py");
    let markers = shared
        .split("DEBT_MARKERS = (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("debris.py defines DEBT_MARKERS");
    for marker in [
        "#[allow(",
        "#![allow(",
        "#[expect(",
        "#![expect(",
        "cfg_attr(",
        "todo!",
        "unimplemented!",
        "#[ignore",
    ] {
        assert!(
            markers.contains(&format!("\"{marker}\"")),
            "DEBT_MARKERS lost {marker:?}: {markers}"
        );
    }
}

/// The gauntlet blocker shape Sisyphus routes on comes from two scripts it
/// never reads: the hit prefix from the shared lint's `render`, and the
/// malformed-findings phrase from the gauntlet gate. Each literal is pinned
/// in its producer and in the Sisyphus prompt so neither can drift alone;
/// both graphs seed `last_script_error`, which the fault scripts read.
#[test]
fn sisyphus_routing_literals_match_their_producing_scripts() {
    let sisyphus = agent_file("sisyphus/config.yaml")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let render = agent_file(".shared/debris.py")
        .split("\ndef render(")
        .nth(1)
        .and_then(|rest| rest.split("\ndef ").next())
        .expect("debris.py defines render")
        .to_string();
    assert!(
        render.contains("f\"Implementation debris -- {"),
        "debris.py render lost the hit prefix"
    );
    assert!(
        sisyphus.contains("Implementation debris -- <path>:<line> -- "),
        "sisyphus must route on the render prefix"
    );
    let gate = agent_file("review-gauntlet/scripts/verdict_gate.py");
    for (label, text) in [
        ("verdict_gate.py", &gate),
        ("sisyphus/config.yaml", &sisyphus),
    ] {
        assert!(
            text.contains("malformed debris_findings"),
            "{label} lost the malformed-findings phrase"
        );
    }
    for graph in ["review-gauntlet/graph.yaml", "adversary/graph.yaml"] {
        let text = agent_file(graph);
        let seeds = text
            .split("\ninitial_state:\n")
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .unwrap_or_else(|| panic!("{graph} has an initial_state block"));
        assert!(
            seeds.lines().any(|l| l == "  last_script_error: ''"),
            "{graph} initial_state lost `last_script_error: ''`"
        );
    }
}

/// The lint's report is read as UTF-8 and compared as exact LF text by the
/// Rust callers. Python on Windows would otherwise write the console codec
/// and CRLF; the locale half is proven by running under a forced cp1252
/// stdio, but CRLF cannot be reproduced on unix, so the `newline` literal
/// is pinned here, in the one stream setup the CLI has.
#[test]
fn debris_cli_forces_utf8_and_lf_on_its_streams() {
    let main = agent_file(".shared/debris.py")
        .split("\ndef main(")
        .nth(1)
        .and_then(|rest| rest.split("\ndef ").next())
        .expect("debris.py defines main")
        .to_string();
    assert!(
        main.contains("for stream in (sys.stdout, sys.stderr):"),
        "main() lost the stdout/stderr setup loop"
    );
    assert!(
        main.contains(
            "stream.reconfigure(encoding=\"utf-8\", errors=\"backslashreplace\", newline=\"\\n\")"
        ),
        "main() lost the UTF-8 + LF stream contract: {main}"
    );
}

#[test]
fn probe_prompt_pins_debris_rules() {
    assert_anchors(
        "probe/config.yaml",
        &[
            "3. **Docs walkthrough:**",
            "AND no consumer-facing docs for one (step 3)",
            "read the page as a first-time consumer",
            "cannot be followed is a behavioral finding",
            "when the caller's prompt names docs pages",
        ],
    );
    assert_anchors(
        "probe/README.md",
        &[
            "## Docs walkthrough",
            "cites `<page:section>`",
            "non-blocking observations",
        ],
    );
    for (label, text) in [
        ("probe/README.md", agent_file("probe/README.md")),
        (
            "usage-pattern-testing/SKILL.md",
            skill_file("usage-pattern-testing/SKILL.md"),
        ),
    ] {
        for sentinel in ["USAGE_PROBE: PASS", "USAGE_PROBE: FAIL"] {
            assert!(
                verdict_block(&text, sentinel).contains("Non-blocking observations:"),
                "{label} {sentinel} template must carry the labelled observations slot"
            );
        }
    }
}

/// The lint is wired into both review graphs as a script stage with a
/// fail-closed fallback, and the adversary's verdict carries the lint's
/// summary line. The behavioural tests live in the `request_context` tests
/// module; this pin keeps the routing visible from the integration suite.
#[test]
fn both_review_graphs_route_through_the_debris_lint_stage() {
    let adversary = agent_file("adversary/graph.yaml");
    let run_checks = adversary
        .split("\n  run_checks:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n  debris_lint:\n").next())
        .expect("adversary graph has run_checks followed by debris_lint");
    assert!(
        run_checks.contains("next: debris_lint"),
        "adversary run_checks must hand off to debris_lint"
    );
    let lint = adversary
        .split("\n  debris_lint:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n  map_criteria:\n").next())
        .expect("adversary graph has debris_lint followed by map_criteria");
    for anchor in [
        "script: scripts/debris_lint.py",
        "fallback: pipeline_fault",
        "next: map_criteria",
    ] {
        assert!(
            lint.contains(anchor),
            "adversary debris_lint node lost {anchor:?}"
        );
    }
    let verdict = agent_file("adversary/scripts/verdict.py");
    for anchor in [
        "def debris_summary_line(state):",
        "implementation-debris hit(s)",
    ] {
        assert!(
            verdict.contains(anchor),
            "adversary verdict.py lost {anchor:?}"
        );
    }
    let fault = agent_file("adversary/scripts/pipeline_fault.py");
    assert!(
        fault.contains("last_script_error"),
        "adversary pipeline_fault.py must read the engine's last_script_error"
    );

    let gauntlet = agent_file("review-gauntlet/graph.yaml");
    let lint = gauntlet
        .split("\n  debris_lint:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n  debris_fault:\n").next())
        .expect("gauntlet graph has debris_lint followed by debris_fault");
    for anchor in [
        "script: scripts/debris_lint.py",
        "fallback: debris_fault",
        "next: select_lanes",
    ] {
        assert!(
            lint.contains(anchor),
            "gauntlet debris_lint node lost {anchor:?}"
        );
    }
    let gate = agent_file("review-gauntlet/scripts/verdict_gate.py");
    for anchor in ["debris: PIPELINE-FAULT -- ", "GAUNTLET_REVIEW_INCOMPLETE"] {
        assert!(
            gate.contains(anchor),
            "gauntlet verdict_gate.py lost {anchor:?}"
        );
    }
}

/// Removes the scratch repo even when an assertion panics mid-run.
#[cfg(unix)]
struct ScratchDir(std::path::PathBuf);

#[cfg(unix)]
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The backtick-quoted command on the first architect line containing
/// `needle`, so the test runs what the prompt actually tells the architect to
/// run rather than a copy that could drift from it.
#[cfg(unix)]
fn architect_quoted_command(needle: &str) -> String {
    agent_quoted_command("architect/config.yaml", needle)
}

/// The backtick-quoted command on the first line of `rel` containing
/// `needle`.
#[cfg(unix)]
fn agent_quoted_command(rel: &str, needle: &str) -> String {
    let text = agent_file(rel);
    let line = text
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{rel} has no line containing {needle:?}"));
    let start = line
        .find('`')
        .expect("quoted command opens with a backtick");
    let rest = &line[start + 1..];
    // The FIRST closing backtick ends the command: the sentence may carry a
    // second code span after it, and none of the quoted commands contains an
    // inner backtick (their nesting is `$(...)`).
    let end = rest
        .find('`')
        .filter(|&e| e > 0)
        .expect("quoted command closes with a backtick");
    let command = rest[..end].to_string();
    assert!(
        !command.contains('`'),
        "extracted command carries a backtick: {command}"
    );
    command
}

#[cfg(unix)]
fn sh(cwd: &Path, command: &str) -> std::process::Output {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

#[cfg(unix)]
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The architect's Phase F recipes are pinned as text above; this runs them.
/// The `<plans rel>` one-liner must answer relative to the repo top (not the
/// project dir), through symlinked prefixes, and yield `.`/`..` exactly when
/// the prompt says to omit the exclude; every sweep anchored with `:/` plus
/// `:(top,literal,exclude)<plans rel>` (or the lint's `--exclude`) must give
/// the same answer from a subdirectory project dir and never surface a
/// committed plan file while still surfacing the same debris in `src/`.
#[cfg(unix)]
#[test]
fn usage_probe_architect_plans_rel_recipe_and_anchored_sweeps_hold_on_a_scratch_repo() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("coyote-debris-architect-{unique}"));
    let _guard = ScratchDir(root.clone());
    let repo = root.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("sub")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("src/lib.rs"), "fn main() {}\n").unwrap();
    fs::write(repo.join("sub/keep.txt"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["checkout", "-qb", "feat/run"]);
    fs::create_dir_all(repo.join("plans")).unwrap();
    fs::create_dir_all(repo.join("plans2")).unwrap();
    fs::write(
        repo.join("plans/TASK-001.md"),
        "# TASK-001\nahead of TASK-002 \u{2014} by design\n",
    )
    .unwrap();
    fs::write(repo.join("plans2/notes.md"), "ahead of TASK-002\n").unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "fn main() {}\n// placeholder until TASK-002 lands \u{2014} debris\n// ok \u{2014} debris-exempt: the glyph is the point\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "run"]);
    let base = git(&repo, &["merge-base", "main", "HEAD"]);
    assert_eq!(base.len(), 40, "scratch repo has a run base");
    let sub = repo.join("sub");

    // --- `<plans rel>` recipe, verbatim from the prompt ---
    let recipe = architect_quoted_command("python3 -c 'import os, sys;");
    for token in [
        "{{plans_dir}}",
        "{{project_dir}}",
        "rev-parse --show-toplevel",
    ] {
        assert!(recipe.contains(token), "recipe lost {token:?}: {recipe}");
    }
    assert!(
        !recipe.contains("merge-base"),
        "the recipe is not a second merge-base site: {recipe}"
    );
    let plans_rel = |plans_dir: &str, project_dir: &Path| -> String {
        let cmd = recipe
            .replace("{{plans_dir}}", plans_dir)
            .replace("{{project_dir}}", &project_dir.display().to_string());
        let out = sh(project_dir, &cmd);
        assert!(
            out.status.success(),
            "recipe failed for {plans_dir:?}: {out:?}"
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let outside = root.join("outside/plans");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(repo.join("docs/plans")).unwrap();
    std::os::unix::fs::symlink("docs/plans", repo.join("linkplans")).unwrap();
    std::os::unix::fs::symlink(&outside, repo.join("fakeplans")).unwrap();
    std::os::unix::fs::symlink(&repo, root.join("viarepo")).unwrap();
    assert_eq!(plans_rel("plans", &repo), "plans");
    assert_eq!(
        plans_rel("../plans", &sub),
        "plans",
        "a relative plans dir resolves against the project dir, the answer is relative to the top"
    );
    assert_eq!(
        plans_rel(&repo.join("plans").display().to_string(), &sub),
        "plans"
    );
    assert_eq!(
        plans_rel(".", &repo),
        ".",
        "plans at the top -> `.` -> the flag is omitted"
    );
    assert!(
        plans_rel(&outside.display().to_string(), &repo).starts_with(".."),
        "an outside plans dir -> `..` -> the flag is omitted"
    );
    assert_eq!(
        plans_rel("linkplans", &repo),
        "docs/plans",
        "a symlinked plans dir is realpath'd to its in-repo home"
    );
    assert!(
        plans_rel("fakeplans", &repo).starts_with(".."),
        "an in-repo link to an outside dir cannot fake an inside answer"
    );
    assert_eq!(
        plans_rel("plans", &root.join("viarepo")),
        "plans",
        "a symlinked repo prefix (as macOS /tmp is) never yields ../.. noise"
    );
    fs::remove_file(repo.join("linkplans")).unwrap();
    fs::remove_file(repo.join("fakeplans")).unwrap();
    fs::remove_dir_all(repo.join("docs")).unwrap();

    // --- whole-diff sweep and the debris-exempt token grep, verbatim ---
    let sweep = architect_quoted_command("Sweep the RUN's whole diff");
    let token_grep = architect_quoted_command("| grep -n 'debris-exempt'");
    let lint = architect_quoted_command("debris.py --exclude <plans rel>");
    for (name, cmd) in [("sweep", &sweep), ("token grep", &token_grep)] {
        assert!(
            cmd.contains("-- :/ \":(top,literal,exclude)<plans rel>\""),
            "{name} lost the top-anchored exclude: {cmd}"
        );
    }
    assert!(
        lint.contains("--exclude <plans rel> {{project_dir}} $base"),
        "{lint}"
    );
    let substitute = |cmd: &str, project_dir: &Path| -> String {
        cmd.replace("{{project_dir}}", &project_dir.display().to_string())
            .replace("$base", &base)
            .replace("<plans rel>", "plans")
            .replace(
                "<agents dir>",
                &Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("assets/agents")
                    .display()
                    .to_string(),
            )
    };
    let run = |cmd: &str, project_dir: &Path| -> (Option<i32>, String) {
        let out = sh(project_dir, &substitute(cmd, project_dir));
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };

    let (code, from_root) = run(&sweep, &repo);
    assert_eq!(code, Some(0), "{from_root}");
    assert!(
        !from_root.contains("+++ b/plans/"),
        "committed plan files never reach the sweep: {from_root}"
    );
    assert!(
        from_root.contains("+++ b/plans2/"),
        "a sibling dir is not swallowed: {from_root}"
    );
    assert!(
        from_root.contains("+// placeholder until TASK-002"),
        "{from_root}"
    );
    let (_, from_sub) = run(&sweep, &sub);
    assert_eq!(
        from_sub, from_root,
        "`:/` makes the sweep project-dir independent"
    );

    let (_, tokens) = run(&token_grep, &sub);
    assert_eq!(
        tokens.lines().count(),
        1,
        "exactly the one src token (none from plans/): {tokens}"
    );
    assert!(tokens.contains("the glyph is the point"), "{tokens}");

    let (code, lint_root) = run(&lint, &repo);
    assert_eq!(code, Some(1), "{lint_root}");
    assert!(lint_root.contains("src/lib.rs:2"), "{lint_root}");
    assert!(lint_root.contains("plans2/notes.md:1"), "{lint_root}");
    assert!(!lint_root.contains("plans/TASK-001.md"), "{lint_root}");
    assert!(lint_root.contains("1 debris-exempt line(s)"), "{lint_root}");
    let (code, lint_sub) = run(&lint, &sub);
    assert_eq!(code, Some(1), "{lint_sub}");
    assert_eq!(
        lint_sub, lint_root,
        "`--exclude` is repo-top relative, so a subdirectory project dir reads the same"
    );
    let (code, unexcluded) = run(&lint.replace("--exclude <plans rel> ", ""), &repo);
    assert_eq!(code, Some(1));
    assert!(
        unexcluded.contains("plans/TASK-001.md:2"),
        "without the exclude the plan file hits, so the exclude is load-bearing: {unexcluded}"
    );

    // --- the STOP guard protects a real hazard: an empty base is an empty sweep ---
    let empty = sh(&repo, "git diff ..HEAD -- :/");
    assert!(empty.status.success(), "{empty:?}");
    assert!(
        empty.stdout.is_empty(),
        "`git diff ..HEAD` is an empty diff, which is why the base must be resolved first"
    );
    let (code, degraded) = run(
        &sweep.replace(" \":(top,literal,exclude)<plans rel>\"", ""),
        &sub,
    );
    assert_eq!(
        code,
        Some(0),
        "the exclude-omitted sweep form is still valid: {degraded}"
    );
    assert!(degraded.contains("+++ b/plans/TASK-001.md"), "{degraded}");
}

/// The debt grep is pinned as text above; this runs the step-runner's recipe
/// verbatim on a scratch repo. The `ls-files --others --exclude-standard`
/// feed into `git add -N` must make every untracked file the coder created
/// part of `git diff <base>` whether or not the coder listed it (without it
/// the sweep reads nothing new), a `.gitignore`d artefact must stay unseen,
/// the regex must match every marker in the shared lint's DEBT_MARKERS and
/// no clean line, `-n` must number the diff stream, a clean step must print
/// nothing and exit 1 (grep's no-match exit is the no-debt result), and the
/// Sisyphus twin must read the same hits from the same tree.
#[cfg(unix)]
#[test]
fn usage_probe_step_runner_debt_grep_recipe_reads_the_coders_new_files_on_a_scratch_repo() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("coyote-debris-debt-grep-{unique}"));
    let _guard = ScratchDir(root.clone());

    let markers: Vec<String> = agent_file(".shared/debris.py")
        .split("DEBT_MARKERS = (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("debris.py defines DEBT_MARKERS")
        .split(',')
        .filter_map(|m| {
            m.trim()
                .strip_prefix('"')?
                .strip_suffix('"')
                .map(str::to_string)
        })
        .collect();
    assert_eq!(markers.len(), 8, "{markers:?}");

    let recipe = agent_quoted_command(
        "step-runner/graph.yaml",
        "ls-files --others --exclude-standard -z | xargs -0 -r git -C {{project_dir}} add -N --",
    );
    for token in [
        "git -C {{project_dir}} ls-files --others --exclude-standard -z | xargs -0 -r git -C {{project_dir}} add -N -- && ",
        "git -C {{project_dir}} diff --no-color --no-ext-diff <base, normally HEAD> | grep -nE '",
    ] {
        assert!(recipe.contains(token), "recipe lost {token:?}: {recipe}");
    }
    let twin = agent_quoted_command(
        "sisyphus/config.yaml",
        "ls-files --others --exclude-standard -z | xargs -0 -r git add -N --",
    );
    assert!(
        twin.starts_with(
            "git ls-files --others --exclude-standard -z | xargs -0 -r git add -N -- && git diff --no-color --no-ext-diff <base> | grep -nE '"
        ),
        "{twin}"
    );

    let make_repo = |name: &str| -> (std::path::PathBuf, String) {
        let repo = root.join(name);
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
        fs::write(
            repo.join("src/lib.rs"),
            "fn tracked() {}\n#[allow(dead_code)] // owned by nobody, removed by the step\nfn old() {}\n",
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        (repo, base)
    };
    let step_runner = |repo: &Path, base: &str| -> std::process::Output {
        let cmd = recipe
            .replace("{{project_dir}}", &repo.display().to_string())
            .replace("<base, normally HEAD>", base);
        sh(&root, &cmd)
    };

    // --- a step that created three files and edited a tracked one ---
    let (repo, base) = make_repo("hits");
    fs::write(repo.join("src/new.rs"), "#[allow(dead_code)]\nfn a() {}\n").unwrap();
    let planted: Vec<String> = markers.iter().map(|m| format!("{m}x")).collect();
    fs::write(
        repo.join("src/more.rs"),
        format!("{}\nfn clean() {{}}\n", planted.join("\n")),
    )
    .unwrap();
    fs::write(repo.join("src/lib.rs"), "fn tracked() {}\nfn edited() {}\n").unwrap();
    fs::write(
        repo.join("src/stray.rs"),
        "todo!() // in a file the coder never mentioned\n",
    )
    .unwrap();
    fs::write(repo.join("scratch.log"), "todo!() left by a tool\n").unwrap();

    // Without the `ls-files | xargs add -N` prefix the untracked files are
    // invisible to the diff.
    let bare = recipe
        .split(" && ")
        .nth(1)
        .unwrap()
        .replace("{{project_dir}}", &repo.display().to_string())
        .replace("<base, normally HEAD>", &base);
    let blind = sh(&root, &bare);
    assert_eq!(blind.status.code(), Some(1), "{blind:?}");
    assert!(
        blind.stdout.is_empty(),
        "`git diff <base>` alone never sees untracked files, so `add -N` is load-bearing: {blind:?}"
    );

    let out = step_runner(&repo, &base);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains(":+#[allow(dead_code)]"), "{text}");
    for line in &planted {
        assert!(
            text.contains(&format!(":+{line}")),
            "DEBT_MARKERS entry {line:?} is not matched by the grep: {text}"
        );
    }
    assert!(
        !text.contains("fn clean"),
        "a clean added line is not a hit: {text}"
    );
    assert!(!text.contains("fn edited"), "{text}");
    assert!(
        !text.contains("removed by the step"),
        "only ADDED lines are debt; a marker the step removed is not a hit: {text}"
    );
    assert!(
        text.contains("the coder never mentioned"),
        "a created file the coder did not list is still swept: {text}"
    );
    assert_eq!(
        text.lines().count(),
        2 + planted.len(),
        "one hit per marker line, nothing else: {text}"
    );
    assert!(
        text.lines().all(|l| l
            .split(':')
            .next()
            .is_some_and(|n| n.parse::<u32>().is_ok())),
        "`-n` numbers every hit: {text}"
    );
    // The .gitignore'd artefact stays ignored and its marker unseen.
    assert!(!text.contains("left by a tool"), "{text}");
    let status = git(&repo, &["status", "--porcelain", "--ignored"]);
    let rows: Vec<&str> = status.lines().map(str::trim_start).collect();
    assert!(rows.contains(&"!! scratch.log"), "{status}");
    assert!(
        rows.contains(&"M src/lib.rs"),
        "a tracked edit is untouched by add -N: {status}"
    );
    assert!(
        rows.contains(&"A src/new.rs")
            && rows.contains(&"A src/more.rs")
            && rows.contains(&"A src/stray.rs"),
        "every created file carries an intent-to-add entry: {status}"
    );

    // Sisyphus runs its twin from inside the repo: same hits, same order.
    let twin_cmd = twin.replace("<base>", &base);
    let twin_out = sh(&repo, &twin_cmd);
    assert_eq!(twin_out.status.code(), Some(0), "{twin_out:?}");
    assert_eq!(
        String::from_utf8_lossy(&twin_out.stdout),
        text,
        "sisyphus twin diverges"
    );

    // A repo whose local config forces colour: `--no-color` keeps the escape
    // sequences out of the stream the grep anchors on, so the hit is found.
    git(&repo, &["config", "color.diff", "always"]);
    let out = step_runner(&repo, &base);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let coloured_repo_text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        !coloured_repo_text.contains("\x1b["),
        "colour escapes reached the grep: {coloured_repo_text:?}"
    );
    assert_eq!(
        coloured_repo_text, text,
        "`color.diff always` must not change the hits"
    );

    // --- a clean step: no created files, one clean edit -> silence, exit 1 ---
    let (clean_repo, clean_base) = make_repo("clean");
    fs::write(
        clean_repo.join("src/lib.rs"),
        "fn tracked() {}\nfn also_fine() {}\n",
    )
    .unwrap();
    fs::write(
        clean_repo.join("src/fresh.rs"),
        "// a new file with nothing to own\nfn fresh() {}\n",
    )
    .unwrap();
    let out = step_runner(&clean_repo, &clean_base);
    assert_eq!(
        out.status.code(),
        Some(1),
        "grep's no-match exit is the clean signal: {out:?}"
    );
    assert!(out.stdout.is_empty(), "no output means no debt: {out:?}");
}

/// The debt grep's `ls-files --others --exclude-standard -z | xargs -0 -r
/// git add -N --` feed is what makes "every file the coder created, listed
/// or not" true, so its edges are the recipe's edges: a file inside a brand
/// new directory, names with spaces and non-ASCII bytes (the `-z`/`-0`
/// pair), a step that created nothing (`-r` keeps `git add -N --` from
/// running with no pathspec while the tracked edit is still swept), an
/// artefact ignored through `.git/info/exclude` or an ignored directory
/// (`--exclude-standard`), and a new file the coder already staged (tracked
/// in the index, so in `git diff <base>` without `add -N`).
#[cfg(unix)]
#[test]
fn usage_probe_step_runner_debt_grep_recipe_edges_new_dirs_odd_names_no_new_files_and_ignore_sources()
 {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("coyote-debris-debt-grep-edges-{unique}"));
    let _guard = ScratchDir(root.clone());

    let recipe = agent_quoted_command(
        "step-runner/graph.yaml",
        // Anchored on the diff half so a drift in the `ls-files | xargs add -N`
        // feed is caught by the scenarios below rather than by the scrape.
        "git -C {{project_dir}} diff --no-color --no-ext-diff <base, normally HEAD> | grep -nE '",
    );
    assert!(recipe.contains("add -N"), "{recipe}");
    let make_repo = |name: &str| -> (std::path::PathBuf, String) {
        let repo = root.join(name);
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        fs::write(repo.join("src/lib.rs"), "fn tracked() {}\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        (repo, base)
    };
    let sweep = |repo: &Path, base: &str| -> (Option<i32>, String) {
        let cmd = recipe
            .replace("{{project_dir}}", &repo.display().to_string())
            .replace("<base, normally HEAD>", base);
        let out = sh(&root, &cmd);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };

    // A new file two directories deep, one with a space in its name and one
    // with non-ASCII bytes: all three are swept, each exactly once.
    let (repo, base) = make_repo("names");
    fs::create_dir_all(repo.join("src/deep/er")).unwrap();
    fs::write(
        repo.join("src/deep/er/x.rs"),
        "#[allow(dead_code)] // deep\nfn a() {}\n",
    )
    .unwrap();
    fs::write(repo.join("src/my file.rs"), "todo!() // spaced\n").unwrap();
    fs::write(
        repo.join("src/\u{fc}n\u{ef}code.rs"),
        "unimplemented!() // unicode name\n",
    )
    .unwrap();
    let (code, text) = sweep(&repo, &base);
    assert_eq!(code, Some(0), "{text}");
    for hit in ["// deep", "// spaced", "// unicode name"] {
        assert_eq!(
            text.matches(hit).count(),
            1,
            "a created file is swept once whatever its path looks like ({hit}): {text}"
        );
    }
    let status = git(&repo, &["status", "--porcelain"]);
    assert_eq!(
        status
            .lines()
            .filter(|l| l.trim_start().starts_with("A "))
            .count(),
        3,
        "every created file carries exactly one intent-to-add entry: {status}"
    );

    // A step that created no file but edited a tracked one: the `-r` leaves
    // `git add -N --` unrun (no "Nothing specified" noise, no failure) and
    // the diff still carries the tracked edit's marker.
    let (repo, base) = make_repo("no-new-files");
    fs::write(
        repo.join("src/lib.rs"),
        "fn tracked() {}\n#![allow(unused)] // tracked edit\n",
    )
    .unwrap();
    let cmd = recipe
        .replace("{{project_dir}}", &repo.display().to_string())
        .replace("<base, normally HEAD>", &base);
    let out = sh(&root, &cmd);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(text.contains("// tracked edit"), "{text}");
    assert!(
        err.is_empty(),
        "with nothing untracked the add -N half must stay silent: {err}"
    );

    // Artefacts ignored through `.git/info/exclude` and through an ignored
    // directory are neither added nor swept; the step reads clean (exit 1).
    let (repo, base) = make_repo("ignore-sources");
    fs::write(repo.join(".git/info/exclude"), "scratch.rs\n").unwrap();
    fs::write(repo.join("scratch.rs"), "todo!() // info/exclude\n").unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join("target/gen.rs"), "todo!() // ignored dir\n").unwrap();
    fs::write(repo.join("src/fresh.rs"), "fn fresh() {}\n").unwrap();
    let (code, text) = sweep(&repo, &base);
    assert_eq!(code, Some(1), "{text}");
    assert!(text.is_empty(), "ignored artefacts are not debt: {text}");
    let status = git(&repo, &["status", "--porcelain", "--ignored"]);
    let rows: Vec<&str> = status.lines().map(str::trim_start).collect();
    assert!(rows.contains(&"!! scratch.rs"), "{status}");
    assert!(rows.contains(&"!! target/"), "{status}");
    assert!(rows.contains(&"A src/fresh.rs"), "{status}");

    // A created file the coder already staged is tracked in the index and
    // therefore in `git diff <base>` on its own; the sweep still reads it.
    let (repo, base) = make_repo("staged");
    fs::write(
        repo.join("src/staged.rs"),
        "#[ignore] // staged\nfn t() {}\n",
    )
    .unwrap();
    git(&repo, &["add", "src/staged.rs"]);
    let (code, text) = sweep(&repo, &base);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("// staged"), "{text}");
}

/// The review-gauntlet README's debris bullet is the operator-facing twin
/// of the banned-glyph sentence: it must name every glyph family the lint's
/// `UNICODE_NEEDLES` table reports, like the three prompt twins above.
#[test]
fn review_gauntlet_readme_debris_bullet_names_every_glyph_family_the_lint_reports() {
    let readme = agent_file("review-gauntlet/README.md");
    let start = readme
        .find("- **Implementation debris is linted, not judged**")
        .expect("review-gauntlet README has the debris bullet");
    let bullet = readme[start..]
        .split("\n- **")
        .next()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let families: std::collections::BTreeSet<&str> = unicode_needle_names()
        .iter()
        .map(|name| glyph_family(name))
        .collect();
    assert!(families.len() >= 10, "{families:?}");
    for family in &families {
        assert!(
            bullet.contains(family),
            "review-gauntlet README debris bullet lacks {family:?}: {bullet}"
        );
    }
}

/// The gate's three `debris` fault arms (lint crashed, lint module
/// unavailable, malformed findings) render one blocker shape, byte for
/// byte, so a consumer matching `-- never a pass` on the debris row sees it
/// on every arm and no arm can regress to a pass-shaped line.
#[test]
fn verdict_gate_debris_fault_arms_share_one_never_a_pass_blocker_shape() {
    let gate = agent_file("review-gauntlet/scripts/verdict_gate.py");
    const ARM: &str =
        "blockers.append(f\"debris: PIPELINE-FAULT -- {debris_incomplete} -- never a pass\")";
    assert_eq!(
        gate.matches(ARM).count(),
        3,
        "the three debris fault arms must append the same blocker line"
    );
    let other_debris_faults = gate
        .lines()
        .filter(|l| l.contains("debris: PIPELINE-FAULT") && !l.contains(ARM))
        .collect::<Vec<_>>();
    assert!(
        other_debris_faults.is_empty(),
        "a debris fault line outside the shared arm shape: {other_debris_faults:?}"
    );
    for cause in [
        "debris_incomplete = debris_error.strip()",
        "debris_incomplete = f\"lint module unavailable: {e}\"",
        "debris_incomplete = f\"malformed debris_findings ({shape})\"",
    ] {
        assert!(
            gate.contains(cause),
            "verdict_gate.py lost the arm {cause:?}"
        );
    }
}

/// The debt sweep anchors a grep on `^+` lines of a `git diff` the operator's
/// git produces, so the recipe (both twins) carries `--no-color --no-ext-diff`:
/// a repo whose config forces colour would wrap every `+` in an escape, and a
/// `diff.external` driver would replace the diff body with whatever that tool
/// prints (which can both hide every real marker and forge one). Under both
/// settings at once the step-runner recipe and the sisyphus twin must read
/// the same hits as a plain repo, with no escape byte and no driver output.
/// The architect's `debris-exempt` token grep is a third anchored grep over
/// the same kind of stream and must hold under the same config.
#[cfg(unix)]
#[test]
fn usage_probe_debt_sweep_twins_and_architect_token_grep_hold_under_colour_and_an_external_differ()
{
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("coyote-debris-debt-grep-hostile-{unique}"));
    let _guard = ScratchDir(root.clone());
    fs::create_dir_all(&root).unwrap();

    // Anchored on the `add -N` feed, not on the diff flags: a dropped flag
    // must reach the scenarios below as a behavioural failure, not as a
    // scrape panic.
    let recipe = agent_quoted_command(
        "step-runner/graph.yaml",
        "xargs -0 -r git -C {{project_dir}} add -N -- && git -C {{project_dir}} diff",
    );
    let twin = agent_quoted_command(
        "sisyphus/config.yaml",
        "xargs -0 -r git add -N -- && git diff",
    );
    let token_grep = architect_quoted_command("| grep -n 'debris-exempt'");
    assert!(recipe.contains("<base, normally HEAD>"), "{recipe}");
    assert!(twin.contains("<base>"), "{twin}");

    // An external differ whose output is itself a forged `+todo!()` line: with
    // `--no-ext-diff` dropped the sweep would print this line and none of the
    // real markers, so the probe is RED-capable in both directions.
    let differ = root.join("ext-diff.sh");
    fs::write(
        &differ,
        "#!/bin/sh\necho '+todo!() // EXTERNAL DIFFER RAN'\necho '+// debris-exempt EXTERNAL DIFFER RAN'\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&differ, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let make_repo = |name: &str| -> (std::path::PathBuf, String) {
        let repo = root.join(name);
        fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("src/lib.rs"), "fn tracked() {}\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        (repo, base)
    };
    let hostile = |repo: &Path| {
        git(repo, &["config", "color.diff", "always"]);
        git(repo, &["config", "color.ui", "always"]);
        git(
            repo,
            &["config", "diff.external", &differ.display().to_string()],
        );
    };
    let text_of = |out: &std::process::Output| String::from_utf8_lossy(&out.stdout).to_string();

    // --- the debt sweep: a tracked edit and a created file, both with markers ---
    let (repo, base) = make_repo("debt");
    fs::write(
        repo.join("src/lib.rs"),
        "fn tracked() {}\n#[allow(dead_code)] // tracked edit\nfn y() {}\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/new.rs"),
        "unimplemented!() // created file\n",
    )
    .unwrap();
    let step_cmd = recipe
        .replace("{{project_dir}}", &repo.display().to_string())
        .replace("<base, normally HEAD>", &base);
    let twin_cmd = twin.replace("<base>", &base);
    let plain = text_of(&sh(&root, &step_cmd));
    assert!(
        plain.contains("// tracked edit") && plain.contains("// created file"),
        "control: a plain repo yields both hits: {plain}"
    );

    hostile(&repo);
    // Without the flags git would hand the grep a coloured, driver-written
    // stream: pin that this repo's config really does both, so the asserts
    // below are load-bearing.
    let raw = sh(&repo, &format!("git diff {base}"));
    let raw_text = text_of(&raw);
    assert!(
        raw_text.contains("EXTERNAL DIFFER RAN") && !raw_text.contains("// tracked edit"),
        "fixture: diff.external must replace the diff body: {raw_text:?}"
    );
    let coloured = sh(&repo, &format!("git diff --no-ext-diff {base}"));
    assert!(
        text_of(&coloured).contains("\x1b["),
        "fixture: color.diff=always must colour the stream: {:?}",
        text_of(&coloured)
    );

    for (name, out) in [
        ("step-runner", sh(&root, &step_cmd)),
        ("sisyphus twin", sh(&repo, &twin_cmd)),
    ] {
        let text = text_of(&out);
        assert_eq!(out.status.code(), Some(0), "{name}: {out:?}");
        assert!(
            !text.contains("\x1b["),
            "{name}: colour escapes reached the grep: {text:?}"
        );
        assert!(
            !text.contains("EXTERNAL DIFFER RAN"),
            "{name}: the external differ's output reached the grep: {text}"
        );
        assert_eq!(
            text, plain,
            "{name}: colour and an external differ must not change the hits"
        );
    }

    // --- the architect's debris-exempt token grep over a committed run ---
    let (repo, base) = make_repo("tokens");
    fs::create_dir_all(repo.join("plans")).unwrap();
    fs::write(
        repo.join("plans/TASK-001.md"),
        "// debris-exempt in the plans dir\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "fn tracked() {}\nconst DASH: &str = \"\\u{2014}\"; // debris-exempt the glyph is the point\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "run"]);
    let token_cmd = token_grep
        .replace("{{project_dir}}", &repo.display().to_string())
        .replace("$base", &base)
        .replace("<plans rel>", "plans");
    let plain = text_of(&sh(&repo, &token_cmd));
    assert_eq!(plain.lines().count(), 1, "control: one src token: {plain}");
    hostile(&repo);
    let out = sh(&repo, &token_cmd);
    let text = text_of(&out);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(!text.contains("\x1b["), "{text:?}");
    assert!(!text.contains("EXTERNAL DIFFER RAN"), "{text}");
    assert_eq!(
        text, plain,
        "the token grep must read the same line under hostile config"
    );
    assert!(text.contains("the glyph is the point"), "{text}");
}

/// Resolving a debt hit to its `<file:line>` goes through the `fs_grep` tool
/// with the hit text as the pattern argument: diff text is never pasted into
/// a shell string. Both sweep twins say so, and no prompt in the chain keeps
/// a `git grep`/`grep -F` recipe that would take the pasted text instead.
#[test]
fn debt_sweep_twins_resolve_hits_with_the_fs_grep_tool_and_no_prompt_keeps_a_shell_grep_recipe() {
    for (rel, span_start) in [
        ("step-runner/graph.yaml", "5. Debt sweep."),
        (
            "sisyphus/config.yaml",
            "### Debt and decisions after a coder returns",
        ),
    ] {
        let text = agent_file(rel);
        let start = text
            .find(span_start)
            .unwrap_or_else(|| panic!("{rel} lost its debt sweep span {span_start:?}"));
        let span = &text[start..];
        let span = &span[..span.find("plus any env-gated").unwrap_or(span.len())];
        for needle in [
            "with the `fs_grep` tool",
            "is its pattern",
            "never paste diff text",
            "keep only matches in",
        ] {
            assert!(
                span.contains(needle),
                "{rel} debt sweep lost {needle:?}: {span}"
            );
        }
        assert!(
            !span.contains("git grep") && !span.contains("grep -nF") && !span.contains("grep -F"),
            "{rel} debt sweep still carries a shell grep over pasted hit text: {span}"
        );
    }
    for rel in [
        "step-runner/graph.yaml",
        "step-runner/README.md",
        "sisyphus/config.yaml",
        "architect/config.yaml",
        "coder/graph.yaml",
    ] {
        let text = agent_file(rel);
        assert!(
            !text.contains("git grep -nF") && !text.contains("grep -nF '"),
            "{rel} keeps a fixed-string shell grep that interpolates hit text"
        );
    }
}

/// The architect's run-closing hygiene pass reads the RUN's whole diff with
/// its own eyes (not through a grep), so that `git diff` needs the same
/// `--no-color --no-ext-diff` the token grep beside it carries: under
/// `color.diff=always` the model would read escape bytes, and under a
/// `diff.external` driver it would read the driver's output in place of the
/// diff. Under both at once the scraped sweep must print the plain repo's
/// diff byte for byte -- the `src/` change present, the committed plan file
/// excluded, no escape, no driver text -- and exit 0.
#[cfg(unix)]
#[test]
fn usage_probe_architect_whole_diff_sweep_holds_under_colour_and_an_external_differ() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("coyote-debris-whole-diff-hostile-{unique}"));
    let _guard = ScratchDir(root.clone());
    fs::create_dir_all(&root).unwrap();

    // Anchored on the prose, not on the flags, so a dropped flag reaches the
    // scenario below as a behavioural failure rather than a scrape panic.
    let sweep = architect_quoted_command("Sweep the RUN's whole diff");
    assert!(
        sweep.contains("$base..HEAD") && sweep.contains("<plans rel>"),
        "the whole-diff sweep spans the run and excludes the plans dir: {sweep}"
    );

    let differ = root.join("ext-diff.sh");
    fs::write(
        &differ,
        "#!/bin/sh\necho '+todo!() // EXTERNAL DIFFER RAN'\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&differ, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let repo = root.join("run");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("plans")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("src/lib.rs"), "fn tracked() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(
        repo.join("src/lib.rs"),
        "fn tracked() {}\nfn added_in_the_run() {} // RUN CHANGE\n",
    )
    .unwrap();
    fs::write(
        repo.join("plans/TASK-001.md"),
        "# plan text // PLAN CHANGE\n",
    )
    .unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "run"]);

    let sweep_cmd = sweep
        .replace("{{project_dir}}", &repo.display().to_string())
        .replace("$base", &base)
        .replace("<plans rel>", "plans");
    let text_of = |out: &std::process::Output| String::from_utf8_lossy(&out.stdout).to_string();
    let control = sh(&repo, &sweep_cmd);
    let plain = text_of(&control);
    assert_eq!(control.status.code(), Some(0), "{control:?}");
    assert!(
        plain.contains("+fn added_in_the_run() {} // RUN CHANGE"),
        "control: the run's src change is in the sweep: {plain}"
    );
    assert!(
        !plain.contains("PLAN CHANGE") && !plain.contains("plans/TASK-001.md"),
        "control: the committed plan file is excluded: {plain}"
    );

    git(&repo, &["config", "color.diff", "always"]);
    git(&repo, &["config", "color.ui", "always"]);
    git(
        &repo,
        &["config", "diff.external", &differ.display().to_string()],
    );
    // Pin that this config really does both, so the asserts are load-bearing.
    let raw = text_of(&sh(&repo, &format!("git diff {base}..HEAD")));
    assert!(
        raw.contains("EXTERNAL DIFFER RAN") && !raw.contains("RUN CHANGE"),
        "fixture: diff.external must replace the diff body: {raw:?}"
    );
    let coloured = text_of(&sh(&repo, &format!("git diff --no-ext-diff {base}..HEAD")));
    assert!(
        coloured.contains("\x1b["),
        "fixture: colour forced: {coloured:?}"
    );

    let out = sh(&repo, &sweep_cmd);
    let text = text_of(&out);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        !text.contains("\x1b["),
        "colour escapes reached the architect: {text:?}"
    );
    assert!(
        !text.contains("EXTERNAL DIFFER RAN"),
        "the external differ's output reached the architect: {text}"
    );
    assert_eq!(
        text, plain,
        "the whole-diff sweep must read the same diff under hostile config"
    );
}
