//! Contract tests for the shipped verification gate scripts under
//! assets/agents: `trim_output` bounds every transcript, the `verify_*`
//! scripts survive 300 KB command output (the per-argument cap that used to
//! crash them is 128 KiB), and both `fix_loop_gate.sh` scripts route the
//! crash / red / green / budget cases the graphs rely on. Skips when `bash`
//! or `jq` is not on PATH, and on Windows: a bare `bash` spawned without an
//! explicit child PATH resolves to the WSL launcher in System32 ahead of Git
//! Bash, so this POSIX harness is Unix-only (like the step-runner one).

use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const CODER_GATE: &str = "coder/scripts/fix_loop_gate.sh";
const STEP_RUNNER_GATE: &str = "step-runner/scripts/fix_loop_gate.sh";
const TRANSCRIPT_BYTES: usize = 300_000;
const LAST_LINE: &str = "LAST LINE OF TRANSCRIPT";
const DEFAULT_GATE_ERROR: &str = "no error recorded in state";

fn tools_missing() -> bool {
    if cfg!(windows) {
        eprintln!("skipping: POSIX bash script harness");
        return true;
    }
    let missing = which::which("bash").is_err() || which::which("jq").is_err();
    if missing {
        eprintln!("skipping: bash/jq not available");
    }
    missing
}

fn asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel)
}

/// Removes the fixture dir even when an assertion panics mid-run.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An empty project dir with a pre-seeded detection cache, so scripts that
/// call `detect_project` never fall through to the LLM detector.
fn fixture_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-gate-scripts-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(".coyote-project.json"),
        r#"{"type":"unknown","build":"","test":"","check":"","lint":"","fmt":""}"#,
    )
    .unwrap();
    TempDirGuard(dir)
}

/// `total` bytes of numbered 10-byte lines, ending in `LAST_LINE` without a
/// trailing newline so `$(...)` capture cannot alter it.
fn transcript(total: usize) -> String {
    let mut text = String::with_capacity(total);
    let mut i = 0;
    while text.len() + 10 < total - LAST_LINE.len() {
        text.push_str(&format!("{i:09}\n"));
        i += 1;
    }
    while text.len() < total - LAST_LINE.len() {
        text.push('.');
    }
    text.push_str(LAST_LINE);
    assert_eq!(text.len(), total);
    text
}

fn run(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs `snippet` in a bash that has sourced utils.sh, with `$text` bound to
/// the contents of `text_file`.
fn run_util(text_file: &Path, snippet: &str, envs: &[(&str, &str)]) -> (i32, String, String) {
    let script = format!(
        "source '{}'; text=$(cat '{}'; printf x); text=${{text%x}}; {snippet}",
        asset(".shared/utils.sh").display(),
        text_file.display()
    );
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(script)
        .env_remove("COYOTE_GATE_OUTPUT_MAX_BYTES");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    run(cmd.output().expect("spawn bash"))
}

/// Runs a shipped script with `state` delivered through GRAPH_STATE_FILE, so
/// a large state never has to fit in one environment string.
fn run_script(
    rel: &str,
    dir: &Path,
    state: &Value,
    envs: &[(&str, &str)],
) -> (i32, String, String) {
    let state_file = dir.join(format!("state-{}.json", rel.replace('/', "-")));
    fs::write(&state_file, state.to_string()).unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg(asset(rel))
        .env("GRAPH_STATE_FILE", &state_file)
        .env_remove("GRAPH_STATE")
        .env_remove("COYOTE_GATE_OUTPUT_MAX_BYTES")
        .env_remove("BUILD_CMD")
        .env_remove("TEST_CMD")
        .env_remove("FORMAT_CMD")
        .env_remove("LINT_CMD");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    run(cmd.output().expect("spawn bash"))
}

fn run_script_json(rel: &str, dir: &Path, state: &Value, envs: &[(&str, &str)]) -> Value {
    let (code, stdout, stderr) = run_script(rel, dir, state, envs);
    assert_eq!(
        code, 0,
        "{rel} exited {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{rel} emitted invalid JSON ({e}): {stdout}\nstderr: {stderr}"))
}

fn str_field<'a>(out: &'a Value, key: &str) -> &'a str {
    out.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field '{key}' in {out}"))
}

fn assert_trimmed(input: &str, output: &str, max: usize) {
    let head = max / 4;
    let tail = max - head;
    assert!(
        output.starts_with(&input[..head]),
        "head ({head} bytes) not preserved"
    );
    assert!(
        output.ends_with(&input[input.len() - tail..]),
        "tail ({tail} bytes) not preserved"
    );
    let middle = &output[head..output.len() - tail];
    assert!(
        middle.contains("bytes trimmed"),
        "marker missing: {middle:?}"
    );
    assert!(
        middle.contains(&format!("{} bytes trimmed", input.len() - head - tail)),
        "marker miscounts: {middle:?}"
    );
}

#[test]
fn trim_output_returns_short_input_byte_identical() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-short");
    let file = dir.0.join("in.txt");
    fs::write(&file, "0123456789").unwrap();
    let (code, stdout, stderr) = run_util(&file, r#"trim_output "$text""#, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "0123456789");
}

#[test]
fn trim_output_keeps_head_quarter_and_tail_three_quarters() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-large");
    let file = dir.0.join("in.txt");
    let input = transcript(TRANSCRIPT_BYTES);
    fs::write(&file, &input).unwrap();
    let (code, stdout, stderr) = run_util(&file, r#"trim_output "$text""#, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_trimmed(&input, &stdout, 65_536);
    assert!(stdout.starts_with(&input[..16_384]));
    assert!(stdout.ends_with(&input[input.len() - 49_152..]));
}

#[test]
fn trim_output_ignores_non_numeric_env_override() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-env-garbage");
    let file = dir.0.join("in.txt");
    let input = transcript(TRANSCRIPT_BYTES);
    fs::write(&file, &input).unwrap();
    let (_, default_out, _) = run_util(&file, r#"trim_output "$text""#, &[]);
    let (code, stdout, stderr) = run_util(
        &file,
        r#"trim_output "$text""#,
        &[("COYOTE_GATE_OUTPUT_MAX_BYTES", "xyz")],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, default_out);
}

#[test]
fn trim_output_explicit_max_beats_env_override() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-explicit");
    let file = dir.0.join("in.txt");
    let input = transcript(TRANSCRIPT_BYTES);
    fs::write(&file, &input).unwrap();
    let (code, stdout, stderr) = run_util(
        &file,
        r#"trim_output "$text" 1000"#,
        &[("COYOTE_GATE_OUTPUT_MAX_BYTES", "100")],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_trimmed(&input, &stdout, 1000);
}

#[test]
fn trim_output_reads_leading_zero_override_as_decimal() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-octal");
    let file = dir.0.join("in.txt");
    let input = transcript(TRANSCRIPT_BYTES);
    fs::write(&file, &input).unwrap();
    let (code, stdout, stderr) = run_util(
        &file,
        r#"trim_output "$text""#,
        &[("COYOTE_GATE_OUTPUT_MAX_BYTES", "08")],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_trimmed(&input, &stdout, 8);
}

struct VerifyScript {
    rel: &'static str,
    cmd_env: &'static str,
    ok_key: &'static str,
    output_key: &'static str,
    success_next: &'static str,
}

const VERIFY_SCRIPTS: [VerifyScript; 4] = [
    VerifyScript {
        rel: "coder/scripts/verify_build.sh",
        cmd_env: "BUILD_CMD",
        ok_key: "build_ok",
        output_key: "build_output",
        success_next: "verify_tests",
    },
    VerifyScript {
        rel: "coder/scripts/verify_tests.sh",
        cmd_env: "TEST_CMD",
        ok_key: "tests_ok",
        output_key: "tests_output",
        success_next: "self_review",
    },
    VerifyScript {
        rel: "step-runner/scripts/verify_build.sh",
        cmd_env: "BUILD_CMD",
        ok_key: "build_ok",
        output_key: "build_output",
        success_next: "verify_tests",
    },
    VerifyScript {
        rel: "step-runner/scripts/verify_tests.sh",
        cmd_env: "TEST_CMD",
        ok_key: "tests_ok",
        output_key: "tests_output",
        success_next: "edge_case_sweep",
    },
];

fn project_state(dir: &Path) -> Value {
    serde_json::json!({ "project_dir": dir.to_string_lossy() })
}

fn write_transcript(dir: &Path) -> String {
    let file = dir.join("transcript.txt");
    fs::write(&file, transcript(TRANSCRIPT_BYTES)).unwrap();
    format!("cat '{}'", file.display())
}

#[test]
fn verify_scripts_survive_300kb_passing_transcript() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-pass");
    let cmd = write_transcript(&dir.0);
    let state = project_state(&dir.0);
    for script in &VERIFY_SCRIPTS {
        let out = run_script_json(
            script.rel,
            &dir.0,
            &state,
            &[(script.cmd_env, cmd.as_str())],
        );
        assert_eq!(
            out[script.ok_key],
            Value::Bool(true),
            "{}: {out}",
            script.rel
        );
        assert_eq!(
            str_field(&out, "_next"),
            script.success_next,
            "{}",
            script.rel
        );
        let output = str_field(&out, script.output_key);
        assert!(output.contains("bytes trimmed"), "{}: {output}", script.rel);
        assert!(output.ends_with(LAST_LINE), "{}: tail lost", script.rel);
    }
}

#[test]
fn verify_scripts_survive_300kb_failing_transcript() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-fail");
    let cmd = format!("{}; exit 1", write_transcript(&dir.0));
    let state = project_state(&dir.0);
    for script in &VERIFY_SCRIPTS {
        let out = run_script_json(
            script.rel,
            &dir.0,
            &state,
            &[(script.cmd_env, cmd.as_str())],
        );
        assert_eq!(
            out[script.ok_key],
            Value::Bool(false),
            "{}: {out}",
            script.rel
        );
        assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{}", script.rel);
        let output = str_field(&out, script.output_key);
        assert!(output.contains("Exit code: 1"), "{}: {output}", script.rel);
        assert!(output.contains("bytes trimmed"), "{}: {output}", script.rel);
        assert!(output.ends_with(LAST_LINE), "{}: tail lost", script.rel);
    }
}

#[test]
fn verify_format_lint_bounds_300kb_formatter_transcript() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-format");
    let cmd = write_transcript(&dir.0);
    let out = run_script_json(
        "step-runner/scripts/verify_format_lint.sh",
        &dir.0,
        &project_state(&dir.0),
        &[("FORMAT_CMD", cmd.as_str()), ("LINT_CMD", "true")],
    );
    assert_eq!(out["lint_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "verify_build");
    let format_output = str_field(&out, "format_output");
    assert!(format_output.contains("bytes trimmed"), "{format_output}");
    assert!(format_output.ends_with(LAST_LINE), "tail lost");
    assert!(format_output.len() < 128 * 1024, "{}", format_output.len());
}

/// An operator can raise the transcript cap past the 128 KiB per-argument
/// limit; format_output must not ride argv or the script dies with E2BIG.
#[test]
fn verify_format_lint_survives_format_output_above_argv_cap() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-format-200k");
    let cmd = write_transcript(&dir.0);
    let out = run_script_json(
        "step-runner/scripts/verify_format_lint.sh",
        &dir.0,
        &project_state(&dir.0),
        &[
            ("FORMAT_CMD", cmd.as_str()),
            ("LINT_CMD", "true"),
            ("COYOTE_GATE_OUTPUT_MAX_BYTES", "200000"),
        ],
    );
    assert_eq!(out["lint_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "verify_build");
    let format_output = str_field(&out, "format_output");
    assert!(format_output.len() >= 200_000, "{}", format_output.len());
    assert!(format_output.ends_with(LAST_LINE), "tail lost");
}

#[test]
fn verify_format_lint_bounds_300kb_passing_lint_transcript() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-lint-pass");
    let cmd = write_transcript(&dir.0);
    let out = run_script_json(
        "step-runner/scripts/verify_format_lint.sh",
        &dir.0,
        &project_state(&dir.0),
        &[("FORMAT_CMD", "true"), ("LINT_CMD", cmd.as_str())],
    );
    assert_eq!(out["lint_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "verify_build");
    let lint_output = str_field(&out, "lint_output");
    assert!(lint_output.contains("bytes trimmed"), "{lint_output}");
    assert!(lint_output.ends_with(LAST_LINE), "tail lost");
    assert!(lint_output.len() < 128 * 1024, "{}", lint_output.len());
}

struct Gate {
    rel: &'static str,
    retry_target: &'static str,
    green_flags: &'static [&'static str],
}

const GATES: [Gate; 2] = [
    Gate {
        rel: CODER_GATE,
        retry_target: "verify_build",
        green_flags: &["build_ok", "tests_ok"],
    },
    Gate {
        rel: STEP_RUNNER_GATE,
        retry_target: "verify_format_lint",
        green_flags: &["lint_ok", "build_ok", "tests_ok"],
    },
];

const VERIFY_CRASH: &str = "script 'verify_tests' failed: x";
const STALE_CRASH: &str = "script 'route_complexity' failed: x";

fn gate(g: &Gate, dir: &Path, state: Value) -> Value {
    run_script_json(g.rel, dir, &state, &[])
}

#[test]
fn gate_first_verify_crash_retries_verification() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-crash-1");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": VERIFY_CRASH, "tests_ok": false }),
        );
        assert_eq!(str_field(&out, "_next"), g.retry_target, "{}: {out}", g.rel);
        assert_eq!(out["gate_retries"], Value::from(1), "{}: {out}", g.rel);
        assert_eq!(str_field(&out, "gate_error"), VERIFY_CRASH, "{}", g.rel);
        assert_eq!(str_field(&out, "last_script_error"), "", "{}", g.rel);
    }
}

#[test]
fn gate_second_verify_crash_ends_failure() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-crash-2");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({
                "last_script_error": VERIFY_CRASH,
                "tests_ok": false,
                "gate_retries": 1,
            }),
        );
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}: {out}", g.rel);
        assert_eq!(
            str_field(&out, "gate_error"),
            format!("verification gate crashed again after one retry: {VERIFY_CRASH}"),
            "{}",
            g.rel
        );
        assert_eq!(str_field(&out, "last_script_error"), "", "{}", g.rel);
    }
}

#[test]
fn gate_red_run_with_stale_non_verify_error_routes_to_implement_and_resets() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-red-stale");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": STALE_CRASH, "tests_ok": false }),
        );
        assert_eq!(str_field(&out, "_next"), "implement", "{}: {out}", g.rel);
        for flag in g.green_flags {
            assert_eq!(out[*flag], Value::Bool(true), "{}: {flag} in {out}", g.rel);
        }
        assert_eq!(str_field(&out, "last_script_error"), "", "{}", g.rel);
        assert_eq!(str_field(&out, "gate_error"), "", "{}", g.rel);
        assert_eq!(out["gate_retries"], Value::from(0), "{}: {out}", g.rel);
        assert_eq!(out["fix_attempts"], Value::from(1), "{}: {out}", g.rel);
    }
}

#[test]
fn gate_all_green_without_error_reports_default_crash_text() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-green");
    for g in &GATES {
        let out = gate(g, &dir.0, serde_json::json!({}));
        assert_eq!(str_field(&out, "_next"), g.retry_target, "{}: {out}", g.rel);
        assert_eq!(
            str_field(&out, "gate_error"),
            DEFAULT_GATE_ERROR,
            "{}",
            g.rel
        );
    }
}

#[test]
fn gate_all_green_with_stale_non_verify_error_does_not_report_it() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-green-stale");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": STALE_CRASH }),
        );
        assert_eq!(str_field(&out, "_next"), g.retry_target, "{}: {out}", g.rel);
        assert_eq!(
            str_field(&out, "gate_error"),
            DEFAULT_GATE_ERROR,
            "{}",
            g.rel
        );
    }
}

#[test]
fn gate_exhausted_budget_ends_failure_with_cleared_gate_error() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-budget");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "tests_ok": false, "fix_attempts": 5, "max_fix_attempts": 5 }),
        );
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}: {out}", g.rel);
        assert_eq!(str_field(&out, "gate_error"), "", "{}", g.rel);
        assert_eq!(out["fix_attempts"], Value::from(5), "{}: {out}", g.rel);
    }
}

#[test]
fn gate_embeds_200kb_transcript_in_fix_instructions() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-large");
    let tests_output = transcript(200_000);
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "tests_ok": false, "tests_output": tests_output }),
        );
        assert_eq!(str_field(&out, "_next"), "implement", "{}: {out}", g.rel);
        let fix_instructions = str_field(&out, "fix_instructions");
        assert!(fix_instructions.contains(LAST_LINE), "{}: tail lost", g.rel);
        assert!(fix_instructions.len() > 200_000, "{}", g.rel);
    }
}

fn assert_gate_bookkeeping_reset(out: &Value) {
    assert_eq!(out["gate_retries"], Value::from(0), "{out}");
    assert_eq!(str_field(out, "gate_error"), "", "{out}");
    assert_eq!(str_field(out, "last_script_error"), "", "{out}");
}

/// coder: self_review → route_review_result → implement starts a fresh
/// verify cycle, so the gate-crash bookkeeping from the previous cycle must
/// not survive into it.
#[test]
fn coder_route_review_result_findings_reset_gate_bookkeeping() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-review");
    let out = run_script_json(
        "coder/scripts/route_review_result.sh",
        &dir.0,
        &serde_json::json!({
            "review_clean": false,
            "review_notes": "src/lib.rs:7 - unwrap on user input",
            "review_attempts": 0,
            "max_review_attempts": 1,
            "gate_retries": 1,
            "gate_error": VERIFY_CRASH,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    assert_eq!(out["review_attempts"], Value::from(1), "{out}");
    let fi = str_field(&out, "fix_instructions");
    assert!(fi.contains("unwrap on user input"), "{fi}");
    assert_gate_bookkeeping_reset(&out);
}

/// jq's `//` treats `false` as absent, so a `.review_clean // true` read
/// coerced every finding to "clean" and the review loop could never fire.
#[test]
fn coder_route_review_result_does_not_coerce_false_review_clean_to_true() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-review-bool");
    let cases = [
        (
            serde_json::json!({"review_clean": false, "review_notes": "x"}),
            "implement",
        ),
        (serde_json::json!({"review_clean": true}), "end_success"),
        (serde_json::json!({}), "end_success"),
    ];
    for (state, expected) in cases {
        let out = run_script_json("coder/scripts/route_review_result.sh", &dir.0, &state, &[]);
        assert_eq!(str_field(&out, "_next"), expected, "state {state} → {out}");
    }
}

/// step-runner: every implement → verify cycle passes through
/// route_coder_result, the single choke point that resets the bookkeeping.
#[test]
fn step_runner_route_coder_result_complete_resets_gate_bookkeeping() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-coder");
    let out = run_script_json(
        "step-runner/scripts/route_coder_result.sh",
        &dir.0,
        &serde_json::json!({
            "coder_result": "Implemented the thing.\n\nCODER_COMPLETE",
            "gate_retries": 1,
            "gate_error": VERIFY_CRASH,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "verify_format_lint", "{out}");
    assert_gate_bookkeeping_reset(&out);
}

/// STEP_FAILED renders `gate_error`; a crash message recovered earlier in the
/// cycle must not survive into a failure caused by the coder itself.
#[test]
fn step_runner_route_coder_result_failed_clears_gate_error() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-coder-failed");
    let out = run_script_json(
        "step-runner/scripts/route_coder_result.sh",
        &dir.0,
        &serde_json::json!({
            "coder_result": "Could not make the tests pass.\n\nCODER_FAILED",
            "gate_error": VERIFY_CRASH,
            "last_script_error": VERIFY_CRASH,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "end_failure", "{out}");
    assert_eq!(str_field(&out, "gate_error"), "", "{out}");
    assert_eq!(str_field(&out, "last_script_error"), "", "{out}");
}

#[test]
fn graphs_seed_and_render_the_gate_crash_keys() {
    for agent in ["coder", "step-runner"] {
        let path = asset(&format!("{agent}/graph.yaml"));
        let graph: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let initial = &graph["initial_state"];
        assert_eq!(
            initial["gate_retries"],
            serde_yaml::Value::from(0),
            "{agent}"
        );
        assert_eq!(
            initial["gate_error"],
            serde_yaml::Value::from(""),
            "{agent}"
        );
        assert_eq!(
            initial["last_script_error"],
            serde_yaml::Value::from(""),
            "{agent}"
        );
        let output = graph["nodes"]["end_failure"]["output"]
            .as_str()
            .unwrap_or_else(|| panic!("{agent}: end_failure.output"));
        assert!(output.contains("{{gate_error}}"), "{agent}");
        assert!(output.contains("{{last_script_error}}"), "{agent}");
        let description = graph["nodes"]["fix_loop_gate"]["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{agent}: fix_loop_gate.description"));
        assert!(
            description.contains("once per cycle"),
            "{agent}: {description}"
        );

        let readme = fs::read_to_string(asset(&format!("{agent}/README.md"))).unwrap();
        assert!(readme.contains("gate crashed, 1st time"), "{agent}");
    }
    for rel in [CODER_GATE, STEP_RUNNER_GATE] {
        let script = fs::read_to_string(asset(rel)).unwrap();
        assert!(script.contains("last_script_error"), "{rel}");
        assert!(script.contains("script 'verify_"), "{rel}");
    }
}

// ---------------------------------------------------------------------------
// Usage-probe tests (spec-first, derived from the acceptance criteria before
// reading the implementation). They cover the patterns the matrix above left
// out: boundary sizes, errexit/pipefail safety, the crash-before-budget
// ordering, the second-crash default text, stage selection after the old
// "no failure detected" else-branch was removed, the lint-fail arm, multibyte
// transcripts, and the README anchors AC7 names.
// ---------------------------------------------------------------------------

/// AC1: "emits text unchanged when it fits" is a closed bound — exactly `max`
/// bytes passes through byte-identical; one byte more trims exactly one.
#[test]
fn usage_probe_trim_output_exact_max_passes_and_one_over_trims_one_byte() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-boundary");
    let exact = dir.0.join("exact.txt");
    let exact_text = transcript(65_536);
    fs::write(&exact, &exact_text).unwrap();
    let (code, stdout, stderr) = run_util(&exact, r#"trim_output "$text""#, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout, exact_text,
        "exactly 64 KiB must pass through unchanged"
    );

    let over = dir.0.join("over.txt");
    let over_text = transcript(65_537);
    fs::write(&over, &over_text).unwrap();
    let (code, stdout, stderr) = run_util(&over, r#"trim_output "$text""#, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_trimmed(&over_text, &stdout, 65_536);
    assert!(stdout.contains("[... 1 bytes trimmed"), "{stdout}");
}

/// AC1: callers inherit `set -e -o pipefail` and pipe trim_output into jq. A
/// degenerate max (1 byte → zero-byte head) must neither fail nor break the
/// pipeline, and the env override must be honoured when no positional max is
/// given.
#[test]
fn usage_probe_trim_output_survives_errexit_pipefail_with_tiny_max() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-tiny");
    let file = dir.0.join("in.txt");
    fs::write(&file, "abcdefghij").unwrap();
    let (code, stdout, stderr) = run_util(
        &file,
        r#"set -e -o pipefail; trim_output "$text" | jq -Rsc '{"t": .}'"#,
        &[("COYOTE_GATE_OUTPUT_MAX_BYTES", "1")],
    );
    assert_eq!(code, 0, "{stderr}");
    let out: Value =
        serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let t = str_field(&out, "t");
    assert!(t.ends_with('j'), "tail byte lost: {t:?}");
    assert!(t.contains("9 bytes trimmed"), "{t:?}");
    assert!(!t.starts_with('a'), "head must be 0 bytes at max=1: {t:?}");
}

/// AC5/AC6: the crash branch runs BEFORE the budget check. A verify_* crash on
/// a run whose fix budget is already spent still gets its one verification
/// retry instead of ending as a budget failure.
#[test]
fn usage_probe_gate_verify_crash_beats_exhausted_budget() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-crash-vs-budget");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({
                "last_script_error": VERIFY_CRASH,
                "tests_ok": false,
                "fix_attempts": 5,
                "max_fix_attempts": 5,
            }),
        );
        assert_eq!(str_field(&out, "_next"), g.retry_target, "{}: {out}", g.rel);
        assert_eq!(out["gate_retries"], Value::from(1), "{}: {out}", g.rel);
        assert_eq!(str_field(&out, "gate_error"), VERIFY_CRASH, "{}", g.rel);
        assert!(
            out.get("fix_attempts").is_none(),
            "{}: crash route must not touch the budget: {out}",
            g.rel
        );
    }
}

/// AC5/AC6: second crash with nothing recorded → end_failure, and the prefix
/// wraps the DEFAULT text (not an empty string).
#[test]
fn usage_probe_gate_second_crash_without_recorded_error_prefixes_default_text() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-crash-2-default");
    for g in &GATES {
        let out = gate(g, &dir.0, serde_json::json!({ "gate_retries": 1 }));
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}: {out}", g.rel);
        assert_eq!(
            str_field(&out, "gate_error"),
            format!("verification gate crashed again after one retry: {DEFAULT_GATE_ERROR}"),
            "{}",
            g.rel
        );
        assert_eq!(str_field(&out, "last_script_error"), "", "{}", g.rel);
    }
}

/// AC5: the crash prefix is exactly `script 'verify_` — `script 'verify'`
/// (no underscore) is a stale/unrelated error and a red run routes to implement.
#[test]
fn usage_probe_gate_prefix_requires_the_verify_underscore() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-prefix");
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": "script 'verify' failed: x", "tests_ok": false }),
        );
        assert_eq!(str_field(&out, "_next"), "implement", "{}: {out}", g.rel);
        assert_eq!(str_field(&out, "gate_error"), "", "{}", g.rel);
    }
}

/// AC5: with the old "no failure detected" else-branch gone, the stage
/// selection must still pick the build transcript when the build is red
/// (the tests flag is irrelevant then) and reset every flag on the way out.
#[test]
fn usage_probe_coder_gate_build_red_embeds_build_output_and_resets() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-build-red");
    let out = gate(
        &GATES[0],
        &dir.0,
        serde_json::json!({
            "build_ok": false,
            "build_output": "BUILD BROKE HERE",
            "tests_output": "MUST NOT APPEAR",
            "fix_attempts": 1,
            "max_fix_attempts": 5,
        }),
    );
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    assert_eq!(out["fix_attempts"], Value::from(2), "{out}");
    let fi = str_field(&out, "fix_instructions");
    assert!(fi.contains("failed the build"), "{fi}");
    assert!(fi.contains("BUILD BROKE HERE"), "{fi}");
    assert!(!fi.contains("MUST NOT APPEAR"), "{fi}");
    for key in ["build_ok", "tests_ok"] {
        assert_eq!(out[key], Value::Bool(true), "{key}: {out}");
    }
    assert_eq!(out["gate_retries"], Value::from(0), "{out}");
    assert_eq!(str_field(&out, "gate_error"), "", "{out}");
    assert_eq!(str_field(&out, "last_script_error"), "", "{out}");
}

/// AC6: step-runner stage selection — lint red wins over green build/tests,
/// the lint transcript is embedded, and all three flags reset.
#[test]
fn usage_probe_step_runner_gate_lint_red_embeds_lint_output_and_resets() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-lint-red");
    let out = gate(
        &GATES[1],
        &dir.0,
        serde_json::json!({
            "lint_ok": false,
            "lint_output": "LINT BROKE HERE",
            "build_output": "MUST NOT APPEAR",
            "tests_output": "MUST NOT APPEAR",
        }),
    );
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    let fi = str_field(&out, "fix_instructions");
    assert!(fi.contains("lint stage"), "{fi}");
    assert!(fi.contains("LINT BROKE HERE"), "{fi}");
    assert!(!fi.contains("MUST NOT APPEAR"), "{fi}");
    for key in ["lint_ok", "build_ok", "tests_ok"] {
        assert_eq!(out[key], Value::Bool(true), "{key}: {out}");
    }
    assert_eq!(out["gate_retries"], Value::from(0), "{out}");
    assert_eq!(str_field(&out, "gate_error"), "", "{out}");
    assert_eq!(str_field(&out, "last_script_error"), "", "{out}");
}

/// AC2: the lint FAIL arm of verify_format_lint.sh also takes the transcript
/// on stdin — 300 KB of lint output with a non-zero exit must round-trip with
/// the exit code, the trim marker and the tail.
#[test]
fn usage_probe_verify_format_lint_bounds_300kb_failing_lint_transcript() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-lint-fail");
    let cmd = format!("{}; exit 3", write_transcript(&dir.0));
    let out = run_script_json(
        "step-runner/scripts/verify_format_lint.sh",
        &dir.0,
        &project_state(&dir.0),
        &[("FORMAT_CMD", "true"), ("LINT_CMD", cmd.as_str())],
    );
    assert_eq!(out["lint_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate");
    let lint_output = str_field(&out, "lint_output");
    assert!(lint_output.contains("Exit code: 3"), "{lint_output}");
    assert!(lint_output.contains("bytes trimmed"), "{lint_output}");
    assert!(lint_output.ends_with(LAST_LINE), "tail lost");
    assert!(lint_output.len() < 128 * 1024, "{}", lint_output.len());
}

/// AC1/AC2: trim_output cuts on BYTES (`local LC_ALL=C`), so a transcript of
/// 4-byte characters can be split mid-character at the head/tail cut. jq must
/// still produce valid JSON, the script must exit 0, and the tail must be
/// intact — on both the stdin (`-Rs`) path and the format_output `--rawfile` path.
#[test]
fn usage_probe_verify_scripts_keep_misaligned_multibyte_transcript_as_valid_json() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-multibyte");
    let file = dir.0.join("mb.txt");
    // 1 ASCII byte shifts every 4-byte char off the 16 384 / 49 152 cut points.
    let mut text = String::from("a");
    for _ in 0..75_000 {
        text.push('😀');
    }
    text.push_str(LAST_LINE);
    fs::write(&file, &text).unwrap();
    let cmd = format!("cat '{}'", file.display());
    let state = project_state(&dir.0);

    let out = run_script_json(
        "coder/scripts/verify_tests.sh",
        &dir.0,
        &state,
        &[("TEST_CMD", cmd.as_str())],
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    let tests_output = str_field(&out, "tests_output");
    assert!(tests_output.ends_with(LAST_LINE), "tail lost");
    assert!(
        tests_output.contains("bytes trimmed"),
        "{}",
        tests_output.len()
    );

    let out = run_script_json(
        "step-runner/scripts/verify_format_lint.sh",
        &dir.0,
        &state,
        &[("FORMAT_CMD", cmd.as_str()), ("LINT_CMD", "true")],
    );
    assert_eq!(out["lint_ok"], Value::Bool(true), "{out}");
    let format_output = str_field(&out, "format_output");
    assert!(format_output.ends_with(LAST_LINE), "tail lost");
    assert!(
        format_output.contains("bytes trimmed"),
        "{}",
        format_output.len()
    );
}

/// AC7: the README anchors the matrix test above does not pin — coder's
/// CODER_FAILED line names the crashed-twice cause and its env list has the
/// new override; step-runner's Fault handling section has the verify_* bullet.
#[test]
fn usage_probe_readmes_document_the_gate_crash_contract() {
    let coder = fs::read_to_string(asset("coder/README.md")).unwrap();
    let failed_line = coder
        .lines()
        .skip_while(|l| !l.contains("`CODER_FAILED`"))
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        failed_line.contains("crashed twice"),
        "coder README CODER_FAILED line must mention the gate-crashed-twice cause: {failed_line}"
    );
    assert!(
        coder.contains("- `COYOTE_GATE_OUTPUT_MAX_BYTES`"),
        "coder README env-override list lacks COYOTE_GATE_OUTPUT_MAX_BYTES"
    );
    assert!(
        coder.contains("`Verification gate error:`"),
        "coder README must name the end_failure gate-error anchor"
    );
    assert!(
        coder.contains("max_fix_attempts` (default `5`)"),
        "coder README max_fix_attempts default drifted from graph.yaml"
    );

    let step = fs::read_to_string(asset("step-runner/README.md")).unwrap();
    assert!(
        step.contains("- `COYOTE_GATE_OUTPUT_MAX_BYTES`"),
        "step-runner README env-override list lacks COYOTE_GATE_OUTPUT_MAX_BYTES"
    );
    assert!(
        step.contains("its own internal budget of 5"),
        "step-runner README coder budget drifted from coder graph.yaml max_fix_attempts"
    );
    let fault_section: String = step
        .lines()
        .skip_while(|l| !l.starts_with("## Fault handling"))
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        fault_section.contains("`verify_format_lint`, `verify_build` and `verify_tests` crashes"),
        "step-runner README Fault handling section lacks the verify_* crash bullet:\n{fault_section}"
    );
    assert!(fault_section.contains("once per cycle"), "{fault_section}");
}

/// AC7: every graph's fix_loop_gate `fallback` is still end_failure and the
/// retry target the gate script emits is a real node in that graph, so the
/// dynamic `_next` cannot dangle.
#[test]
fn usage_probe_gate_retry_targets_exist_in_their_graphs() {
    for (agent, target) in [
        ("coder", "verify_build"),
        ("step-runner", "verify_format_lint"),
    ] {
        let graph: serde_yaml::Value = serde_yaml::from_str(
            &fs::read_to_string(asset(&format!("{agent}/graph.yaml"))).unwrap(),
        )
        .unwrap();
        let nodes = graph["nodes"].as_mapping().unwrap();
        assert!(
            nodes.contains_key(serde_yaml::Value::from(target)),
            "{agent}: retry target '{target}' is not a node"
        );
        assert_eq!(
            graph["nodes"]["fix_loop_gate"]["fallback"],
            serde_yaml::Value::from("end_failure"),
            "{agent}"
        );
        assert_eq!(
            graph["nodes"][target]["type"],
            serde_yaml::Value::from("script"),
            "{agent}"
        );
    }
}

// ---------------------------------------------------------------------------
// Round-3 usage-probe tests: the choke-point reset contract (AC7) exercised as
// a cycle rather than one script at a time, the lint-skipped `--rawfile` site
// (AC2), the script-side fix budget default against each graph's seed
// (AC5/AC6), hostile error text through both crash arms, and the remaining
// trim_output override shapes (AC1).
// ---------------------------------------------------------------------------

/// Simulates the engine merging a script node's `state_updates` into graph
/// state: every emitted key except the routing key overwrites the prior value.
fn merge_updates(state: &mut Value, out: &Value) {
    let state = state.as_object_mut().unwrap();
    for (k, v) in out.as_object().unwrap() {
        if k != "_next" {
            state.insert(k.clone(), v.clone());
        }
    }
}

/// AC7 (coder): "one gate retry per verification cycle". A crash → retry
/// leaves gate_retries=1 in state and a second crash in the SAME cycle ends
/// the run; but once route_review_result loops back to implement with
/// findings, the next cycle's first crash must get its own retry again.
#[test]
fn usage_probe_coder_review_loop_restores_the_gate_retry_for_the_next_cycle() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("coder-cycle");
    let g = &GATES[0];
    let mut state = serde_json::json!({
        "gate_retries": 0, "gate_error": "", "last_script_error": "",
        "review_clean": false,
        "review_notes": "src/lib.rs:7 - unwrap on user input",
        "review_attempts": 0, "max_review_attempts": 1,
    });

    // Cycle 1: verify crashed → the gate retries verification once.
    state["last_script_error"] = Value::from(VERIFY_CRASH);
    let out = gate(g, &dir.0, state.clone());
    assert_eq!(str_field(&out, "_next"), g.retry_target, "{out}");
    merge_updates(&mut state, &out);
    assert_eq!(state["gate_retries"], Value::from(1));

    // Same cycle, verify crashed again → budget for this cycle is spent.
    state["last_script_error"] = Value::from(VERIFY_CRASH);
    let out = gate(g, &dir.0, state.clone());
    assert_eq!(str_field(&out, "_next"), "end_failure", "{out}");

    // Instead, the retry ran green and self_review found issues: the route
    // back to implement is the choke point that resets the bookkeeping.
    state["last_script_error"] = Value::from("");
    let out = run_script_json("coder/scripts/route_review_result.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    merge_updates(&mut state, &out);
    assert_eq!(state["gate_retries"], Value::from(0), "{state}");
    assert_eq!(state["gate_error"], Value::from(""), "{state}");

    // Cycle 2: a fresh crash is granted a fresh retry, not end_failure.
    state["last_script_error"] = Value::from(VERIFY_CRASH);
    let out = gate(g, &dir.0, state.clone());
    assert_eq!(str_field(&out, "_next"), g.retry_target, "{out}");
    assert_eq!(out["gate_retries"], Value::from(1), "{out}");
    assert_eq!(str_field(&out, "gate_error"), VERIFY_CRASH);
}

/// AC7 (step-runner): the gate's retry edge (fix_loop_gate →
/// verify_format_lint) bypasses route_coder_result, so gate_retries=1 survives
/// the retry; the next implement → CODER_COMPLETE pass through the choke
/// point resets it and the following cycle's first crash retries again.
#[test]
fn usage_probe_step_runner_coder_complete_restores_the_gate_retry_for_the_next_cycle() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("step-runner-cycle");
    let g = &GATES[1];
    let mut state = serde_json::json!({
        "gate_retries": 0, "gate_error": "", "last_script_error": "",
        "coder_result": "done\n\nCODER_COMPLETE",
    });

    state["last_script_error"] = Value::from(VERIFY_CRASH);
    let out = gate(g, &dir.0, state.clone());
    assert_eq!(str_field(&out, "_next"), g.retry_target, "{out}");
    merge_updates(&mut state, &out);
    assert_eq!(state["gate_retries"], Value::from(1));

    // The retry edge does not pass through route_coder_result: a second
    // crash in the same cycle ends the run.
    state["last_script_error"] = Value::from(VERIFY_CRASH);
    let out = gate(g, &dir.0, state.clone());
    assert_eq!(str_field(&out, "_next"), "end_failure", "{out}");

    // A red run instead → implement → coder completes → choke point resets.
    state["last_script_error"] = Value::from("");
    let out = run_script_json(
        "step-runner/scripts/route_coder_result.sh",
        &dir.0,
        &state,
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "verify_format_lint", "{out}");
    merge_updates(&mut state, &out);
    assert_eq!(state["gate_retries"], Value::from(0), "{state}");

    state["last_script_error"] = Value::from(VERIFY_CRASH);
    let out = gate(g, &dir.0, state.clone());
    assert_eq!(str_field(&out, "_next"), g.retry_target, "{out}");
    assert_eq!(out["gate_retries"], Value::from(1), "{out}");
}

/// AC2: the lint-skipped branch is the third `--rawfile fo` site. With no
/// lint command and the cap raised past 128 KiB, format_output must still
/// reach jq intact and the skip warning must still be emitted.
#[test]
fn usage_probe_verify_format_lint_skip_branch_carries_format_output_above_argv_cap() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-format-nolint-200k");
    let cmd = write_transcript(&dir.0);
    let out = run_script_json(
        "step-runner/scripts/verify_format_lint.sh",
        &dir.0,
        &project_state(&dir.0),
        &[
            ("FORMAT_CMD", cmd.as_str()),
            ("COYOTE_GATE_OUTPUT_MAX_BYTES", "200000"),
        ],
    );
    assert_eq!(out["lint_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "verify_build");
    let format_output = str_field(&out, "format_output");
    assert!(format_output.len() >= 200_000, "{}", format_output.len());
    assert!(format_output.ends_with(LAST_LINE), "tail lost");
    assert!(
        str_field(&out, "lint_output").contains("GATE NOT RUN"),
        "{out}"
    );
}

/// AC5/AC6: with no `max_fix_attempts` in state the script's default must be
/// the graph's seed (coder 5, step-runner 2): one below it still routes to
/// implement and labels the attempt "N of N"; at it the budget is exhausted.
#[test]
fn usage_probe_gate_default_fix_budget_matches_graph_seed() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-default-budget");
    for (agent, g) in [("coder", &GATES[0]), ("step-runner", &GATES[1])] {
        let graph: serde_yaml::Value = serde_yaml::from_str(
            &fs::read_to_string(asset(&format!("{agent}/graph.yaml"))).unwrap(),
        )
        .unwrap();
        let seed = graph["initial_state"]["max_fix_attempts"]
            .as_u64()
            .unwrap_or_else(|| panic!("{agent}: initial_state.max_fix_attempts"));

        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "tests_ok": false, "fix_attempts": seed - 1 }),
        );
        assert_eq!(str_field(&out, "_next"), "implement", "{agent}: {out}");
        assert_eq!(out["fix_attempts"], Value::from(seed), "{agent}: {out}");
        assert!(
            str_field(&out, "fix_instructions").contains(&format!("attempt {seed} of {seed})")),
            "{agent}: {out}"
        );

        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "tests_ok": false, "fix_attempts": seed }),
        );
        assert_eq!(str_field(&out, "_next"), "end_failure", "{agent}: {out}");
        assert_eq!(out["fix_attempts"], Value::from(seed), "{agent}: {out}");
        assert_eq!(str_field(&out, "gate_error"), "", "{agent}");
    }
}

/// Serialization edge: the engine-recorded error can carry anything the
/// failing script printed. Quotes, backslashes, newlines and multibyte text
/// must round-trip byte-equal into gate_error on the first crash and after
/// the "crashed again" prefix on the second.
#[test]
fn usage_probe_gate_error_round_trips_hostile_last_script_error() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-hostile-error");
    let hostile =
        "script 'verify_tests' failed: exit 126 \"quoted\" \\back\n\tline2 ✓ {\"json\":1}";
    for g in &GATES {
        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": hostile, "tests_ok": false }),
        );
        assert_eq!(str_field(&out, "_next"), g.retry_target, "{}: {out}", g.rel);
        assert_eq!(str_field(&out, "gate_error"), hostile, "{}", g.rel);

        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": hostile, "gate_retries": 1 }),
        );
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}: {out}", g.rel);
        assert_eq!(
            str_field(&out, "gate_error"),
            format!("verification gate crashed again after one retry: {hostile}"),
            "{}",
            g.rel
        );
        assert_eq!(str_field(&out, "last_script_error"), "", "{}", g.rel);
    }
}

/// AC1: an empty or negative COYOTE_GATE_OUTPUT_MAX_BYTES is "non-numeric" to
/// the `^[0-9]+$` contract and must fall back to the 64 KiB default — the
/// output is byte-identical to the unset case.
#[test]
fn usage_probe_trim_output_empty_and_negative_override_use_the_default() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-env-empty-negative");
    let file = dir.0.join("in.txt");
    let input = transcript(70_000);
    fs::write(&file, &input).unwrap();
    let (code, default_out, stderr) = run_util(&file, r#"trim_output "$text""#, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert_trimmed(&input, &default_out, 65536);
    for value in ["", "-1", " 100", "1e3"] {
        let (code, stdout, stderr) = run_util(
            &file,
            r#"trim_output "$text""#,
            &[("COYOTE_GATE_OUTPUT_MAX_BYTES", value)],
        );
        assert_eq!(code, 0, "{value:?}: {stderr}");
        assert_eq!(stdout, default_out, "override {value:?} did not fall back");
    }
}

// ---------------------------------------------------------------------------
// Round-4 usage-probe tests: the argument-less `trim_output` call (AC1), every
// `verify_*` node id through the crash-branch prefix (AC5/AC6), the
// route_review_result budget-exhausted arm carrying notes above the argv cap
// (AC7), the step-runner unknown-sentinel branch (AC7), and the graph/README
// anchors AC9 names that the earlier anchor tests left unpinned.
// ---------------------------------------------------------------------------

/// AC1: "missing `$1` reads as empty" — the helper must survive an
/// argument-less call under the callers' `set -euo pipefail` (a bare `$1`
/// would abort on `nounset`) and emit nothing, exit 0; an explicit empty
/// string behaves the same whatever the cap.
#[test]
fn usage_probe_trim_output_without_an_argument_emits_nothing_under_nounset() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trim-no-arg");
    let file = dir.0.join("in.txt");
    fs::write(&file, "ignored").unwrap();
    for snippet in [
        "set -euo pipefail; trim_output; printf '|rc=%d' $?",
        "set -euo pipefail; trim_output ''; printf '|rc=%d' $?",
        "set -euo pipefail; trim_output '' 0; printf '|rc=%d' $?",
    ] {
        let (code, stdout, stderr) =
            run_util(&file, snippet, &[("COYOTE_GATE_OUTPUT_MAX_BYTES", "7")]);
        assert_eq!(code, 0, "{snippet}: {stderr}");
        assert_eq!(stdout, "|rc=0", "{snippet}: {stdout:?} {stderr}");
    }
}

/// AC5/AC6: the crash branch keys off the `script 'verify_` prefix the
/// engine writes for ANY verify node — `verify_build` and
/// `verify_format_lint` crashes must be retried exactly like `verify_tests`,
/// in both gates, with the error echoed into gate_error verbatim.
#[test]
fn usage_probe_gate_crash_branch_fires_for_every_verify_node_id() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("gate-crash-every-node");
    for g in &GATES {
        for node in ["verify_build", "verify_format_lint", "verify_tests"] {
            let err = format!(
                "script '{node}' failed: Script '/x/{node}.sh' failed with exit code Some(126):\nbash: jq: Argument list too long"
            );
            let out = gate(
                g,
                &dir.0,
                serde_json::json!({ "last_script_error": err, "build_ok": false, "fix_attempts": 0 }),
            );
            assert_eq!(
                str_field(&out, "_next"),
                g.retry_target,
                "{} {node}: {out}",
                g.rel
            );
            assert_eq!(
                out["gate_retries"],
                Value::from(1),
                "{} {node}: {out}",
                g.rel
            );
            assert_eq!(str_field(&out, "gate_error"), err, "{} {node}", g.rel);
            assert_eq!(str_field(&out, "last_script_error"), "", "{} {node}", g.rel);
            assert!(
                out.get("fix_attempts")
                    .is_none_or(|v| v.as_u64() == Some(0)),
                "{} {node}: a crash retry must not spend the fix budget: {out}",
                g.rel
            );
        }
    }
}

/// AC7: both arms of coder/route_review_result.sh read `review_notes` from
/// stdin — a 300 000-byte note set (above the 128 KiB per-argument cap the
/// change removes) must reach `fix_instructions` on the findings arm and
/// `review_notes_unresolved` on the budget-exhausted arm, tail intact, with
/// HEAD's routing targets (`implement` / `end_success`) unchanged.
#[test]
fn usage_probe_coder_route_review_result_both_arms_carry_300kb_notes() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-review-300k");
    let notes = transcript(TRANSCRIPT_BYTES);

    let out = run_script_json(
        "coder/scripts/route_review_result.sh",
        &dir.0,
        &serde_json::json!({
            "review_clean": false,
            "review_notes": notes,
            "review_attempts": 0,
            "max_review_attempts": 1,
            "gate_retries": 1,
            "gate_error": VERIFY_CRASH,
            "last_script_error": VERIFY_CRASH,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "implement", "{}", out["_next"]);
    assert_eq!(out["review_attempts"], Value::from(1));
    let fi = str_field(&out, "fix_instructions");
    assert!(
        fi.starts_with("## Self-review feedback (attempt 1 of 1)"),
        "{}",
        &fi[..80]
    );
    assert!(fi.ends_with(LAST_LINE), "findings arm lost the tail");
    assert!(fi.len() > TRANSCRIPT_BYTES, "{}", fi.len());
    assert_gate_bookkeeping_reset(&out);

    let out = run_script_json(
        "coder/scripts/route_review_result.sh",
        &dir.0,
        &serde_json::json!({
            "review_clean": false,
            "review_notes": notes,
            "review_attempts": 1,
            "max_review_attempts": 1,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "end_success", "{}", out["_next"]);
    let unresolved = str_field(&out, "review_notes_unresolved");
    assert!(
        unresolved.starts_with("Shipped with unresolved review notes (budget exhausted):\n"),
        "{}",
        &unresolved[..80]
    );
    assert!(unresolved.ends_with(LAST_LINE), "budget arm lost the tail");
    assert_eq!(
        unresolved.len(),
        "Shipped with unresolved review notes (budget exhausted):\n".len() + TRANSCRIPT_BYTES,
        "budget arm must carry the notes byte-for-byte"
    );
    assert!(out.get("fix_instructions").is_none(), "{out}");
}

/// AC7: step-runner route_coder_result.sh's `*)` (no recognizable sentinel)
/// branch ends the step and must clear the gate-crash keys like CODER_FAILED
/// does, keeping HEAD's blocking_reason; CODER_REJECTED keeps routing to
/// end_rejected.
#[test]
fn usage_probe_step_runner_route_coder_result_unknown_sentinel_clears_gate_keys() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-coder-unknown");
    let out = run_script_json(
        "step-runner/scripts/route_coder_result.sh",
        &dir.0,
        &serde_json::json!({
            "coder_result": "I ran out of context before finishing.",
            "gate_retries": 1,
            "gate_error": VERIFY_CRASH,
            "last_script_error": VERIFY_CRASH,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "end_failure", "{out}");
    assert!(
        str_field(&out, "blocking_reason").contains("no recognizable sentinel"),
        "{out}"
    );
    assert_eq!(str_field(&out, "gate_error"), "", "{out}");
    assert_eq!(str_field(&out, "last_script_error"), "", "{out}");

    let out = run_script_json(
        "step-runner/scripts/route_coder_result.sh",
        &dir.0,
        &serde_json::json!({ "coder_result": "Plan rejected.\n\nCODER_REJECTED" }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "end_rejected", "{out}");
}

/// AC9 anchors left unpinned by the earlier anchor tests: the coder
/// `verify_tests` description names `self_review`; both `fix_loop_gate`
/// descriptions name BOTH triggers; every verify node in each README's
/// mermaid diagram has its dotted "script crashed" edge into fix_loop_gate;
/// the retry edge points at the gate script's actual retry target; and the
/// coder README draws the self_review → route_review_result → implement loop.
#[test]
fn usage_probe_graph_and_readme_anchors_draw_every_crash_edge() {
    for (agent, g, verify_nodes) in [
        ("coder", &GATES[0], &["verify_build", "verify_tests"][..]),
        (
            "step-runner",
            &GATES[1],
            &["verify_format_lint", "verify_build", "verify_tests"][..],
        ),
    ] {
        let graph: serde_yaml::Value = serde_yaml::from_str(
            &fs::read_to_string(asset(&format!("{agent}/graph.yaml"))).unwrap(),
        )
        .unwrap();
        let gate_desc = graph["nodes"]["fix_loop_gate"]["description"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            gate_desc.contains("last_script_error"),
            "{agent}: {gate_desc}"
        );
        assert!(gate_desc.contains("still green"), "{agent}: {gate_desc}");
        assert!(
            gate_desc.contains(&format!("(from {})", g.retry_target)),
            "{agent}: description must name the retry target: {gate_desc}"
        );
        for node in verify_nodes {
            assert_eq!(
                graph["nodes"][*node]["fallback"],
                serde_yaml::Value::from("fix_loop_gate"),
                "{agent}: {node} must fall back to the gate"
            );
        }

        let readme = fs::read_to_string(asset(&format!("{agent}/README.md"))).unwrap();
        for node in verify_nodes {
            let edge = format!("{node} -. \"script crashed\" .-> fix_loop_gate");
            assert!(
                readme.lines().any(|l| l.trim() == edge),
                "{agent} README lacks the mermaid edge `{edge}`"
            );
        }
        let retry_edge = format!(
            "fix_loop_gate -->|\"gate crashed, 1st time\"| {}",
            g.retry_target
        );
        assert!(
            readme.lines().any(|l| l.trim() == retry_edge),
            "{agent} README retry edge must target {}: `{retry_edge}`",
            g.retry_target
        );
    }

    let coder: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(asset("coder/graph.yaml")).unwrap()).unwrap();
    let verify_tests_desc = coder["nodes"]["verify_tests"]["description"]
        .as_str()
        .unwrap();
    assert!(
        verify_tests_desc.contains("self_review"),
        "coder verify_tests description must say it routes to self_review: {verify_tests_desc}"
    );
    let readme = fs::read_to_string(asset("coder/README.md")).unwrap();
    for edge in [
        "verify_tests -->|pass| self_review",
        "route_review_result -->|findings| implement",
    ] {
        assert!(
            readme.lines().any(|l| l.trim() == edge),
            "coder README lacks the review-loop edge `{edge}`"
        );
    }
    assert!(
        readme
            .lines()
            .any(|l| l.trim().starts_with("self_review") && l.contains("--> route_review_result")),
        "coder README lacks the self_review → route_review_result edge"
    );
}
