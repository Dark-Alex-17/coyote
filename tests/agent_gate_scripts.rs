//! Contract tests for the shipped verification gate scripts under
//! assets/agents: `trim_output` bounds every transcript, the `verify_*`
//! scripts survive 300 KB command output (the per-argument cap that used to
//! crash them is 128 KiB), and both `fix_loop_gate.sh` scripts route the
//! crash / red / green / budget cases the graphs rely on. On unix `bash` and
//! `jq` must be on PATH (asserted, so a missing tool cannot hide the
//! harness); elsewhere the harness skips without them. Every spawn goes
//! through `bash_program`, which resolves bash on PATH and refuses the WSL
//! launcher Windows ships in System32: `Command::new("bash")` by bare name
//! finds that launcher ahead of Git Bash, so the guard and the spawn must
//! agree on one absolute path. Script output is read with CRLF folded to LF
//! because Git Bash and a native jq.exe may end lines that way. That
//! Windows arm (the System32 filter, the CRLF fold) is exercised only on
//! the windows-latest CI lane. The jq argv pin at the bottom only reads the
//! scripts and so runs even where the harness skips.

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

/// The first bash on PATH outside System32. The one Windows puts there is
/// the WSL launcher, which is not a shell: it prints a banner about missing
/// distributions and exits 1.
fn bash_program() -> Option<PathBuf> {
    which::which_all("bash")
        .ok()?
        .find(|path| is_outside_system32(path))
}

/// Twin of `is_outside_system32` in src/testing.rs; change both together.
/// Splits on both separators because `Path::components` leaves a Windows
/// path whole on unix, where this is unit-tested.
fn is_outside_system32(path: &Path) -> bool {
    !path
        .to_string_lossy()
        .split(['/', '\\'])
        .any(|part| part.eq_ignore_ascii_case("system32"))
}

#[test]
fn is_outside_system32_reads_windows_and_unix_paths() {
    for launcher in [
        r"C:\Windows\System32\bash.exe",
        r"c:\windows\SYSTEM32\bash.exe",
        "C:/Windows/System32/bash.exe",
        r"\\?\C:\Windows\System32\bash.exe",
    ] {
        assert!(!is_outside_system32(Path::new(launcher)), "{launcher}");
    }
    for shell in [r"C:\Program Files\Git\bin\bash.exe", "/usr/bin/bash"] {
        assert!(is_outside_system32(Path::new(shell)), "{shell}");
    }
}

fn tools_missing() -> bool {
    if cfg!(unix) {
        assert!(
            bash_program().is_some(),
            "bash must resolve on unix; a silent skip would hide the harness"
        );
        assert!(
            which::which("jq").is_ok(),
            "jq must be on PATH on unix; a silent skip would hide the harness"
        );
    }
    let missing = bash_program().is_none() || which::which("jq").is_err();
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

fn bash() -> Command {
    Command::new(bash_program().expect("bash resolved by tools_missing"))
}

fn run(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
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
    let mut cmd = bash();
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
    let mut cmd = bash();
    cmd.arg(asset(rel))
        .env("GRAPH_STATE_FILE", &state_file)
        .env_remove("GRAPH_STATE")
        .env_remove("COYOTE_GATE_OUTPUT_MAX_BYTES")
        .env_remove("BUILD_CMD")
        .env_remove("TEST_CMD")
        .env_remove("FORMAT_CMD")
        .env_remove("LINT_CMD")
        .env_remove("LLM_AGENT_VAR_BUILD_LOCK");
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

/// A string field the script captured with `$(jq -r ...)`. On Windows the
/// native jq.exe writes its text-mode output with CRLF, so a multi-line
/// value picks up a CR per line on the way through the script; the fold
/// keeps the byte-equality assertions about the text itself.
fn captured_field(out: &Value, key: &str) -> String {
    let text = str_field(out, key);
    if cfg!(windows) {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
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
        // A green test run routes through the deterministic debt_scan node, which
        // then falls through to edge_case_sweep.
        success_next: "debt_scan",
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

/// The `build_lock` graph variable wraps every gate command as ONE unit. Each
/// command reads `$GATE_SEEN` in a child shell so only the prefix can set it.
/// The compound leg proves the second `&&` half is also under the prefix, and
/// the env-led leg uses a real wrapper prefix so a leading `FOO=bar` assignment
/// survives (a word-prefix splice would exec `FOO=bar` as the program). The
/// empty-string leg mirrors the production default of `build_lock: ''`. The
/// `Ran:` line names the effective command, prefix included.
#[test]
fn verify_scripts_run_gate_under_build_lock_prefix() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-build-lock");
    let state = project_state(&dir.0);
    let simple = r#"sh -c 'printf "seen=%s\n" "${GATE_SEEN:-unset}"'"#;
    let compound = r#"sh -c 'printf "a=%s\n" "${GATE_SEEN:-unset}"' && sh -c 'printf "b=%s\n" "${GATE_SEEN:-unset}"'"#;
    let env_led = r#"FOO=bar sh -c 'printf "seen=%s foo=%s\n" "${GATE_SEEN:-unset}" "$FOO"'"#;
    let wrapper = r#"sh -c 'GATE_SEEN=1 exec "$0" "$@"'"#;
    let legs: [(&str, Option<&str>, &[&str]); 5] = [
        (
            simple,
            Some("env GATE_SEEN=1"),
            &["seen=1", "Ran: env GATE_SEEN=1 sh -c"],
        ),
        (compound, Some("env GATE_SEEN=1"), &["a=1", "b=1"]),
        (env_led, Some(wrapper), &["seen=1 foo=bar"]),
        (simple, Some(""), &["seen=unset", "Ran: sh -c"]),
        (simple, None, &["seen=unset", "Ran: sh -c"]),
    ];
    for (cmd, lock, expected) in legs {
        let lock_env: Vec<(&str, &str)> = lock
            .into_iter()
            .map(|v| ("LLM_AGENT_VAR_BUILD_LOCK", v))
            .collect();
        for script in &VERIFY_SCRIPTS {
            let mut envs = vec![(script.cmd_env, cmd)];
            envs.extend_from_slice(&lock_env);
            let out = run_script_json(script.rel, &dir.0, &state, &envs);
            assert_eq!(
                out[script.ok_key],
                Value::Bool(true),
                "{} under {lock:?}: {out}",
                script.rel
            );
            let output = str_field(&out, script.output_key);
            for needle in expected {
                assert!(
                    output.contains(needle),
                    "{} under {lock:?}: expected {needle} in {output}",
                    script.rel
                );
            }
        }
        let mut envs = vec![("FORMAT_CMD", cmd), ("LINT_CMD", cmd)];
        envs.extend_from_slice(&lock_env);
        let out = run_script_json(
            "step-runner/scripts/verify_format_lint.sh",
            &dir.0,
            &state,
            &envs,
        );
        assert_eq!(out["lint_ok"], Value::Bool(true), "under {lock:?}: {out}");
        for key in ["format_output", "lint_output"] {
            let output = str_field(&out, key);
            for needle in expected {
                assert!(
                    output.contains(needle),
                    "{key} under {lock:?}: expected {needle} in {output}"
                );
            }
        }
    }
}

/// A prefixed gate runs in a child shell; it must still fail the way the bare
/// gate does: the command's exit status reaches the verdict, and a pipeline
/// whose left side fails is a failure (`pipefail`), not a pass through `cat`.
/// The bare legs pin the other arm of run_gate to the same verdict. The
/// unbound-variable legs expand in the gate shell itself (a `sh -c '...'`
/// wrapper would hide the reference from `set -u`); the exit code differs
/// between the arms (1 bare, 127 prefixed), so only the verdict is pinned.
#[test]
fn verify_scripts_under_build_lock_keep_exit_status_and_pipefail() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("verify-build-lock-fail");
    let state = project_state(&dir.0);
    let prefixed = Some("env GATE_SEEN=1");
    let unbound = r#"echo "$UNSET_GATE_VAR""#;
    let legs: [(Option<&str>, &str, Option<&str>); 6] = [
        (prefixed, "sh -c 'exit 7'", Some("Exit code: 7")),
        (prefixed, "sh -c 'exit 7' | cat", Some("Exit code: 7")),
        (None, "sh -c 'exit 7'", Some("Exit code: 7")),
        (None, "sh -c 'exit 7' | cat", Some("Exit code: 7")),
        (prefixed, unbound, None),
        (None, unbound, None),
    ];
    for (lock, cmd, exit_line) in legs {
        let lock_env: Vec<(&str, &str)> = lock
            .into_iter()
            .map(|v| ("LLM_AGENT_VAR_BUILD_LOCK", v))
            .collect();
        for script in &VERIFY_SCRIPTS {
            let mut envs = vec![(script.cmd_env, cmd)];
            envs.extend_from_slice(&lock_env);
            let out = run_script_json(script.rel, &dir.0, &state, &envs);
            assert_eq!(
                out[script.ok_key],
                Value::Bool(false),
                "{} running {cmd} under {lock:?}: {out}",
                script.rel
            );
            assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{}", script.rel);
            let output = str_field(&out, script.output_key);
            if let Some(needle) = exit_line {
                assert!(
                    output.contains(needle),
                    "{} running {cmd} under {lock:?}: {output}",
                    script.rel
                );
            }
        }
        let mut envs = vec![("FORMAT_CMD", cmd), ("LINT_CMD", cmd)];
        envs.extend_from_slice(&lock_env);
        let out = run_script_json(
            "step-runner/scripts/verify_format_lint.sh",
            &dir.0,
            &state,
            &envs,
        );
        assert_eq!(
            out["lint_ok"],
            Value::Bool(false),
            "running {cmd} under {lock:?}: {out}"
        );
        if let Some(needle) = exit_line {
            for key in ["format_output", "lint_output"] {
                let output = str_field(&out, key);
                assert!(
                    output.contains(needle),
                    "{key} running {cmd} under {lock:?}: {output}"
                );
            }
        }
    }
}

/// Every graph that forwards or consumes `build_lock` declares it with an
/// empty default, so an unset variable renders as `''` and the gate runs bare.
/// The Sisyphus gauntlet template must ask for bare lane names in `Settled
/// lanes:` because build_items.py refuses verdict words and parentheticals,
/// and say the tokens are inert without `Re-review delta:`, which is how
/// build_items.py treats them.
#[test]
fn build_lock_is_declared_with_empty_default_and_settled_lanes_are_bare() {
    for rel in [
        "coder/graph.yaml",
        "step-runner/graph.yaml",
        "sisyphus/config.yaml",
        "architect/config.yaml",
        "review-gauntlet/graph.yaml",
        "adversary/graph.yaml",
    ] {
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(asset(rel)).unwrap()).unwrap();
        let variables = doc["variables"]
            .as_sequence()
            .unwrap_or_else(|| panic!("{rel}: variables"));
        let build_lock = variables
            .iter()
            .find(|v| v["name"].as_str() == Some("build_lock"))
            .unwrap_or_else(|| panic!("{rel}: no build_lock variable"));
        assert_eq!(
            build_lock["default"],
            serde_yaml::Value::from(""),
            "{rel}: build_lock default"
        );
    }

    let sisyphus = fs::read_to_string(asset("sisyphus/instructions.md")).unwrap();
    let settled = sisyphus
        .lines()
        .find(|l| l.starts_with("Settled lanes:"))
        .expect("sisyphus gauntlet template has a `Settled lanes:` placeholder");
    assert!(settled.contains("bare lane names"), "{settled}");
    assert!(
        sisyphus
            .contains("omit on the first round (ignored unless `Re-review delta:` is also given)>"),
        "the Settled lanes placeholder must say the tokens are inert without a delta"
    );
}

fn declared_variables(rel: &str) -> Vec<serde_yaml::Value> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(asset(rel)).unwrap()).unwrap();
    doc["variables"]
        .as_sequence()
        .unwrap_or_else(|| panic!("{rel}: variables"))
        .clone()
}

fn declared<'a>(
    variables: &'a [serde_yaml::Value],
    rel: &str,
    name: &str,
) -> &'a serde_yaml::Value {
    variables
        .iter()
        .find(|v| v["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("{rel}: no {name} variable"))
}

/// The architect's `task_granularity` and the Sisyphus `review_cadence` are a
/// pair: micro granularity spawns Sisyphus with `review_cadence: batch`
/// beside `build_lock`, so both configs declare both halves with their
/// standalone defaults, and the architect's forward literal names a variable
/// Sisyphus actually declares.
#[test]
fn micro_granularity_and_batch_cadence_are_declared_as_a_pair() {
    let architect = declared_variables("architect/config.yaml");
    assert_eq!(
        declared(&architect, "architect/config.yaml", "task_granularity")["default"],
        serde_yaml::Value::from("standard")
    );
    declared(&architect, "architect/config.yaml", "build_lock");

    let sisyphus = declared_variables("sisyphus/config.yaml");
    assert_eq!(
        declared(&sisyphus, "sisyphus/config.yaml", "review_cadence")["default"],
        serde_yaml::Value::from("task")
    );
    declared(&sisyphus, "sisyphus/config.yaml", "build_lock");

    let forwarded = r#""review_cadence": "batch""#;
    let instructions = fs::read_to_string(asset("architect/instructions.md")).unwrap();
    assert!(
        instructions.contains(forwarded),
        "architect/instructions.md must forward {forwarded}"
    );
    let forwarded_name = forwarded.split('"').nth(1).unwrap();
    declared(&sisyphus, "sisyphus/config.yaml", forwarded_name);
}

/// Rendering the Sisyphus instructions under `review_cadence=batch` drops the
/// gauntlet section and keeps the batch section; the `debris-exempt` rule the
/// batch section leans on lives outside both blocks, so either cadence
/// renders it.
#[test]
fn sisyphus_cadence_blocks_render_exclusively_and_share_the_debris_rule() {
    if tools_missing() {
        return;
    }
    let hoisted = "a `Debris exemptions:` heading (`<file:line> -- <reason>`)";
    let script = format!(
        "source '{}'; render_instructions '{}'",
        asset(".shared/utils.sh").display(),
        asset("sisyphus/instructions.md").display()
    );
    for (cadence, present, absent) in [
        (
            "batch",
            "### Review cadence: batch",
            "### The review gauntlet",
        ),
        (
            "task",
            "### The review gauntlet",
            "### Review cadence: batch",
        ),
    ] {
        let (code, stdout, stderr) = run(bash()
            .arg("-c")
            .arg(&script)
            .env("LLM_AGENT_VAR_REVIEW_CADENCE", cadence)
            .output()
            .expect("spawn bash"));
        assert_eq!(code, 0, "render under {cadence}: {stderr}");
        assert!(stdout.contains(present), "{cadence}: missing {present:?}");
        assert!(!stdout.contains(absent), "{cadence}: leaked {absent:?}");
        assert!(stdout.contains(hoisted), "{cadence}: missing {hoisted:?}");
    }
}

/// The gauntlet hands `build_lock` to the adversary lane through `inputs:`
/// (the same structured channel as `verification_commands`), and every
/// spawn template that forwards the variable spells it the same way, so an
/// empty value travels too rather than being dropped by a conditional.
#[test]
fn build_lock_forwarding_is_unconditional_in_templates_and_gauntlet_inputs() {
    let gauntlet: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(asset("review-gauntlet/graph.yaml")).unwrap())
            .unwrap();
    let inputs = &gauntlet["nodes"]["run_adversary"]["inputs"];
    assert_eq!(
        inputs["verification_commands"],
        serde_yaml::Value::from("{{verification_commands}}")
    );
    assert_eq!(
        inputs["build_lock"],
        serde_yaml::Value::from("{{build_lock}}")
    );

    let forwarded = r#""build_lock": "{{build_lock}}""#;
    for rel in ["sisyphus/instructions.md", "architect/instructions.md"] {
        let doc = fs::read_to_string(asset(rel)).unwrap();
        let count = doc.matches(forwarded).count();
        assert_eq!(
            count, 4,
            "{rel}: {forwarded} appears {count} times, expected exactly 4"
        );
        assert!(
            !doc.contains("When `{{build_lock}}` is non-empty"),
            "{rel}: build_lock forwarding must not be conditional"
        );
    }
}

/// The tool-shaped build/test runners share the gate helper with the script
/// nodes, so a `build_lock` prefix covers them as well.
#[test]
fn tools_sh_build_and_test_runners_go_through_run_gate() {
    for rel in ["coder/tools.sh", "sisyphus/tools.sh"] {
        let script = fs::read_to_string(asset(rel)).unwrap();
        for var in ["build_cmd", "test_cmd"] {
            assert!(
                !script.contains(&format!("eval \"${{{var}}}\"")),
                "{rel}: {var} must not be eval'd directly"
            );
            assert!(
                script.contains(&format!("run_gate \"${{{var}}}\"")),
                "{rel}: {var} must run through run_gate"
            );
            assert!(
                script.contains(&format!("Running: $(gate_ran \"${{{var}}}\")")),
                "{rel}: the Running: line for {var} must name the effective command"
            );
        }
    }
}

/// The prose that tells a caller how long a lock wait may take quotes the
/// gate nodes' timeouts; this reads the figures from the graphs so the two
/// cannot drift apart silently.
#[test]
fn build_lock_prose_quotes_the_gate_node_timeouts() {
    let cases: [(&str, &str, &[&str]); 2] = [
        (
            "coder/graph.yaml",
            "coder/README.md",
            &["verify_build", "verify_tests"],
        ),
        (
            "step-runner/graph.yaml",
            "step-runner/README.md",
            &["verify_format_lint", "verify_build", "verify_tests"],
        ),
    ];
    for (graph_rel, readme_rel, nodes) in cases {
        let graph: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(asset(graph_rel)).unwrap()).unwrap();
        let description = graph["variables"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|v| v["name"].as_str() == Some("build_lock"))
            .and_then(|v| v["description"].as_str())
            .unwrap_or_else(|| panic!("{graph_rel}: build_lock description"));
        let readme = fs::read_to_string(asset(readme_rel)).unwrap();
        for node in nodes {
            let timeout = graph["nodes"][node]["timeout"]
                .as_u64()
                .unwrap_or_else(|| panic!("{graph_rel}: nodes.{node}.timeout"));
            let figure = format!("{timeout} s");
            assert!(
                description.contains(&figure),
                "{graph_rel}: build_lock description must quote {figure} for {node}"
            );
            assert!(
                readme.contains(&figure),
                "{readme_rel}: must quote {figure} for {node}"
            );
        }
    }
}

/// Every `flock -w <secs>` figure in the build_lock prose is less than the
/// shortest coder or step-runner gate timeout, so a caller who copies the
/// example gets a wait that gives up before the node is killed. The READMEs
/// that show the invocation form and the gauntlet/adversary prose must each
/// carry an example; the latter four show the canonical one.
#[test]
fn build_lock_prose_flock_waits_fit_under_the_shortest_gate_timeout() {
    fn flock_wait_seconds(doc: &str) -> Vec<u64> {
        doc.split("flock -w ")
            .skip(1)
            .filter_map(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().ok()
            })
            .collect()
    }
    let gates: [(&str, &[&str]); 2] = [
        ("coder/graph.yaml", &["verify_build", "verify_tests"]),
        (
            "step-runner/graph.yaml",
            &["verify_format_lint", "verify_build", "verify_tests"],
        ),
    ];
    let shortest = gates
        .iter()
        .flat_map(|(graph_rel, nodes)| {
            let graph: serde_yaml::Value =
                serde_yaml::from_str(&fs::read_to_string(asset(graph_rel)).unwrap()).unwrap();
            nodes
                .iter()
                .map(|node| {
                    graph["nodes"][node]["timeout"]
                        .as_u64()
                        .unwrap_or_else(|| panic!("{graph_rel}: nodes.{node}.timeout"))
                })
                .collect::<Vec<_>>()
        })
        .min()
        .unwrap();
    let must_show_one = [
        "coder/README.md",
        "step-runner/README.md",
        "adversary/graph.yaml",
        "adversary/scripts/run_checks.py",
        "review-gauntlet/graph.yaml",
        "review-gauntlet/README.md",
    ];
    let canonical_example = "flock -w 240 /path/to/lock";
    let must_show_canonical = [
        "adversary/graph.yaml",
        "adversary/scripts/run_checks.py",
        "review-gauntlet/graph.yaml",
        "review-gauntlet/README.md",
    ];
    for rel in [
        "adversary/graph.yaml",
        "adversary/scripts/run_checks.py",
        "adversary/README.md",
        "review-gauntlet/graph.yaml",
        "review-gauntlet/README.md",
        "coder/graph.yaml",
        "coder/README.md",
        "step-runner/graph.yaml",
        "step-runner/README.md",
        "sisyphus/config.yaml",
        "architect/config.yaml",
        "architect/README.md",
    ] {
        let doc = fs::read_to_string(asset(rel)).unwrap();
        let waits = flock_wait_seconds(&doc);
        if must_show_one.contains(&rel) {
            assert!(
                !waits.is_empty(),
                "{rel}: must show a `flock -w <secs>` example"
            );
        }
        if must_show_canonical.contains(&rel) {
            assert!(
                doc.contains(canonical_example),
                "{rel}: must show the canonical example {canonical_example:?}"
            );
        }
        for wait in waits {
            assert!(
                wait < shortest,
                "{rel}: flock -w {wait} is not below the shortest gate timeout {shortest}"
            );
        }
    }
}

/// Where a caller learns about `build_lock`: the adversary and gauntlet
/// READMEs introduce it beside `verification_commands` (the variable it
/// travels with), and the architect README lists it in the variables table.
#[test]
fn build_lock_is_documented_beside_verification_commands() {
    for rel in ["adversary/README.md", "review-gauntlet/README.md"] {
        let doc = fs::read_to_string(asset(rel)).unwrap();
        let paragraph = doc
            .split("\n\n")
            .find(|p| p.contains("`build_lock`"))
            .unwrap_or_else(|| panic!("{rel}: no `build_lock` mention"));
        assert!(
            paragraph.contains("`verification_commands`"),
            "{rel}: `build_lock` must be introduced next to `verification_commands`"
        );
    }
    let architect = fs::read_to_string(asset("architect/README.md")).unwrap();
    assert!(
        architect.contains("| `build_lock` |"),
        "architect/README.md: variables table needs a build_lock row"
    );
    let row = architect
        .lines()
        .find(|l| l.starts_with("| `build_lock` |"))
        .unwrap();
    assert!(
        row.contains("double quotes"),
        "architect/README.md: build_lock row must state the JSON-quoting constraint"
    );
}

/// The graph descriptions and READMEs for the coder and step-runner are
/// twins: each must warn that the prefix is re-parsed by the shell and that
/// a timed-out gate can leave an orphan lock wait behind. The step-runner
/// README shows the invocation form, and the orchestrator configs state the
/// JSON-quoting constraint their `--variables` hop imposes.
#[test]
fn build_lock_prose_twins_carry_the_same_warnings() {
    for rel in [
        "coder/graph.yaml",
        "coder/README.md",
        "step-runner/graph.yaml",
        "step-runner/README.md",
    ] {
        let doc = fs::read_to_string(asset(rel)).unwrap();
        for needle in [
            "re-parsed",
            "running later unobserved",
            "flock '/tmp/my lock'",
        ] {
            assert!(doc.contains(needle), "{rel}: missing {needle:?}");
        }
        assert!(
            !doc.contains(r#"flock "/tmp/my lock""#),
            "{rel}: path examples must use single quotes"
        );
    }
    let step_runner = fs::read_to_string(asset("step-runner/README.md")).unwrap();
    assert!(step_runner.contains("--agent-variable build_lock"));
    assert!(
        step_runner.contains("spawned coder's own gates inherit the prefix"),
        "step-runner/README.md: must say the spawned coder's gates inherit the prefix"
    );

    // The inner-shell caveat is spelled in the helper and both READMEs.
    let exported_only = "sees only exported variables";
    for rel in [
        ".shared/utils.sh",
        "coder/README.md",
        "step-runner/README.md",
    ] {
        let doc = fs::read_to_string(asset(rel)).unwrap();
        assert!(
            doc.contains(exported_only),
            "{rel}: missing {exported_only:?}"
        );
    }

    // pipefail and the POSIX limit of the prefixed arm, stated at both
    // places a caller reads about run_checks.
    for rel in ["adversary/scripts/run_checks.py", "adversary/README.md"] {
        let doc = fs::read_to_string(asset(rel)).unwrap();
        for needle in ["pipefail applies only", "POSIX-only"] {
            assert!(doc.contains(needle), "{rel}: missing {needle:?}");
        }
    }

    for rel in ["sisyphus/config.yaml", "architect/config.yaml"] {
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(asset(rel)).unwrap()).unwrap();
        let description = doc["variables"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|v| v["name"].as_str() == Some("build_lock"))
            .and_then(|v| v["description"].as_str())
            .unwrap_or_else(|| panic!("{rel}: build_lock description"));
        assert!(
            description.contains("double quotes"),
            "{rel}: build_lock description must state the JSON-quoting constraint"
        );
        assert!(
            description.contains("review-gauntlet/adversary verification runs"),
            "{rel}: build_lock description must name the review lanes' verification runs"
        );
    }
    let architect = fs::read_to_string(asset("architect/README.md")).unwrap();
    assert!(
        architect.contains("review-gauntlet/adversary verification runs"),
        "architect/README.md: build_lock row must name the same reach as the config twins"
    );
}

/// The engineer's three surfaces agree: the README usage block spawns it
/// with `--agent-variable` (never the orchestrators' `--variables` hop), the
/// config grants `fs_cat.sh`, and every fs tool the instructions call "your
/// own" is a granted global tool.
#[test]
fn engineer_agent_surface_is_consistent() {
    let readme = fs::read_to_string(asset("engineer/README.md")).unwrap();
    let usage = readme
        .split("## Usage")
        .nth(1)
        .and_then(|rest| rest.split("```").nth(1))
        .expect("engineer/README.md: fenced block under ## Usage");
    for needle in [
        "--agent-variable project_dir",
        "--agent-variable reference_root",
    ] {
        assert!(
            usage.contains(needle),
            "engineer/README.md usage: missing {needle:?}"
        );
    }
    for stale in ["--variable ", "--variables"] {
        assert!(
            !readme.contains(stale),
            "engineer/README.md: {stale:?} is the orchestrators' flag, not a config agent's"
        );
    }

    let doc: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(asset("engineer/config.yaml")).unwrap()).unwrap();
    let global_tools: Vec<String> = doc["global_tools"]
        .as_sequence()
        .expect("engineer/config.yaml: global_tools")
        .iter()
        .map(|t| {
            t.as_str()
                .expect("global_tools entry is a string")
                .to_string()
        })
        .collect();
    assert!(
        global_tools.iter().any(|t| t == "fs_cat.sh"),
        "engineer/config.yaml: global_tools must grant fs_cat.sh: {global_tools:?}"
    );

    let instructions = fs::read_to_string(asset("engineer/instructions.md")).unwrap();
    // Indented lines continue the list item above them, so the tool list
    // reads the same whichever name the line wrap lands on.
    let mut unwrapped = String::with_capacity(instructions.len());
    for line in instructions.lines() {
        if line.starts_with(' ') {
            unwrapped.push_str(line.trim_start());
        } else {
            unwrapped.push('\n');
            unwrapped.push_str(line);
        }
    }
    let lead = "own fs tools (";
    let listed = unwrapped
        .split_once(lead)
        .and_then(|(_, rest)| rest.split_once(" - "))
        .map(|(names, _)| names)
        .unwrap_or_else(|| panic!("engineer/instructions.md: no `{lead}<a/b/c> - ...` sentence"));
    let names: Vec<&str> = listed.split('/').collect();
    assert!(
        names.len() >= 2,
        "engineer/instructions.md: own fs tools list {listed:?}"
    );
    for name in names {
        let tool = format!("{name}.sh");
        assert!(
            global_tools.contains(&tool),
            "engineer/instructions.md names {name} as an own fs tool but config.yaml does not grant {tool}: {global_tools:?}"
        );
    }
}

/// run_checks.py wraps each declared command in the `build_lock` prefix and
/// records the prefix as `under`; without one (absent or empty) the record is
/// unchanged. A failing command's exit status survives the wrapper. The
/// prefix is read from state only, so an inherited LLM_AGENT_VAR_BUILD_LOCK
/// must not leak into a bare run. A present non-string value is a declaration
/// error: nothing runs and the record is the PIPELINE-FAULT marker.
#[cfg(unix)]
#[test]
fn adversary_run_checks_runs_declared_commands_under_build_lock() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fixture_dir("run-checks-build-lock");
    let script = asset("adversary/scripts/run_checks.py");
    let declared = r#"["sh -c 'printf seen=%s \"${GATE_SEEN:-unset}\"'", "sh -c 'exit 3'"]"#;
    let declared_cmds = serde_json::from_str::<Vec<String>>(declared).unwrap();
    let legs: [(Option<&str>, &str); 3] = [
        (Some("env GATE_SEEN=1"), "seen=1"),
        (Some(""), "seen=unset"),
        (None, "seen=unset"),
    ];
    for (lock, expected) in legs {
        let mut state = serde_json::json!({
            "project_dir": dir.0.to_string_lossy(),
            "verification_commands": declared,
        });
        if let Some(prefix) = lock {
            state["build_lock"] = Value::String(prefix.to_string());
        }
        let state_file = dir.0.join(format!(
            "state-{}.json",
            lock.map_or("absent", |p| if p.is_empty() { "empty" } else { "set" })
        ));
        fs::write(&state_file, state.to_string()).unwrap();
        let out = Command::new("python3")
            .arg(&script)
            .env("GRAPH_STATE_FILE", &state_file)
            .env("ADVERSARY_STACK_LOCK_PATH", dir.0.join("stack.lock"))
            .env_remove("GRAPH_STATE")
            .env("LLM_AGENT_VAR_BUILD_LOCK", "env GATE_SEEN=leaked")
            .output()
            .expect("spawn python3");
        let (code, stdout, stderr) = run(out);
        assert_eq!(
            code, 0,
            "run_checks.py under {lock:?}\nstdout: {stdout}\nstderr: {stderr}"
        );
        let out: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}\nstderr: {stderr}"));
        let results = out["exec_results"]
            .as_array()
            .unwrap_or_else(|| panic!("exec_results must be a record list: {out}"));
        assert_eq!(results.len(), declared_cmds.len(), "{out}");
        for (record, declared_cmd) in results.iter().zip(&declared_cmds) {
            assert_eq!(
                record["cmd"].as_str(),
                Some(declared_cmd.as_str()),
                "{record}"
            );
            match lock {
                Some(prefix) if !prefix.is_empty() => {
                    assert_eq!(record["under"], Value::String(prefix.to_string()))
                }
                _ => assert!(
                    record.get("under").is_none(),
                    "a bare run must not carry `under`: {record}"
                ),
            }
        }
        let (seen, failing) = (&results[0], &results[1]);
        assert_eq!(seen["exit"], 0, "{seen}");
        assert_eq!(str_field(seen, "tail"), expected, "{seen}");
        assert_eq!(failing["exit"], 3, "{failing}");
    }

    let state = serde_json::json!({
        "project_dir": dir.0.to_string_lossy(),
        "verification_commands": declared,
        "build_lock": 7,
    });
    let state_file = dir.0.join("state-non-string.json");
    fs::write(&state_file, state.to_string()).unwrap();
    let out = Command::new("python3")
        .arg(&script)
        .env("GRAPH_STATE_FILE", &state_file)
        .env("ADVERSARY_STACK_LOCK_PATH", dir.0.join("stack.lock"))
        .env_remove("GRAPH_STATE")
        .output()
        .expect("spawn python3");
    let (code, stdout, stderr) = run(out);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let out: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}\nstderr: {stderr}"));
    let marker = str_field(&out, "exec_results");
    assert!(
        marker.starts_with("PIPELINE-FAULT: build_lock declaration invalid"),
        "a non-string build_lock must fail closed, not run bare: {out}"
    );
    assert!(marker.contains("got int"), "{marker}");
    assert_eq!(out["pipeline_faults"], serde_json::json!([marker]));
}

/// verdict.py names the prefix a verification command ran under, so a reader
/// of the report can tell a locked run from a bare one; a record without
/// `under` renders as before.
#[cfg(unix)]
#[test]
fn adversary_verdict_names_the_prefix_a_verification_run_was_under() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fixture_dir("verdict-under");
    let state = serde_json::json!({
        "pipeline_faults": [],
        "crit_verdicts": [
            {"id": "c1", "text": "tests pass", "status": "MET",
             "evidence": "src/x.rs:1 + test src/x.rs:99", "complaint": ""}
        ],
        "extra_complaints": [],
        "observations": "",
        "exec_results": [
            {"cmd": "cargo test", "exit": 0, "duration_s": 1.5, "tail": "",
             "under": "flock -w 240 /tmp/lock"},
            {"cmd": "cargo build", "exit": 0, "duration_s": 0.5, "tail": ""}
        ]
    });
    let state_file = dir.0.join("state.json");
    fs::write(&state_file, state.to_string()).unwrap();
    let out = Command::new("python3")
        .arg(asset("adversary/scripts/verdict.py"))
        .env("GRAPH_STATE_FILE", &state_file)
        .env("ADVERSARY_STACK_LOCK_PATH", dir.0.join("stack.lock"))
        .env_remove("GRAPH_STATE")
        .output()
        .expect("spawn python3");
    let (code, stdout, stderr) = run(out);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    let out: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON ({e}): {stdout}\nstderr: {stderr}"));
    let report = str_field(&out, "adv_report");
    let run_line = |cmd: &str| {
        report
            .lines()
            .find(|l| l.contains(&format!("`{cmd}`")))
            .unwrap_or_else(|| panic!("no run line for {cmd}: {report}"))
    };
    assert!(
        run_line("cargo test").ends_with("1.5s (under: flock -w 240 /tmp/lock)"),
        "{report}"
    );
    assert!(
        run_line("cargo build").ends_with("0.5s"),
        "a bare record must not gain an `under` suffix: {report}"
    );
}

/// A plan with two complete tasks and one pending task; the first complete
/// task's log holds `completed` SHAs and the ledger block `log` under its
/// entry, the second names the plan without its `.md` (the spelling most task
/// files in this repo use) and has no ledger lines.
#[cfg(unix)]
fn pr_body_fixture(label: &str, log: &str) -> TempDirGuard {
    let dir = fixture_dir(label);
    fs::write(
        dir.0.join("PLAN-x.md"),
        "---\nrigor: prototype\n---\n# Plan\n",
    )
    .unwrap();
    fs::write(dir.0.join("summary.md"), "Summary prose.\n").unwrap();
    let done = dir.0.join("tasks/TASK-1-a");
    let pending = dir.0.join("tasks/TASK-2-b");
    fs::create_dir_all(&done).unwrap();
    fs::create_dir_all(&pending).unwrap();
    fs::write(
        done.join("index.md"),
        "---\nplan: PLAN-x.md\nstatus: complete\ntitle: \"A\"\n---\n",
    )
    .unwrap();
    fs::write(
        done.join("log.md"),
        format!("## 2026-10-27 - completed\nCommits abc1234, deadbee1 on feat/x.\n\n{log}"),
    )
    .unwrap();
    fs::write(
        pending.join("index.md"),
        "---\nplan: PLAN-x.md\nstatus: pending\ntitle: \"B\"\n---\n",
    )
    .unwrap();
    let bare = dir.0.join("tasks/TASK-3-c");
    fs::create_dir_all(&bare).unwrap();
    fs::write(
        bare.join("index.md"),
        "---\nplan: PLAN-x\nstatus: complete\ntitle: \"C\"\n---\n",
    )
    .unwrap();
    fs::write(
        bare.join("log.md"),
        "## 2026-10-27 - completed\nCommit 0123abc on feat/x.\n",
    )
    .unwrap();
    dir
}

#[cfg(unix)]
fn assemble_pr_body_with(
    dir: &Path,
    plan: &str,
    extra_args: &[&str],
    envs: &[(&str, &str)],
) -> (i32, String, String) {
    run(Command::new("python3")
        .arg(asset("architect/assemble_pr_body.py"))
        .arg("--plan")
        .arg(dir.join(plan))
        .arg("--tasks-dir")
        .arg(dir.join("tasks"))
        .arg("--summary-file")
        .arg(dir.join("summary.md"))
        .arg("--followups")
        .args(extra_args)
        .envs(envs.iter().copied())
        .output()
        .expect("spawn python3"))
}

#[cfg(unix)]
fn assemble_pr_body_with_env(dir: &Path, envs: &[(&str, &str)]) -> (i32, String, String) {
    assemble_pr_body_with(dir, "PLAN-x.md", &[], envs)
}

#[cfg(unix)]
fn assemble_pr_body(dir: &Path) -> (i32, String, String) {
    assemble_pr_body_with_env(dir, &[])
}

/// assemble_pr_body.py reads a tag line together with the block under it
/// (bullets up to a blank line, heading, column-0 prose, or next entry tag),
/// one entry per bullet with an indented wrap joined onto its bullet, and a
/// `(deferred by quality bar)` qualifier never cuts an entry short; a wrapped
/// prose paragraph after a mid-line tag is not a run of entries; a tag line
/// with nothing on it or under it is a NOTE, not a silent `None.`. A task
/// whose `plan:` omits the `.md` belongs to the plan all the same.
#[cfg(unix)]
#[test]
fn assemble_pr_body_reads_ledger_blocks_whole() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = pr_body_fixture(
        "assemble-pr-body",
        "Follow-ups:\n\
         - first item (deferred by quality bar)\n\
         - second item (deferred by quality bar) and third item\n  \
           (deferred by quality bar)\n\
         rejected-finding: finding one - evidence src/x.rs:3\n\n\
         Some narrative. rejected-finding: finding two - evidence src/y.rs:9\n\
         which continues the narrative and is not a finding.\n\n",
    );
    let (code, stdout, stderr) = assemble_pr_body(&dir.0);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("**Quality bar:** prototype"), "{stdout}");
    assert!(stdout.contains("## Tasks\n- [x] TASK-1 - A ("), "{stdout}");
    assert!(
        stdout.contains("`abc1234`") && stdout.contains("`deadbee1`"),
        "{stdout}"
    );
    assert!(!stdout.contains("TASK-2"), "{stdout}");
    assert!(
        stdout.contains("\n- [x] TASK-3 - C (`0123abc`)"),
        "a task whose `plan:` has no `.md` must still be listed: {stdout}"
    );
    assert!(!stderr.contains("checklist left empty"), "{stderr}");
    let decisions: Vec<&str> = stdout
        .lines()
        .skip_while(|l| *l != "## Review decisions")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .collect();
    assert_eq!(
        decisions,
        [
            "- (TASK-1) finding one - evidence src/x.rs:3",
            "- (TASK-1) finding two - evidence src/y.rs:9",
        ],
        "{stdout}"
    );
    assert!(!stdout.contains("continues the narrative"), "{stdout}");
    let followups: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("- [ ] (TASK-1) "))
        .collect();
    assert_eq!(
        followups,
        [
            "- [ ] (TASK-1) first item (deferred by quality bar)",
            "- [ ] (TASK-1) second item (deferred by quality bar) and third item (deferred by quality bar)",
        ],
        "{stdout}"
    );
    assert!(
        stderr.contains("NOTE") && stderr.contains("TASK-2-b (status: pending)"),
        "{stderr}"
    );

    let dir = pr_body_fixture("assemble-pr-body-empty-tag", "Follow-ups:\n");
    let (code, stdout, stderr) = assemble_pr_body(&dir.0);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("## Follow-up / manual actions\nNone.\n"),
        "{stdout}"
    );
    assert!(
        stderr.contains("NOTE TASK-1: Follow-ups: tag with nothing under it"),
        "{stderr}"
    );
}

/// The body is UTF-8 whatever the host locale: a ledger line carrying a
/// severity glyph reaches stdout intact under a C locale with a cp1252
/// console codec (UTF-8 mode off) instead of raising on the write.
#[cfg(unix)]
#[test]
fn assemble_pr_body_writes_utf8_whatever_the_locale() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = pr_body_fixture(
        "assemble-pr-body-locale",
        "rejected-finding: \u{1f7e1} finding one - evidence src/x.rs:3\n",
    );
    let (code, stdout, stderr) = assemble_pr_body_with_env(
        &dir.0,
        &[
            ("LC_ALL", "C"),
            ("LANG", "C"),
            ("PYTHONIOENCODING", "cp1252"),
            ("PYTHONUTF8", "0"),
        ],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("- (TASK-1) \u{1f7e1} finding one - evidence src/x.rs:3"),
        "the glyph must reach the body verbatim: {stdout}\nstderr: {stderr}"
    );
    assert!(!stderr.contains("Traceback"), "{stderr}");
}

/// `--tasks` scopes the body to a PR group's members: the checklist and the
/// ledger rollups hold the listed ids alone, and a listed id that is not
/// under the plan is a NOTE, never a silent drop.
#[cfg(unix)]
#[test]
fn assemble_pr_body_tasks_filter_scopes_checklist_and_rollups_to_the_group() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = pr_body_fixture(
        "assemble-pr-body-tasks",
        "rejected-finding: finding one - evidence src/x.rs:3\n\
         Follow-ups:\n\
         - first item\n\n",
    );
    let (code, stdout, stderr) =
        assemble_pr_body_with(&dir.0, "PLAN-x.md", &["--tasks", "TASK-3"], &[]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("\n- [x] TASK-3 - C (`0123abc`)"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("TASK-1") && !stdout.contains("TASK-2"),
        "only the listed group member may appear: {stdout}"
    );
    assert!(
        !stdout.contains("finding one") && !stdout.contains("first item"),
        "an unlisted task's ledger lines must stay out of the rollups: {stdout}"
    );
    assert!(
        stdout.contains("## Follow-up / manual actions\nNone.\n"),
        "{stdout}"
    );
    assert!(!stderr.contains("checklist left empty"), "{stderr}");

    let (code, stdout, stderr) = assemble_pr_body_with(
        &dir.0,
        "PLAN-x.md",
        &["--tasks", "TASK-1,TASK-3,TASK-9"],
        &[],
    );
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("\n- [x] TASK-1 - A ("), "{stdout}");
    assert!(stdout.contains("\n- [x] TASK-3 - C ("), "{stdout}");
    assert!(!stdout.contains("TASK-2"), "{stdout}");
    assert!(
        stdout.contains("- (TASK-1) finding one - evidence src/x.rs:3")
            && stdout.contains("- [ ] (TASK-1) first item"),
        "{stdout}"
    );
    assert!(
        stderr.contains("NOTE listed in --tasks but not found under the plan: TASK-9"),
        "{stderr}"
    );

    let (code, _, stderr) = assemble_pr_body_with(&dir.0, "PLAN-x.md", &["--tasks", "TASK-2"], &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("TASK-2-b (status: pending)") && stderr.contains("checklist left empty"),
        "a listed but incomplete member is a NOTE: {stderr}"
    );

    // The tools.sh wrapper is the model's only way in: it must forward the
    // option, or the directive's `--tasks` never reaches the script.
    if tools_missing() {
        return;
    }
    let script = format!(
        "source '{}'; assemble_pr_body",
        asset("architect/tools.sh").display()
    );
    let (code, stdout, stderr) = run(bash()
        .arg("-c")
        .arg(&script)
        .env(
            "LLM_ROOT_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
        )
        .env("LLM_AGENT_ROOT_DIR", asset("architect"))
        .env("LLM_OUTPUT", "/dev/stdout")
        .env("argc_plan", dir.0.join("PLAN-x.md"))
        .env("argc_tasks_dir", dir.0.join("tasks"))
        .env("argc_summary_file", dir.0.join("summary.md"))
        .env("argc_tasks", "TASK-3")
        .output()
        .expect("spawn bash"));
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("\n- [x] TASK-3 - C (") && !stdout.contains("TASK-1"),
        "the wrapper must forward --tasks: {stdout}"
    );
}

/// Plan identity strips only a trailing `.md`: a dotted slug keeps its dots,
/// so `PLAN-a.b.md` owns a task saying `plan: PLAN-a.b` and not one saying
/// `plan: PLAN-a`.
#[cfg(unix)]
#[test]
fn assemble_pr_body_keeps_the_dots_of_a_dotted_plan_slug() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fixture_dir("assemble-pr-body-dotted");
    fs::write(
        dir.0.join("PLAN-a.b.md"),
        "---\nrigor: production\n---\n# Plan\n",
    )
    .unwrap();
    fs::write(dir.0.join("summary.md"), "Summary prose.\n").unwrap();
    for (name, plan) in [("TASK-1-dotted", "PLAN-a.b"), ("TASK-2-prefix", "PLAN-a")] {
        let task = dir.0.join("tasks").join(name);
        fs::create_dir_all(&task).unwrap();
        fs::write(
            task.join("index.md"),
            format!("---\nplan: {plan}\nstatus: complete\ntitle: \"{name}\"\n---\n"),
        )
        .unwrap();
    }
    let (code, stdout, stderr) = assemble_pr_body_with(&dir.0, "PLAN-a.b.md", &[], &[]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(
        stdout.contains("\n- [x] TASK-1 - TASK-1-dotted ("),
        "{stdout}"
    );
    assert!(
        !stdout.contains("TASK-2"),
        "a task of the prefix plan must not be mistaken for the dotted plan's: {stdout}"
    );
}

/// The architect's PR-body directive renders in both strategies; only the
/// `multi` render adds the `--tasks` group filter, since a single-PR run
/// wants every complete task of the plan.
#[test]
fn architect_pr_body_directive_names_the_tasks_filter_only_under_multi() {
    if tools_missing() {
        return;
    }
    let script = format!(
        "source '{}'; render_instructions '{}'",
        asset(".shared/utils.sh").display(),
        asset("architect/instructions.md").display()
    );
    let directive = "Build the body with `assemble_pr_body --plan";
    let filter = "Pass `--tasks <this group's member task ids, comma-separated>` as well";
    for (strategy, filtered) in [("multi", true), ("single", false)] {
        let (code, stdout, stderr) = run(bash()
            .arg("-c")
            .arg(&script)
            .env("LLM_AGENT_VAR_PR_STRATEGY", strategy)
            .output()
            .expect("spawn bash"));
        assert_eq!(code, 0, "render under {strategy}: {stderr}");
        assert!(
            stdout.contains(directive),
            "{strategy}: missing {directive:?}"
        );
        assert_eq!(
            stdout.contains(filter),
            filtered,
            "{strategy}: the `--tasks` sentence must render only under multi"
        );
        assert_eq!(stdout.contains("--tasks "), filtered, "{strategy}");
    }
}

/// collect_ledger_lines prints the block under each hit indented beneath
/// the `<task> (log.md:NN): <line>` line, stopping at the next entry tag, so
/// a bare `Follow-ups:` line arrives with its bullets and a wrapped bullet
/// keeps its indented wrap line.
#[cfg(unix)]
#[test]
fn collect_ledger_lines_prints_the_block_under_a_hit() {
    if tools_missing() {
        return;
    }
    let dir = pr_body_fixture(
        "collect-ledger-lines",
        "Follow-ups:\n\
         - first item (deferred by quality bar)\n\
         - second item\n  \
           that wraps\n\
         rejected-finding: finding one - evidence src/x.rs:3\n",
    );
    let script = format!(
        "source '{}'; collect_ledger_lines",
        asset("architect/tools.sh").display()
    );
    let (code, stdout, stderr) = run(bash()
        .arg("-c")
        .arg(&script)
        .env(
            "LLM_ROOT_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets"),
        )
        .env("LLM_OUTPUT", "/dev/stdout")
        .env("argc_dir", dir.0.join("tasks"))
        .env("argc_tag", "Follow-ups:")
        .output()
        .expect("spawn bash"));
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stdout,
        [
            "TASK-1-a (log.md:4): Follow-ups:",
            "  - first item (deferred by quality bar)",
            "  - second item",
            "    that wraps",
            "",
        ]
        .join("\n")
    );
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

/// `run()` folds CRLF to LF on both streams so the exact-match assertions
/// above read the same bytes whether the child is a unix bash or Git Bash
/// feeding native jq.exe. Unix children never emit CRLF on their own, so
/// the fold is driven here with an explicit `\r\n` payload; a lone `\r`
/// is not a line ending and must survive.
#[test]
fn run_folds_crlf_to_lf_on_both_streams_and_keeps_a_lone_cr() {
    if tools_missing() {
        return;
    }
    let output = bash()
        .arg("-c")
        .arg("printf 'a\\r\\nb\\rc'; printf 'd\\r\\ne' >&2; exit 3")
        .output()
        .expect("spawn bash");
    let (code, stdout, stderr) = run(output);
    assert_eq!(code, 3);
    assert_eq!(stdout, "a\nb\rc");
    assert_eq!(stderr, "d\ne");
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

/// end_success renders `Design: {{design_notes}}`, and the engine prints a
/// JSON null as the literal word. Both end routes re-emit the self-review's
/// value as a string (verbatim when present), and the loop back to implement
/// clears it so a self_review that then faults cannot ship the previous
/// pass's disclosure (self_review_fault rebuilds it from the trail's ASK
/// lines alone).
#[test]
fn coder_route_review_result_design_notes_is_a_string_on_every_exit() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-review-design-notes");
    let note = "Strategy: src/pay/strategy.rs; call sites src/pay/mod.rs";
    let cases = [
        (
            serde_json::json!({"review_clean": true, "design_notes": null}),
            "end_success",
            "",
        ),
        (
            serde_json::json!({"review_clean": true, "design_notes": note}),
            "end_success",
            note,
        ),
        (
            serde_json::json!({
                "review_clean": false,
                "review_notes": "x.rs:1 - y",
                "review_attempts": 1,
                "design_notes": null,
            }),
            "end_success",
            "",
        ),
        (
            serde_json::json!({
                "review_clean": false,
                "review_notes": "x.rs:1 - y",
                "design_notes": note,
            }),
            "implement",
            "",
        ),
    ];
    for (state, next, design_notes) in cases {
        let out = run_script_json("coder/scripts/route_review_result.sh", &dir.0, &state, &[]);
        assert_eq!(str_field(&out, "_next"), next, "state {state} -> {out}");
        assert_eq!(
            str_field(&out, "design_notes"),
            design_notes,
            "state {state} -> {out}"
        );
    }
}

/// self_review's fallback edge carries no state_updates, so a dead review
/// leaves `design_notes` at its empty seed and an implementer's
/// `Design: ASK -- <question>` would vanish from CODER_COMPLETE. The fault
/// script lifts those lines out of `design_trail` (label stripped, trimmed,
/// de-duplicated, oldest first), marks `review_notes`, and never touches
/// the trail or the implementer's summary.
#[test]
fn coder_self_review_fault_carries_the_trail_ask_lines() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("self-review-fault");
    let one = "\nAdded the strategies.\n  Design: ASK -- fold the two parsers into one trait?  \nDesign: Strategy, src/pay/strategy.rs";
    let two = "\nAdded the strategies.\nDesign: ASK -- fold the two parsers into one trait?\nFixed the test.\nDesign: ASK -- fold the two parsers into one trait?\nDesign: ASK -- keep the codes table in its own module?";
    let none = "\nAdded the strategies.\nDesign: Strategy, src/pay/strategy.rs";
    let cases = [
        (
            serde_json::json!({"design_trail": one, "review_notes": ""}),
            "ASK -- fold the two parsers into one trait?",
            "self-review did not run; ",
        ),
        (
            serde_json::json!({"design_trail": two, "review_notes": "x.rs:1 - y"}),
            "ASK -- fold the two parsers into one trait?\nASK -- keep the codes table in its own module?",
            "self-review did not run; x.rs:1 - y",
        ),
        (
            serde_json::json!({"design_trail": none, "review_notes": null}),
            "",
            "self-review did not run; ",
        ),
        (serde_json::json!({}), "", "self-review did not run; "),
    ];
    for (extra, design_notes, review_notes) in cases {
        let mut state = project_state(&dir.0);
        state["last_node_output"] = Value::String("LLM node 'self_review' failed".to_string());
        for (k, v) in extra.as_object().unwrap() {
            state[k] = v.clone();
        }
        let out = run_script_json("coder/scripts/self_review_fault.sh", &dir.0, &state, &[]);
        assert_eq!(
            str_field(&out, "_next"),
            "end_success",
            "state {state} -> {out}"
        );
        assert_eq!(
            str_field(&out, "design_notes"),
            design_notes,
            "state {state} -> {out}"
        );
        assert_eq!(
            str_field(&out, "review_notes"),
            review_notes,
            "state {state} -> {out}"
        );
        assert!(
            out.get("design_trail").is_none(),
            "the fault script reads the trail but must not rewrite it: {out}"
        );
        assert!(
            out.get("last_node_output").is_none(),
            "the fault script leaves the engine's failure text in place: {out}"
        );
    }
}

/// step-runner end_success renders `{{design_decisions}}` under its own
/// heading, and a null from write_handoff would print as the literal word.
/// check_handoff's success emit re-emits the key as a string; a non-null
/// block passes through verbatim.
#[test]
fn step_runner_check_handoff_success_guards_null_design_decisions() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("check-handoff-design");
    let handoff = dir.0.join("handoff.md");
    fs::write(
        &handoff,
        "---\nresult: complete\n---\n## Summary\n## Completed\n## Not completed\n## Deviations\n## Downstream plan updates\n## Edge cases discovered\n## Evidence\n## Notes for next step\n",
    )
    .unwrap();
    let block = "Strategy: src/pay/strategy.rs; call sites src/pay/mod.rs";
    for (design_decisions, expected) in [(Value::Null, ""), (Value::String(block.into()), block)] {
        let state = serde_json::json!({
            "handoff_path": handoff.to_str().unwrap(),
            "step_plan_path": "",
            "design_decisions": design_decisions,
        });
        let out = run_script_json("step-runner/scripts/check_handoff.sh", &dir.0, &state, &[]);
        assert_eq!(
            str_field(&out, "_next"),
            "gate_user_review",
            "state {state} -> {out}"
        );
        assert_eq!(str_field(&out, "handoff_fix"), "", "state {state} -> {out}");
        assert_eq!(
            str_field(&out, "design_decisions"),
            expected,
            "state {state} -> {out}"
        );
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

/// route_sweep follows the edge-case sweep, whose tolerated script crash is
/// recorded in `last_script_error`; every arm clears it so a later unrelated
/// failure does not render the stale message.
#[test]
fn route_sweep_clears_the_last_script_error_on_every_arm() {
    if tools_missing() {
        return;
    }
    for (needs_review, skip_review, next) in [
        (true, None, "independent_review"),
        (false, None, "write_handoff"),
        (true, Some("1"), "write_handoff"),
    ] {
        let mut cmd = bash();
        cmd.arg(asset("step-runner/scripts/route_sweep.sh"))
            .env(
                "GRAPH_STATE",
                serde_json::json!({ "needs_independent_review": needs_review }).to_string(),
            )
            .env_remove("GRAPH_STATE_FILE")
            .env_remove("STEP_SKIP_REVIEW");
        if let Some(skip) = skip_review {
            cmd.env("STEP_SKIP_REVIEW", skip);
        }
        let (code, stdout, stderr) = run(cmd.output().expect("spawn bash"));
        assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
        let out: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
        assert_eq!(str_field(&out, "_next"), next, "{out}");
        assert_eq!(str_field(&out, "last_script_error"), "", "{out}");
    }
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
/// new override; step-runner's STEP_FAILED bullet names the routing gates
/// and its Fault handling section has the verify_* bullet.
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
    let failed_bullet: String = step
        .lines()
        .skip_while(|l| !l.starts_with("- `STEP_FAILED`"))
        .take_while(|l| l.starts_with("- `STEP_FAILED`") || l.starts_with("  "))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        failed_bullet.contains("`route_staleness`/`route_sweep`/`route_review`")
            && failed_bullet.contains("`Last script error:`"),
        "step-runner README STEP_FAILED bullet must name the routing gates and the \
         `Last script error:` anchor: {failed_bullet}"
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
        assert_eq!(captured_field(&out, "gate_error"), hostile, "{}", g.rel);

        let out = gate(
            g,
            &dir.0,
            serde_json::json!({ "last_script_error": hostile, "gate_retries": 1 }),
        );
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}: {out}", g.rel);
        assert_eq!(
            captured_field(&out, "gate_error"),
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
            assert_eq!(captured_field(&out, "gate_error"), err, "{} {node}", g.rel);
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

// The coder's self_review reads the implementer's summary as
// `{{last_node_output}}` (the `Design:` heading it reconciles into
// `design_notes`), and only the three llm nodes publish that key. Between
// implement and self_review the engine merges each script's JSON key-wise
// into state, so the summary survives exactly when no script emits the key.
// Drive every script on that stretch, on every route it takes, with the
// summary seeded, and require the key absent from each emitted object.
#[test]
fn usage_probe_coder_verify_ladder_scripts_never_emit_the_implementers_summary_key() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("summary-survives");
    let summary = "Added the payment strategies.\nDesign: Strategy, src/pay/strategy.rs; call sites src/pay/mod.rs";
    let seeded = |extra: Value| {
        let mut state = project_state(&dir.0);
        state["last_node_output"] = Value::String(summary.to_string());
        if let Value::Object(map) = extra {
            for (k, v) in map {
                state[k] = v;
            }
        }
        state
    };
    let pass = "true";
    let fail = "false";
    let mut runs: Vec<(String, Value)> = Vec::new();
    for (cmd, label) in [(pass, "pass"), (fail, "fail")] {
        runs.push((
            format!("verify_build.sh/{label}"),
            run_script_json(
                "coder/scripts/verify_build.sh",
                &dir.0,
                &seeded(serde_json::json!({})),
                &[("BUILD_CMD", cmd)],
            ),
        ));
        runs.push((
            format!("verify_tests.sh/{label}"),
            run_script_json(
                "coder/scripts/verify_tests.sh",
                &dir.0,
                &seeded(serde_json::json!({})),
                &[("TEST_CMD", cmd)],
            ),
        ));
    }
    for (label, extra) in [
        (
            "fix_loop_gate.sh/red-run-loops-to-implement",
            serde_json::json!({"tests_ok": false}),
        ),
        (
            "fix_loop_gate.sh/verify-crash-retries",
            serde_json::json!({"last_script_error": VERIFY_CRASH, "tests_ok": false}),
        ),
        ("fix_loop_gate.sh/all-green-fallback", serde_json::json!({})),
        (
            "fix_loop_gate.sh/budget-exhausted",
            serde_json::json!({"tests_ok": false, "fix_attempts": 5}),
        ),
        (
            "route_review_result.sh/clean",
            serde_json::json!({"review_clean": true}),
        ),
        (
            "route_review_result.sh/findings-loop-to-implement",
            serde_json::json!({"review_clean": false, "review_notes": "x.rs:1 - y"}),
        ),
        (
            "route_review_result.sh/budget-exhausted",
            serde_json::json!({"review_clean": false, "review_attempts": 1}),
        ),
    ] {
        let rel = format!("coder/scripts/{}", label.split('/').next().unwrap());
        runs.push((
            label.to_string(),
            run_script_json(&rel, &dir.0, &seeded(extra), &[]),
        ));
    }
    assert_eq!(
        runs.len(),
        11,
        "every route on the implement -> self_review stretch is driven"
    );
    for (label, out) in &runs {
        assert!(
            out.get("last_node_output").is_none(),
            "{label} would overwrite the implementer's summary before self_review reads it: {out}"
        );
    }
}

// The implement node appends every pass's summary to `design_trail`
// (`"{{design_trail}}\n{{output}}"`), the only place a first pass's
// `Design: ASK -- <question>` survives a fix-loop or review-loop re-entry
// for the self_review to carry. The scripts on that stretch merge key-wise
// into state, and route_review_result deliberately resets `design_notes`
// on the loop back, so a reset copied onto the trail would erase the open
// question with no other symptom. Seed the trail and drive every route the
// scripts take: none may emit the key.
#[test]
fn usage_probe_coder_gate_scripts_never_emit_the_design_trail() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trail-survives");
    let trail = "\nAdded the strategies.\nDesign: ASK -- fold the two parsers into one trait?";
    let seeded = |extra: Value| {
        let mut state = project_state(&dir.0);
        state["design_trail"] = Value::String(trail.to_string());
        state["design_notes"] = Value::String("Strategy: src/pay/strategy.rs".to_string());
        if let Value::Object(map) = extra {
            for (k, v) in map {
                state[k] = v;
            }
        }
        state
    };
    let mut runs: Vec<(String, Value)> = Vec::new();
    for (cmd, label) in [("true", "pass"), ("false", "fail")] {
        for script in ["verify_build", "verify_tests"] {
            let env_key = if script == "verify_build" {
                "BUILD_CMD"
            } else {
                "TEST_CMD"
            };
            runs.push((
                format!("{script}.sh/{label}"),
                run_script_json(
                    &format!("coder/scripts/{script}.sh"),
                    &dir.0,
                    &seeded(serde_json::json!({})),
                    &[(env_key, cmd)],
                ),
            ));
        }
    }
    for (label, extra) in [
        (
            "fix_loop_gate.sh/red-run-loops-to-implement",
            serde_json::json!({"tests_ok": false}),
        ),
        (
            "fix_loop_gate.sh/verify-crash-retries",
            serde_json::json!({"last_script_error": VERIFY_CRASH, "tests_ok": false}),
        ),
        ("fix_loop_gate.sh/all-green-fallback", serde_json::json!({})),
        (
            "fix_loop_gate.sh/budget-exhausted",
            serde_json::json!({"tests_ok": false, "fix_attempts": 5}),
        ),
        (
            "route_review_result.sh/clean",
            serde_json::json!({"review_clean": true}),
        ),
        (
            "route_review_result.sh/findings-loop-to-implement",
            serde_json::json!({"review_clean": false, "review_notes": "x.rs:1 - y"}),
        ),
        (
            "route_review_result.sh/budget-exhausted",
            serde_json::json!({"review_clean": false, "review_attempts": 1}),
        ),
    ] {
        let rel = format!("coder/scripts/{}", label.split('/').next().unwrap());
        runs.push((
            label.to_string(),
            run_script_json(&rel, &dir.0, &seeded(extra), &[]),
        ));
    }
    assert_eq!(runs.len(), 11, "every route on the stretch is driven");
    for (label, out) in &runs {
        assert!(
            out.get("design_trail").is_none(),
            "{label} would rewrite the trail of implement passes before self_review reads it: {out}"
        );
    }
    let loop_back = runs
        .iter()
        .find(|(label, _)| label == "route_review_result.sh/findings-loop-to-implement")
        .map(|(_, out)| out)
        .unwrap();
    assert_eq!(
        str_field(loop_back, "design_notes"),
        "",
        "the review loop resets the disclosure (the next self_review rebuilds it from the trail) but, per the assertion above, leaves the trail alone: {loop_back}"
    );
}

// Fold three implement passes through the shipped scripts the way the
// engine merges them: implement writes `last_node_output` and appends to
// `design_trail`; every script's JSON is merged key-wise on top. After a
// verify-red fix loop and a review loop the trail still carries the first
// pass's ASK line, oldest first, while `last_node_output` holds only the
// latest pass and `design_notes` has been cleared for the next review.
#[test]
fn usage_probe_coder_design_trail_keeps_the_first_pass_ask_across_both_loops() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("trail-fold");
    let mut state = project_state(&dir.0);
    state["design_trail"] = Value::String(String::new());
    state["design_notes"] = Value::String(String::new());
    let implement = |state: &mut Value, output: &str| {
        let prior = state["design_trail"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        state["design_trail"] = Value::String(format!("{prior}\n{output}"));
        state["last_node_output"] = Value::String(output.to_string());
    };
    let merge = |state: &mut Value, out: &Value| {
        for (k, v) in out.as_object().expect("scripts emit an object") {
            state[k] = v.clone();
        }
    };
    let first = "Added the strategies.\nDesign: ASK -- fold the two parsers into one trait?";
    let second = "Fixed the failing test.";
    let third = "Renamed per the review.";

    implement(&mut state, first);
    let out = run_script_json(
        "coder/scripts/verify_tests.sh",
        &dir.0,
        &state,
        &[("TEST_CMD", "false")],
    );
    merge(&mut state, &out);
    let out = run_script_json("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    merge(&mut state, &out);

    implement(&mut state, second);
    let out = run_script_json(
        "coder/scripts/verify_tests.sh",
        &dir.0,
        &state,
        &[("TEST_CMD", "true")],
    );
    merge(&mut state, &out);
    state["review_clean"] = Value::Bool(false);
    state["review_notes"] = Value::String("x.rs:1 - y".to_string());
    state["design_notes"] = Value::String("Strategy: src/pay/strategy.rs".to_string());
    let out = run_script_json("coder/scripts/route_review_result.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    merge(&mut state, &out);

    implement(&mut state, third);
    assert_eq!(
        state["design_trail"].as_str().unwrap(),
        format!("\n{first}\n{second}\n{third}"),
        "the trail lists every pass oldest first, the ASK line included"
    );
    assert_eq!(
        state["last_node_output"].as_str().unwrap(),
        third,
        "the summary the self_review reads first is the latest pass alone"
    );
    assert_eq!(
        state["design_notes"].as_str().unwrap(),
        "",
        "the review loop cleared the previous disclosure"
    );
    assert!(
        state["design_trail"]
            .as_str()
            .unwrap()
            .contains("Design: ASK -- fold the two parsers into one trait?"),
        "the open question survives for the self_review to carry verbatim"
    );
}

/// The fault script's line grammar is "a trail line that, once
/// trimmed, begins `Design: ASK -- `". A CRLF ending is whitespace and is
/// trimmed; a bulleted or label-less variant, an ASK with no question text,
/// and the review skill's own `Design: DO/DEFER/DON'T` verdict shapes are
/// not ASK lines and never reach `design_notes`; a non-string review_notes
/// is stringified rather than crashing the fallback path.
#[test]
fn usage_probe_coder_self_review_fault_lifts_only_trimmed_ask_lines() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("self-review-fault-grammar");
    let trail = "\nSummary.\nDesign: ASK -- q1?\r\n- Design: ASK -- bulleted?\nASK -- label-less?\nDesign: ASK -- \nDesign: ASK --no-space?\nDesign: DO -- keep it\nDesign: DEFER -- later\nDesign: DON'T -- no\n\tDesign: ASK -- q2?\t\nDesign: ASK -- q1?";
    let mut state = project_state(&dir.0);
    state["design_trail"] = Value::String(trail.to_string());
    state["review_notes"] = Value::Number(7.into());
    let out = run_script_json("coder/scripts/self_review_fault.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "end_success", "{out}");
    assert_eq!(
        str_field(&out, "design_notes"),
        "ASK -- q1?\nASK -- q2?",
        "CRLF and tab padding are trimmed, duplicates fold to the first, everything else is left behind: {out}"
    );
    assert_eq!(
        str_field(&out, "review_notes"),
        "self-review did not run; 7",
        "a non-string review_notes is stringified: {out}"
    );
    assert_eq!(
        out.as_object().unwrap().len(),
        3,
        "the fault emits exactly design_notes, review_notes and _next: {out}"
    );

    // Running the fault twice over the same state is a no-op the second time.
    let mut again = state.clone();
    for (k, v) in out.as_object().unwrap() {
        again[k] = v.clone();
    }
    let out2 = run_script_json("coder/scripts/self_review_fault.sh", &dir.0, &again, &[]);
    assert_eq!(
        str_field(&out2, "design_notes"),
        str_field(&out, "design_notes"),
        "idempotent over the trail: {out2}"
    );
}

/// Both null-guard twins (`(.design_notes // "") | tostring`
/// in coder route_review_result, `(.design_decisions // "" | tostring)` in
/// step-runner check_handoff) treat a MISSING key like null and stringify a
/// non-string instead of letting the end template print a bare JSON value.
/// Routes that do not end the run carry neither key.
#[test]
fn usage_probe_design_disclosure_guards_stringify_non_strings_on_both_twins() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("design-guard-twins");
    for (state, next, expected) in [
        (serde_json::json!({"review_clean": true}), "end_success", ""),
        (
            serde_json::json!({"review_clean": true, "design_notes": 3}),
            "end_success",
            "3",
        ),
        (
            serde_json::json!({"review_clean": false, "review_attempts": 1, "design_notes": ["a"]}),
            "end_success",
            "[\"a\"]",
        ),
    ] {
        let out = run_script_json("coder/scripts/route_review_result.sh", &dir.0, &state, &[]);
        assert_eq!(str_field(&out, "_next"), next, "{state} -> {out}");
        assert_eq!(
            str_field(&out, "design_notes"),
            expected,
            "{state} -> {out}"
        );
        assert!(
            out.get("design_trail").is_none(),
            "the route never rewrites the trail: {out}"
        );
    }

    let handoff = dir.0.join("handoff.md");
    fs::write(
        &handoff,
        "---\nresult: complete\n---\n## Summary\n## Completed\n## Not completed\n## Deviations\n## Downstream plan updates\n## Edge cases discovered\n## Evidence\n## Notes for next step\n",
    )
    .unwrap();
    let base = serde_json::json!({
        "handoff_path": handoff.to_str().unwrap(),
        "step_plan_path": "",
    });
    let mut missing = base.clone();
    missing["handoff_attempts"] = Value::Number(0.into());
    let out = run_script_json(
        "step-runner/scripts/check_handoff.sh",
        &dir.0,
        &missing,
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "gate_user_review", "{out}");
    assert_eq!(
        str_field(&out, "design_decisions"),
        "",
        "a missing key is guarded like null: {out}"
    );
    let mut number = base.clone();
    number["design_decisions"] = Value::Number(12.into());
    let out = run_script_json("step-runner/scripts/check_handoff.sh", &dir.0, &number, &[]);
    assert_eq!(str_field(&out, "design_decisions"), "12", "{out}");

    // A handoff that fails validation loops back without touching the key,
    // so write_handoff's next pass is the only writer.
    let mut broken = base.clone();
    broken["handoff_path"] = Value::String(dir.0.join("absent.md").to_str().unwrap().into());
    broken["design_decisions"] = Value::Null;
    let out = run_script_json("step-runner/scripts/check_handoff.sh", &dir.0, &broken, &[]);
    assert_eq!(str_field(&out, "_next"), "write_handoff", "{out}");
    assert!(
        out.get("design_decisions").is_none(),
        "the retry route leaves design_decisions to write_handoff: {out}"
    );
}

/// `line` repeated until the text holds at least `min` bytes.
fn lines_of(line: &str, min: usize) -> String {
    let mut text = String::with_capacity(min + line.len());
    while text.len() < min {
        text.push_str(line);
    }
    text
}

/// The first `n` bytes of `text` as bash's `$(head -c n ...)` yields them:
/// trailing newlines stripped by the command substitution.
fn head_c(text: &str, n: usize) -> &str {
    text[..n].trim_end_matches('\n')
}

/// The last `n` chars of `text`, for failure messages that must not dump
/// a 40 KB field.
fn last_chars(text: &str, n: usize) -> &str {
    let start = text.char_indices().rev().nth(n - 1).map_or(0, |(i, _)| i);
    &text[start..]
}

/// A 40 KB review report with a CRITICAL mark enters the fix branch and
/// arrives in fix_instructions byte for byte after the script's header,
/// minus the trailing newline that `$(...)` always strips. The report
/// carries the bytes jq must escape (quotes, backslashes, a CR) and the
/// multibyte mark.
#[test]
fn route_review_carries_a_state_sized_report_into_fix_instructions() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-review");
    let tail = "END ".repeat(250);
    let report = format!(
        "{}{tail}\n",
        lines_of(
            "- \u{1F534} CRITICAL: \"quoted\" and \\back\\slash and a\rcarriage return\n",
            40_000
        )
    );
    assert!(report.len() > 32_767, "report must exceed the Windows cap");
    let out = run_script_json(
        "step-runner/scripts/route_review.sh",
        &dir.0,
        &serde_json::json!({
            "review_report": report,
            "review_attempts": 0,
            "max_review_attempts": 2,
        }),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    assert_eq!(out["review_attempts"], 1);
    assert_eq!(out["needs_independent_review"], false);
    let header = "## Independent review findings (attempt 1 of 2)\n\nAn independent reviewer flagged \
                  CRITICAL (\u{1F534}) findings. Address ONLY the \u{1F534} findings with minimal \
                  edits. Do not refactor unrelated code.\n\n";
    let fix = str_field(&out, "fix_instructions");
    assert!(
        fix.starts_with(header),
        "{}",
        fix.chars().take(300).collect::<String>()
    );
    assert!(fix.ends_with(&tail), "{}", last_chars(fix, 300));
    let expected = format!("{header}{}", report.trim_end_matches('\n'));
    assert!(
        fix == expected,
        "fix_instructions is {} bytes, header + report is {}",
        fix.len(),
        expected.len()
    );
}

/// A 40 KB user comment at the approval gate rides fix_instructions byte
/// for byte after the script's header. `payload` ends without a newline,
/// so the `$(...)` strip inside the script has nothing to take.
#[test]
fn revise_from_choice_carries_state_sized_feedback_into_fix_instructions() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("revise-from-choice");
    let feedback = payload(40_000);
    assert!(
        feedback.len() > 32_767,
        "feedback must exceed the Windows cap"
    );
    let out = run_script_json(
        "step-runner/scripts/revise_from_choice.sh",
        &dir.0,
        &serde_json::json!({"user_feedback": feedback}),
        &[],
    );
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    let header = "## Revision requested by the user at the step approval gate\n\nAddress these \
                  comments with minimal edits, then the step re-verifies and the handoff is \
                  rewritten:\n\n";
    let fix = str_field(&out, "fix_instructions");
    assert!(
        fix.starts_with(header),
        "{}",
        fix.chars().take(300).collect::<String>()
    );
    assert!(
        fix.ends_with(&feedback),
        "fix_instructions is {} bytes and ends: {}",
        fix.len(),
        last_chars(fix, 300)
    );
    let expected = format!("{header}{feedback}");
    assert!(
        fix == expected,
        "fix_instructions is {} bytes, header + feedback is {}",
        fix.len(),
        expected.len()
    );
}

/// The three documents resolve_step reads are each cut by `head -c` inside
/// a `$(...)`, so the output holds exactly the first N bytes with trailing
/// newlines stripped: the plan is cut mid-line, the handoff and notes land
/// on a line end so the strip is observable. The key set is the one the
/// graph has always consumed.
#[test]
fn resolve_step_cuts_each_document_at_its_head_c_limit() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("resolve-step");
    let plans = dir.0.join("plans");
    fs::create_dir_all(plans.join("steps")).unwrap();
    fs::create_dir_all(plans.join("handoffs")).unwrap();
    // 25-byte lines: 8000 is a multiple, and the handoff frontmatter is exactly
    // one line-length, so both cuts end on a newline (editing that frontmatter
    // moves the handoff cut); the plan's frontmatter shifts its cut into a line.
    let line = "doc line: 0123456789 abc\n";
    assert_eq!(line.len(), 25);
    let plan = format!(
        "---\ntitle: second\nstatus: pending\n---\n{}",
        lines_of(line, 30_000)
    );
    fs::write(plans.join("steps/02-second.md"), &plan).unwrap();
    fs::write(
        plans.join("steps/01-first.md"),
        "---\ntitle: first\nstatus: complete\n---\nfirst\n",
    )
    .unwrap();
    let handoff = format!("---\nresult: complete\n---\n{}", lines_of(line, 20_000));
    assert_eq!(handoff.as_bytes()[15_999], b'\n');
    fs::write(plans.join("handoffs/01-first.md"), &handoff).unwrap();
    let notes = lines_of(line, 10_000);
    assert_eq!(notes.as_bytes()[7_999], b'\n');
    fs::write(plans.join("NOTES.md"), &notes).unwrap();

    let out = run_script_json(
        "step-runner/scripts/resolve_step.sh",
        &dir.0,
        &serde_json::json!({"initial_prompt": "Run step 2 of the plan"}),
        &[
            ("LLM_AGENT_VAR_PROJECT_DIR", dir.0.to_str().unwrap()),
            ("LLM_AGENT_VAR_STEP", "next"),
        ],
    );
    assert_eq!(str_field(&out, "_next"), "orient", "{out}");
    assert_eq!(out["step_number"], 2);
    assert_eq!(str_field(&out, "step_title"), "second");
    assert_eq!(str_field(&out, "step_plan"), head_c(&plan, 24_000));
    assert_eq!(str_field(&out, "prev_handoff"), head_c(&handoff, 16_000));
    assert_eq!(str_field(&out, "notes"), head_c(&notes, 8_000));
    let mut keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "_next",
            "blocking_reason",
            "handoff_path",
            "notes",
            "notes_path",
            "plans_dir",
            "prev_handoff",
            "prev_handoff_path",
            "project_dir",
            "step_number",
            "step_plan",
            "step_plan_path",
            "step_slug",
            "step_title",
        ]
    );
}

/// Resolves the shell the librarian's node timeout is measured against on
/// a stock macOS install: `/bin/bash` is bash 3.2 there, the interpreter
/// whose `${x//[[:space:]]/}` expansion is quadratic. Elsewhere `/bin/bash`
/// is a modern bash and the probe still runs, just without the sharp edge.
fn system_bash() -> Command {
    let system = Path::new("/bin/bash");
    if system.is_file() {
        Command::new(system)
    } else {
        bash()
    }
}

/// Seeds `findings` through GRAPH_STATE_FILE and runs final_format.sh under
/// `system_bash`, returning the parsed output and the wall time.
fn run_final_format_timed(dir: &Path, label: &str, findings: &str) -> (Value, std::time::Duration) {
    let state_file = dir.join(format!("final-format-{label}.json"));
    fs::write(
        &state_file,
        serde_json::json!({ "findings": findings }).to_string(),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let mut cmd = system_bash();
    cmd.arg(asset("librarian/scripts/final_format.sh"))
        .env("GRAPH_STATE_FILE", &state_file)
        .env_remove("GRAPH_STATE");
    let (code, stdout, stderr) = run(cmd.output().expect("spawn bash"));
    let elapsed = started.elapsed();
    assert_eq!(
        code, 0,
        "final_format.sh[{label}] exited {code}\nstderr: {stderr}"
    );
    let out: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("final_format.sh[{label}] emitted invalid JSON ({e})"));
    (out, elapsed)
}

/// The librarian node runs final_format.sh with `timeout: 5`. Its
/// whitespace probe must stay linear so a 40 KB report (with no heading,
/// so the whole body is probed) and a 40 KB all-whitespace body both finish
/// well inside that budget under the system bash, routing to end_success
/// and end_failure respectively with the body carried byte for byte.
#[test]
fn final_format_probes_a_40kb_body_inside_the_node_timeout_under_the_system_bash() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("final-format-linear");
    let node_timeout = std::time::Duration::from_secs(5);

    let body = payload(40_000);
    assert!(body.len() > 32_767 && !body.contains("## Findings"));
    let (out, elapsed) = run_final_format_timed(&dir.0, "text", &body);
    assert_eq!(str_field(&out, "_next"), "end_success");
    assert!(
        str_field(&out, "findings") == body,
        "40 KB body did not round-trip ({} bytes out)",
        str_field(&out, "findings").len()
    );
    assert!(
        elapsed < node_timeout,
        "40 KB body took {elapsed:?}; the node kills the script at {node_timeout:?}"
    );

    // No CR directly before LF: `run` folds CRLF, which would mask the round
    // trip. The `$(...)` capture of `jq -r` strips the trailing newline.
    let blank = " \t\r \n".repeat(8_000);
    let (out, elapsed) = run_final_format_timed(&dir.0, "blank", &blank);
    assert_eq!(str_field(&out, "_next"), "end_failure");
    assert!(
        str_field(&out, "findings") == blank.trim_end_matches('\n'),
        "40 KB of whitespace did not round-trip ({} bytes out)",
        str_field(&out, "findings").len()
    );
    assert!(
        elapsed < node_timeout,
        "40 KB of whitespace took {elapsed:?}; the node kills the script at {node_timeout:?}"
    );
}

/// A `mktemp` for the front of PATH that refuses `-d` and defers everything
/// else to the real one, so only the spill-directory step can fail.
#[cfg(unix)]
fn mktemp_without_dirs(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let real = which::which("mktemp").expect("mktemp on PATH");
    let bin = dir.join("mktemp-bin");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("mktemp");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = -d ] && {{ echo 'mktemp: no directories today' >&2; exit 1; }}; done\nexec '{}' \"$@\"\n",
            real.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn entries_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A step layout resolve_step.sh resolves to step 1 with no prior handoff.
fn one_step_project(dir: &Path) {
    let plans = dir.join("plans");
    fs::create_dir_all(plans.join("steps")).unwrap();
    fs::create_dir_all(plans.join("handoffs")).unwrap();
    fs::write(
        plans.join("steps/01-first.md"),
        "---\ntitle: first\nstatus: pending\n---\nbody\n",
    )
    .unwrap();
}

/// The spill directory resolve_step.sh and verify_format_lint.sh hand to
/// `--rawfile` is gone when the script exits, on the success route and the
/// early-exit route alike: a private TMPDIR is empty afterwards.
#[test]
fn spill_directories_are_removed_when_the_scripts_exit() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("spill-cleanup");
    one_step_project(&dir.0);
    let tmp = dir.0.join("private-tmp");
    fs::create_dir_all(&tmp).unwrap();
    let tmp_str = tmp.to_str().unwrap();

    let out = run_script_json(
        "step-runner/scripts/resolve_step.sh",
        &dir.0,
        &serde_json::json!({}),
        &[
            ("LLM_AGENT_VAR_PROJECT_DIR", dir.0.to_str().unwrap()),
            ("TMPDIR", tmp_str),
        ],
    );
    assert_eq!(str_field(&out, "_next"), "orient", "{out}");
    assert_eq!(
        entries_in(&tmp),
        Vec::<String>::new(),
        "resolve_step left files in TMPDIR"
    );

    for (label, envs) in [
        ("skip", vec![("TMPDIR", tmp_str)]),
        (
            "green",
            vec![
                ("TMPDIR", tmp_str),
                ("FORMAT_CMD", "echo formatted"),
                ("LINT_CMD", "echo linted"),
            ],
        ),
        (
            "red",
            vec![
                ("TMPDIR", tmp_str),
                ("FORMAT_CMD", "echo formatted"),
                ("LINT_CMD", "echo lint failed; exit 2"),
            ],
        ),
    ] {
        let out = run_script_json(
            "step-runner/scripts/verify_format_lint.sh",
            &dir.0,
            &serde_json::json!({ "project_dir": dir.0.to_str().unwrap() }),
            &envs,
        );
        assert!(str_field(&out, "format_output").len() > 1, "{label}: {out}");
        assert_eq!(
            entries_in(&tmp),
            Vec::<String>::new(),
            "verify_format_lint[{label}] left files in TMPDIR"
        );
    }
}

/// When `mktemp -d` fails, each script stops before jq with exit 1, an
/// empty stdout (no half-built JSON for the engine to merge) and a stderr
/// line naming the script, the failed call and the TMPDIR in play. Unix
/// only: the failing mktemp is a PATH shim with an executable bit.
#[cfg(unix)]
#[test]
fn a_failed_mktemp_d_stops_each_script_before_jq_and_names_its_cause() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("spill-mktemp-fails");
    one_step_project(&dir.0);
    let plan_path = dir.0.join("plans/steps/01-first.md");
    let plan_before = fs::read(&plan_path).unwrap();
    let shim_bin = mktemp_without_dirs(&dir.0);
    let path = format!(
        "{}:{}",
        shim_bin.display(),
        env::var("PATH").unwrap_or_default()
    );
    let tmp = dir.0.join("private-tmp");
    fs::create_dir_all(&tmp).unwrap();

    for (rel, state, extra) in [
        (
            "step-runner/scripts/resolve_step.sh",
            serde_json::json!({}),
            vec![("LLM_AGENT_VAR_PROJECT_DIR", dir.0.to_str().unwrap())],
        ),
        (
            "step-runner/scripts/verify_format_lint.sh",
            serde_json::json!({ "project_dir": dir.0.to_str().unwrap() }),
            vec![
                ("FORMAT_CMD", "echo formatted"),
                ("LINT_CMD", "echo linted"),
            ],
        ),
    ] {
        let mut envs = vec![("PATH", path.as_str()), ("TMPDIR", tmp.to_str().unwrap())];
        envs.extend(extra);
        let (code, stdout, stderr) = run_script(rel, &dir.0, &state, &envs);
        let script = rel.rsplit('/').next().unwrap();
        assert_eq!(code, 1, "{rel}\nstdout: {stdout}\nstderr: {stderr}");
        assert_eq!(
            stdout, "",
            "{rel} must not emit JSON once the spill dir is missing"
        );
        let expected = format!("{script}: mktemp -d failed (TMPDIR={})", tmp.display());
        assert!(
            stderr.lines().any(|line| line == expected),
            "{rel} stderr must carry {expected:?}, got:\n{stderr}"
        );
    }
    // resolve_step.sh spills before it rewrites the plan's status, so a
    // missing spill directory leaves the step plan exactly as found.
    assert_eq!(
        fs::read(&plan_path).unwrap(),
        plan_before,
        "a failed mktemp -d must leave the step plan file untouched (no status rewrite)"
    );
}

/// Every `$var` a shipped script may hand to jq through `--arg` or
/// `--argjson`, keyed by path under assets/agents. Each entry is bounded by
/// something visible in the script: a fixed literal, a path, a `head -c` /
/// `${x:0:N}` cut, a command string, a small integer (exit codes, attempt
/// counters, step numbers), or (`gate_error`) the engine's 8 KiB cap on
/// SCRIPT_FALLBACK_ERROR_MAX_BYTES. Anything not listed here is presumed
/// state-sized and must take the stdin or `--rawfile` route.
const JQ_ARG_ALLOW_LIST: &[(&str, &[&str])] = &[
    (".shared/utils.sh", &["s"]),
    (
        "coder/scripts/fix_loop_gate.sh",
        &["gate_error", "fix_attempts", "next_attempts"],
    ),
    ("coder/scripts/resolve_paths.sh", &["resolved"]),
    ("coder/scripts/route_review_result.sh", &["next_review"]),
    ("coder/scripts/verify_build.sh", &["cmd", "exit_code"]),
    ("coder/scripts/verify_tests.sh", &["cmd", "exit_code"]),
    ("librarian/scripts/final_format.sh", &["next"]),
    ("step-runner/scripts/check_handoff.sh", &["problems"]),
    (
        "step-runner/scripts/fix_loop_gate.sh",
        &["gate_error", "fix_attempts", "next_attempts"],
    ),
    ("step-runner/scripts/note_llm_fault.sh", &["fault_note"]),
    (
        "step-runner/scripts/resolve_step.sh",
        &[
            "1",
            "project_dir",
            "plans_dir",
            "step_number",
            "step_slug",
            "step_title",
            "plan_file",
            "prev_handoff_path",
            "notes_path",
            "handoff_path",
            "blocking_reason",
            "next_node",
        ],
    ),
    ("step-runner/scripts/route_review.sh", &["next_review"]),
    ("step-runner/scripts/verify_build.sh", &["cmd", "exit_code"]),
    (
        "step-runner/scripts/verify_format_lint.sh",
        &["lint_cmd", "lint_rc"],
    ),
    ("step-runner/scripts/verify_tests.sh", &["cmd", "exit_code"]),
];

const JQ_ARGV_RULE: &str = "state-sized text must reach jq on stdin (`printf '%s' \"$v\" | jq -Rsc`) \
     or via `--rawfile <name> <real file>`; argv is capped at 32 KiB on Windows";

fn shell_scripts_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            shell_scripts_under(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "sh") {
            out.push(path);
        }
    }
}

/// Bash's blank metacharacters: the only characters that separate words.
fn bash_blank(c: char) -> bool {
    matches!(c, ' ' | '\t')
}

/// The script with comment-only lines dropped, so prose that quotes the
/// forbidden idioms (the comments explaining this rule) is not a hit. A
/// `#` opens a comment only outside quotes, so a line that starts with
/// `#` inside a multi-line quoted operand is kept: bash hands it to jq
/// as part of the value.
fn code_lines(script: &str) -> String {
    let mut kept = Vec::new();
    let mut in_double = false;
    let mut in_single = false;
    // `$'...'`: a backslash escapes the next character, the quote included.
    let mut in_ansi = false;
    // Quote state saved at each unclosed `$(` or backtick: the text inside a
    // command substitution is parsed afresh, so its quotes nest on their own.
    // ... plus the paren depth of a subshell/group inside that frame (its `)`
    // closes the group, not the substitution) and whether the frame was
    // opened by a backtick (closed by the next backtick, never by `)`).
    let mut nested: Vec<(bool, bool, usize, bool)> = Vec::new();
    let mut depth = 0usize;
    // Terminators of heredocs opened on the current line (body follows), and
    // the one whose body is being read: body lines are data, so they drive
    // neither the comment strip nor the quote bookkeeping.
    let mut pending_heredocs: Vec<(String, bool)> = Vec::new();
    let mut heredoc: Option<(String, bool)> = None;
    // True when the previous kept line ended in an unquoted backslash that
    // bash deletes together with the newline: the next line continues the
    // same word, so a leading `#` there is text, not a comment.
    let mut continued = false;
    for line in script.lines() {
        if let Some((term, strip_tabs)) = &heredoc {
            kept.push(line);
            let body = if *strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if body == term {
                heredoc = if pending_heredocs.is_empty() {
                    None
                } else {
                    Some(pending_heredocs.remove(0))
                };
            }
            continue;
        }
        if !in_double
            && !in_single
            && !in_ansi
            && !continued
            && line.trim_start_matches(bash_blank).starts_with('#')
        {
            continue;
        }
        kept.push(line);
        // A line that only continues a glued word (a bare `\`) inherits the
        // glue; bash's blanks are space and tab, so a CR, FF or NBSP before
        // the backslash still glues.
        let mut prev = if continued { '\\' } else { ' ' };
        continued = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if in_ansi {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '\'' => in_ansi = false,
                    _ => {}
                }
                prev = c;
                continue;
            }
            match c {
                '\\' if !in_single => {
                    if chars.next().is_none() {
                        // `x \<nl># prose` is still a comment: only a backslash
                        // glued to a word carries that word onto the next line.
                        continued = !bash_blank(prev);
                    }
                }
                '"' if !in_single => in_double = !in_double,
                '\'' if !in_double => in_single = !in_single,
                '$' if !in_single => match chars.peek() {
                    Some('\'') if !in_double => {
                        chars.next();
                        in_ansi = true;
                    }
                    Some('(') => {
                        chars.next();
                        nested.push((in_double, in_single, depth, false));
                        in_double = false;
                        in_single = false;
                        depth = 0;
                    }
                    _ => {}
                },
                '(' if !in_double && !in_single => depth += 1,
                ')' if !in_double && !in_single => {
                    if depth > 0 {
                        depth -= 1;
                    } else if let Some((d, s, dp, _)) = nested.pop_if(|frame| !frame.3) {
                        in_double = d;
                        in_single = s;
                        depth = dp;
                    }
                }
                '`' if !in_single => {
                    if let Some((d, s, dp, _)) = nested.pop_if(|frame| frame.3) {
                        in_double = d;
                        in_single = s;
                        depth = dp;
                    } else {
                        nested.push((in_double, in_single, depth, true));
                        in_double = false;
                        in_single = false;
                        depth = 0;
                    }
                }
                // `<<WORD` / `<<-WORD` (not the here-string `<<<`) opens a
                // heredoc whose body starts on the next line.
                '<' if !in_double && !in_single && chars.peek() == Some(&'<') => {
                    chars.next();
                    if chars.peek() == Some(&'<') {
                        chars.next();
                    } else {
                        let strip_tabs = chars.peek() == Some(&'-');
                        if strip_tabs {
                            chars.next();
                        }
                        while chars.peek().is_some_and(|c| bash_blank(*c)) {
                            chars.next();
                        }
                        let mut term = String::new();
                        while let Some(&c) = chars.peek() {
                            if bash_blank(c) || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')')
                            {
                                break;
                            }
                            chars.next();
                            if !matches!(c, '"' | '\'' | '\\') {
                                term.push(c);
                            }
                        }
                        pending_heredocs.push((term, strip_tabs));
                    }
                }
                '#' if !in_double && !in_single && bash_blank(prev) => break,
                _ => {}
            }
            prev = c;
        }
        if heredoc.is_none() && !pending_heredocs.is_empty() {
            heredoc = Some(pending_heredocs.remove(0));
        }
    }
    kept.join("\n")
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Blanks and backslash-newline continuations, so a value pushed onto the
/// next line is still read as the value.
fn skip_blanks(mut rest: &str) -> &str {
    loop {
        rest = rest.trim_start();
        match rest.strip_prefix("\\\n") {
            Some(after) => rest = after,
            None => return rest,
        }
    }
}

/// One `--arg <name> <value>` style operand: the whole shell word, quotes
/// included. `quoted` is false when any bare segment of the word carries an
/// expansion, whose size the scan cannot read with confidence.
struct JqFlagValue<'a> {
    name: &'a str,
    value: &'a str,
    quoted: bool,
}

/// Length of the shell word at the start of `rest` and whether every bare
/// segment of it is free of `$`, backtick and backslash. Bash joins
/// adjacent quoted and bare segments into one word (`"$a"$b`,
/// `"x""$(cmd)"`), so every segment is read up to an unquoted blank.
fn shell_word(rest: &str) -> (usize, bool) {
    let bytes = rest.as_bytes();
    let mut end = 0;
    let mut quoted = true;
    while end < bytes.len() {
        match bytes[end] {
            b'"' => {
                end += 1;
                while end < bytes.len() && bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                end = (end + 1).min(bytes.len());
            }
            b'\'' => {
                end += 1;
                while end < bytes.len() && bytes[end] != b'\'' {
                    end += 1;
                }
                end = (end + 1).min(bytes.len());
            }
            // Only bash's blank metacharacters end a word; a CR or FF
            // inside the word is just a byte of it.
            b' ' | b'\t' | b'\n' => break,
            // Bash deletes a backslash-newline, so what follows it joins
            // this word unless it is blank: `"$a"\<nl>$big` is one word,
            // `"$a"\<nl>  --arg` is two.
            b'\\' if bytes.get(end + 1) == Some(&b'\n') => match bytes.get(end + 2) {
                Some(next) if !matches!(next, b' ' | b'\t' | b'\n') => end += 2,
                _ => break,
            },
            c => {
                if matches!(c, b'$' | b'`' | b'\\') {
                    quoted = false;
                }
                end += 1;
            }
        }
    }
    (end.min(rest.len()), quoted)
}

/// The shell words after a `--arg <name>` / `--rawfile <name>` flag: the
/// name word and the value word. jq binds any string as a name (`--arg
/// -r "$big"` is legal), so the name word is checked like the value and
/// an expansion sitting in name position is refused the same way.
fn jq_flag_value<'a>(code: &'a str, flag: &str) -> Vec<JqFlagValue<'a>> {
    let mut hits = Vec::new();
    for (idx, _) in code.match_indices(flag) {
        let rest = &code[idx + flag.len()..];
        if !(rest.starts_with(|c: char| c.is_whitespace()) || rest.starts_with("\\\n")) {
            continue;
        }
        let rest = skip_blanks(rest);
        let (name_end, name_quoted) = shell_word(rest);
        let name_word = &rest[..name_end];
        let name = name_word.trim_matches(|c| c == '\'' || c == '"');
        let rest = skip_blanks(&rest[name_end..]);
        let (end, quoted) = shell_word(rest);
        let value = &rest[..end];
        hits.push(JqFlagValue {
            name,
            value: name_word,
            quoted: name_quoted,
        });
        hits.push(JqFlagValue {
            name,
            value,
            quoted,
        });
    }
    hits
}

/// Every `$name` / `${name...}` expansion inside a double-quoted value.
fn expanded_vars(value: &str) -> Vec<&str> {
    let mut vars = Vec::new();
    let mut rest = value;
    while let Some(pos) = rest.find('$') {
        let after = &rest[pos + 1..];
        let body = after.strip_prefix('{').unwrap_or(after);
        let end = body.find(|c: char| !is_name_char(c)).unwrap_or(body.len());
        if end > 0 {
            vars.push(&body[..end]);
        }
        rest = &body[end..];
    }
    vars
}

/// The argv-pin violations in one script's text; `rel` is its path under
/// assets/agents, which selects the allow-list and prefixes each message.
fn jq_argv_violations(rel: &str, script: &str) -> Vec<String> {
    let code = code_lines(script);
    let allowed: &[&str] = JQ_ARG_ALLOW_LIST
        .iter()
        .find(|(script, _)| *script == rel)
        .map_or(&[], |(_, vars)| vars);
    let mut violations = Vec::new();

    for flag in ["--rawfile", "--slurpfile"] {
        for JqFlagValue { name, value, .. } in jq_flag_value(&code, flag) {
            // Any segment of the word may be the operand bash hands jq, so the
            // whole word is read, quotes included.
            let joined: String = value
                .chars()
                .filter(|c| !matches!(c, '"' | '\'' | '\\'))
                .collect();
            if value.contains("<(") {
                violations.push(format!(
                    "{rel}: `{flag} {name} <(...)` is a process substitution; jq.exe cannot open it"
                ));
            } else if joined.contains("/dev/") || joined.contains("/proc/") {
                violations.push(format!(
                    "{rel}: `{flag} {name} {value}` names an MSYS device or /proc entry; jq.exe cannot open it"
                ));
            }
        }
    }
    // Positional operands carry no name to allow-list, so the flags that
    // introduce them are refused outright.
    for flag in ["--args", "--jsonargs"] {
        for line in code.lines().filter(|line| {
            line.split_whitespace()
                .any(|token| token.trim_end_matches('\\') == flag)
        }) {
            violations.push(format!(
                "{rel}: `{flag}` puts positional operands on jq's argv ({}); {JQ_ARGV_RULE}",
                line.trim()
            ));
        }
    }
    for flag in ["--arg", "--argjson"] {
        for JqFlagValue {
            name,
            value,
            quoted,
        } in jq_flag_value(&code, flag)
        {
            if value.contains("$(") || value.contains('`') || value.contains("${!") {
                violations.push(format!(
                    "{rel}: `{flag} {name}` runs a command substitution or indirect expansion, whose size nothing bounds"
                ));
                continue;
            }
            if !quoted {
                violations.push(format!(
                    "{rel}: `{flag} {name} {value}` is an unquoted expansion; double-quote it so the scan can read it"
                ));
                continue;
            }
            for var in expanded_vars(value) {
                if !allowed.contains(&var) {
                    violations.push(format!(
                        "{rel}: `{flag} {name}` expands ${var}, which is not in the allow-list of bounded variables"
                    ));
                }
            }
        }
    }
    violations
}

/// Each bypass shape the scan must refuse, run against an in-memory script
/// so the pin itself is under test, next to the shapes it must accept. The
/// allow-list in play is resolve_step.sh's.
#[test]
fn jq_argv_pin_catches_every_known_bypass_shape() {
    let rel = "step-runner/scripts/resolve_step.sh";
    let slipped: Vec<&str> = [
        (
            "command substitution",
            r#"jq -nc --arg r "$(jq -r .review_report <<< "$GRAPH_STATE")" '{}'"#,
        ),
        ("backtick", "jq -nc --arg r \"`cat report`\" '{}'"),
        ("indirect expansion", r#"jq -nc --arg r "${!ref}" '{}'"#),
        (
            "line continuation",
            "jq -nc --arg sp \\\n  \"$step_plan\" '{}'",
        ),
        ("bare word", "jq -nc --arg sp $step_plan '{}'"),
        ("double-quoted name", r#"jq -nc --arg "r" "$big" '{}'"#),
        ("argjson", r#"jq -nc --argjson big "$big" '{}'"#),
        ("rawfile on /dev", r#"jq -nc --rawfile x /dev/stdin '{}'"#),
        (
            "rawfile process substitution",
            r#"jq -nc --rawfile x <(printf '%s' "$big") '{}'"#,
        ),
        (
            "slurpfile on /dev",
            r#"jq -nc --slurpfile x /dev/stdin '{}'"#,
        ),
        (
            "slurpfile process substitution",
            r#"jq -nc --slurpfile x <(printf '%s' "$big") '{}'"#,
        ),
        ("positional args", r#"jq -nc '$ARGS' --args "$big""#),
    ]
    .into_iter()
    .filter(|(_, script)| jq_argv_violations(rel, script).is_empty())
    .map(|(label, _)| label)
    .collect();
    assert!(slipped.is_empty(), "slipped past the pin: {slipped:?}");

    for (label, script) in [
        ("literal string", r#"jq -nc --arg r "fixed text" '{}'"#),
        (
            "allow-listed variable",
            r#"jq -nc --arg pd "$project_dir" '{}'"#,
        ),
        (
            "allow-listed argjson",
            r#"jq -nc --argjson sn "$step_number" '{}'"#,
        ),
        (
            "rawfile on a real file",
            r#"jq -nc --rawfile sp "$spill/sp" '{}'"#,
        ),
        (
            "slurpfile on a real file",
            r#"jq -nc --slurpfile x "$spill/fo" '{}'"#,
        ),
        (
            "quoted-value scan reaches the real variable",
            "jq -nc \\\n  --arg pd \\\n  \"$project_dir\" '{}'",
        ),
    ] {
        let found = jq_argv_violations(rel, script);
        assert!(found.is_empty(), "{label} was flagged: {found:?}");
    }
}

/// On Windows the graph engine's scripts run under MSYS bash but spawn the
/// native jq.exe, whose whole command line is capped at 32,767 chars and
/// which cannot open an MSYS `/dev/fd` pipe. MAX_STATE_SIZE_BYTES is 32 KiB,
/// so any state field handed to jq via `--arg "$var"` or
/// `--rawfile x <(...)` is broken there by construction. This walks every
/// shipped script and holds each `--arg` / `--argjson` to the allow-list of
/// variables whose size is visibly bounded, and refuses `--args` /
/// `--jsonargs` outright. It only reads files, so it runs on every OS,
/// Windows included.
#[test]
fn shipped_scripts_never_put_state_sized_text_on_jq_argv() {
    let root = asset("");
    let mut scripts = Vec::new();
    shell_scripts_under(&root, &mut scripts);
    scripts.sort();
    assert!(!scripts.is_empty(), "no scripts under {}", root.display());

    let mut violations = Vec::new();
    for path in &scripts {
        let rel = path
            .strip_prefix(&root)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\\', "/");
        violations.extend(jq_argv_violations(&rel, &fs::read_to_string(path).unwrap()));
    }

    assert!(
        violations.is_empty(),
        "{JQ_ARGV_RULE}\n{}",
        violations.join("\n")
    );
    for (script, _) in JQ_ARG_ALLOW_LIST {
        assert!(
            scripts.iter().any(|p| p.ends_with(script)),
            "allow-list names a script that no longer ships: {script}"
        );
    }
}

// ---- the Windows argv contract, observed on this OS ----

/// A `jq` shim for the front of PATH that appends the byte total of every
/// argv it receives to `log`, then hands over to the real jq. The shipped
/// scripts call `jq` by bare name, so this sees exactly what jq.exe would
/// be asked to carry on its command line on Windows.
#[cfg(unix)]
fn jq_argv_shim(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let real = which::which("jq").expect("jq resolved by tools_missing");
    let log = dir.join("jq-argv.log");
    let bin = dir.join("shim-bin");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("jq");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nn=0\nfor a in \"$@\"; do n=$((n+${{#a}}+1)); done\n\
             printf '%s\\n' \"$n\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// `n` bytes of text with the characters jq must escape (quotes,
/// backslashes, tabs, a red mark, multibyte), ending without a newline so
/// `$(...)` capture cannot alter it.
fn payload(n: usize) -> String {
    let unit = "line \u{1F534} with \"quotes\" and \\back\\slash and\ttab and \u{FC}n\u{EF}\n";
    let mut text = String::new();
    while text.len() < n {
        text.push_str(unit);
    }
    text.push_str("PAYLOAD END");
    text
}

/// Every shipped script that forwards a state-sized field to jq must keep
/// that field off jq's argv: on Windows the whole jq.exe command line is
/// capped at 32,767 chars and MAX_STATE_SIZE_BYTES is 32 KiB. With ~40 KB
/// in every such field, every jq invocation the five scripts make stays
/// under 8 KiB of argv while the field still arrives intact in the output.
/// Unix only: the shim relies on an executable bit and `#!/bin/sh`.
#[cfg(unix)]
#[test]
fn state_sized_fields_reach_jq_off_argv_in_every_shipped_script() {
    if tools_missing() {
        return;
    }
    let fx = fixture_dir("jq-argv");
    let dir = fx.0.as_path();
    let (shim_bin, log) = jq_argv_shim(dir);
    let path = format!(
        "{}:{}",
        shim_bin.display(),
        env::var("PATH").unwrap_or_default()
    );
    let envs: &[(&str, &str)] = &[("PATH", path.as_str())];
    let big = payload(40_000);
    assert!(big.len() > 32_767, "payload must exceed the Windows cap");

    // route_review: the review report rides fix_instructions.
    let out = run_script_json(
        "step-runner/scripts/route_review.sh",
        dir,
        &serde_json::json!({"review_report": big, "review_attempts": 0, "max_review_attempts": 2}),
        envs,
    );
    assert_eq!(str_field(&out, "_next"), "implement");
    assert_eq!(out["review_attempts"], 1);
    assert!(str_field(&out, "fix_instructions").ends_with(&big));

    // revise_from_choice: the user's feedback rides fix_instructions.
    let out = run_script_json(
        "step-runner/scripts/revise_from_choice.sh",
        dir,
        &serde_json::json!({"user_feedback": big}),
        envs,
    );
    assert_eq!(str_field(&out, "_next"), "implement");
    assert!(str_field(&out, "fix_instructions").ends_with(&big));

    // final_format: the findings body is trimmed to its heading and kept.
    let out = run_script_json(
        "librarian/scripts/final_format.sh",
        dir,
        &serde_json::json!({"findings": format!("narrative preamble\n## Findings\n{big}")}),
        envs,
    );
    assert_eq!(str_field(&out, "_next"), "end_success");
    assert_eq!(str_field(&out, "findings"), format!("## Findings\n{big}"));

    // resolve_step: three document bodies, each cut by head -c.
    let plans = dir.join("plans");
    fs::create_dir_all(plans.join("steps")).unwrap();
    fs::create_dir_all(plans.join("handoffs")).unwrap();
    let ascii_line = "plan line with \"quotes\" and \\slash and\ttab 0123456789\n";
    let plan = format!(
        "---\ntitle: second\nstatus: pending\n---\n{}",
        lines_of(ascii_line, 30_000)
    );
    fs::write(plans.join("steps/02-second.md"), &plan).unwrap();
    fs::write(
        plans.join("steps/01-first.md"),
        "---\ntitle: first\nstatus: complete\n---\nfirst\n",
    )
    .unwrap();
    let handoff = format!(
        "---\nresult: complete\n---\n{}",
        lines_of(ascii_line, 20_000)
    );
    fs::write(plans.join("handoffs/01-first.md"), &handoff).unwrap();
    let notes = lines_of(ascii_line, 10_000);
    fs::write(plans.join("NOTES.md"), &notes).unwrap();
    let project = dir.to_str().unwrap();
    let out = run_script_json(
        "step-runner/scripts/resolve_step.sh",
        dir,
        &serde_json::json!({}),
        &[
            ("PATH", path.as_str()),
            ("LLM_AGENT_VAR_PROJECT_DIR", project),
            ("LLM_AGENT_VAR_STEP", "2"),
        ],
    );
    assert_eq!(str_field(&out, "_next"), "orient");
    assert_eq!(str_field(&out, "step_plan"), head_c(&plan, 24_000));
    assert_eq!(str_field(&out, "prev_handoff"), head_c(&handoff, 16_000));
    assert_eq!(str_field(&out, "notes"), head_c(&notes, 8_000));

    // verify_format_lint: a 40 KB format transcript and a 40 KB red lint.
    let fmt_cmd =
        "python3 -c \"import sys; sys.stdout.write(('fmt line ' + 'f'*70 + chr(10))*500)\"";
    let lint_cmd = "python3 -c \"import sys; sys.stdout.write(('lint line ' + 'l'*70 + chr(10))*500)\"; exit 2";
    if which::which("python3").is_ok() {
        let out = run_script_json(
            "step-runner/scripts/verify_format_lint.sh",
            dir,
            &serde_json::json!({"project_dir": project}),
            &[
                ("PATH", path.as_str()),
                ("FORMAT_CMD", fmt_cmd),
                ("LINT_CMD", lint_cmd),
            ],
        );
        assert_eq!(str_field(&out, "_next"), "fix_loop_gate");
        assert_eq!(out["lint_ok"], false);
        assert!(str_field(&out, "format_output").len() > 32_767);
        assert!(str_field(&out, "lint_output").len() > 32_767);
    } else {
        eprintln!("python3 not on PATH: verify_format_lint transcript leg skipped");
    }

    let sizes: Vec<usize> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert!(sizes.len() >= 5, "expected one line per jq call: {sizes:?}");
    let largest = sizes.iter().copied().max().unwrap();
    assert!(
        largest < 8 * 1024,
        "a shipped script put {largest} bytes on jq's argv with ~40 KB fields; Windows caps the \
         whole jq.exe command line at 32,767 chars, so state-sized text must go on stdin or \
         through --rawfile on a real file (all argv totals: {sizes:?})"
    );
}

// ---- the bash resolver refuses a System32 launcher ----

/// Child half of the resolver probe: prints what `bash_program` resolves
/// to under the PATH the parent set. Ignored so a plain run never executes
/// it; the parent spawns this binary with `--ignored --exact`.
#[test]
#[ignore] // child half of a probe, run by its parent via --ignored --exact; not a skipped test
fn bash_program_probe_child() {
    match bash_program() {
        Some(p) => println!("BASH_PROGRAM={}", p.display()),
        None => println!("BASH_PROGRAM=none"),
    }
}

#[cfg(unix)]
fn resolver_answer(path_env: &str) -> String {
    let exe = env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "bash_program_probe_child",
            "--nocapture",
        ])
        .env("PATH", path_env)
        .output()
        .expect("spawn the test binary");
    let (_, stdout, stderr) = run(out);
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("BASH_PROGRAM="))
        .unwrap_or_else(|| panic!("no BASH_PROGRAM line\nstdout: {stdout}\nstderr: {stderr}"))
        .to_string()
}

/// A `bash` that lives under a `System32` component (any letter case) is
/// the WSL launcher, not a shell. The resolver must skip it even when it
/// leads PATH, and must report nothing when it is the only candidate. The
/// decoy is a real PATH hit: a plain lookup picks it first. Unix only: the
/// decoy needs an executable bit, which `which` on Windows reads as PATHEXT.
#[cfg(unix)]
#[test]
fn bash_program_skips_a_system32_launcher_that_leads_path() {
    use std::os::unix::fs::PermissionsExt;
    if tools_missing() {
        return;
    }
    let fx = fixture_dir("system32");
    let decoy_dir = fx.0.join("Windows").join("SYSTEM32");
    fs::create_dir_all(&decoy_dir).unwrap();
    let decoy = decoy_dir.join("bash");
    fs::write(
        &decoy,
        "#!/bin/sh\necho 'Windows Subsystem for Linux has no installed distributions.'\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).unwrap();

    let real_path = env::var("PATH").unwrap();
    let expected = bash_program().unwrap();
    let led_by_decoy = format!("{}:{real_path}", decoy_dir.display());

    // Control: a plain PATH lookup takes the decoy, so skipping it is the
    // resolver's doing.
    let plain = which::which_in("bash", Some(&led_by_decoy), env::current_dir().unwrap()).unwrap();
    assert_eq!(plain, decoy, "the decoy must lead a plain PATH lookup");

    let chosen = resolver_answer(&led_by_decoy);
    assert_eq!(
        Path::new(&chosen),
        expected,
        "the resolver must step over the System32 entry to the real bash"
    );
    assert!(
        !Path::new(&chosen)
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("system32")),
        "{chosen}"
    );

    let only_decoy = decoy_dir.display().to_string();
    assert_eq!(
        resolver_answer(&only_decoy),
        "none",
        "with only the launcher on PATH the harness must skip, not spawn it"
    );
}

/// The two engineered fail-open edges stay as they were and the six
/// routing gates each draw a "script crashed" edge in their README: five
/// into end_failure, and coder's route_complexity into gate_approval, so a
/// reader of the diagram sees where each crash lands.
#[test]
fn readme_diagrams_draw_the_fail_closed_gate_edges_and_keep_the_open_ones() {
    for (agent, crash_edges, open_edges) in [
        (
            "step-runner",
            &[
                ("route_staleness", "end_failure"),
                ("route_sweep", "end_failure"),
                ("route_review", "end_failure"),
                ("debt_scan", "edge_case_sweep"),
            ][..],
            &[
                "independent_review -. \"2 attempts failed\" .-> write_handoff",
                "edge_case_sweep -. \"LLM fault\" .-> write_handoff",
                "verify_tests -->|\"pass\"| debt_scan",
                "debt_scan --> edge_case_sweep",
            ][..],
        ),
        (
            "coder",
            &[
                ("route_complexity", "gate_approval"),
                ("route_review_result", "end_failure"),
                ("self_review_fault", "end_failure"),
            ][..],
            &[][..],
        ),
    ] {
        let readme = fs::read_to_string(asset(&format!("{agent}/README.md"))).unwrap();
        for (gate, target) in crash_edges {
            let edge = format!("{gate} -. \"script crashed\" .-> {target}");
            assert!(
                readme.lines().any(|l| l.trim() == edge),
                "{agent} README lacks the mermaid edge `{edge}`"
            );
        }
        for edge in open_edges {
            assert!(
                readme.lines().any(|l| l.trim() == *edge),
                "{agent} README lost the deliberate fail-open edge `{edge}`"
            );
        }
        assert!(
            readme.contains("Last script error:"),
            "{agent} README must tell the reader where a crashed gate's error surfaces"
        );
    }
}

/// A `mktemp -d` for the front of PATH that hands out a directory nobody can
/// write into and records its path, so the spill-directory step succeeds
/// and only the document writes can fail.
#[cfg(unix)]
fn mktemp_with_read_only_dirs(dir: &Path, record: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let real = which::which("mktemp").expect("mktemp on PATH");
    let bin = dir.join("mktemp-ro-bin");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("mktemp");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = -d ]; then\n    d=$('{real}' \"$@\") || exit 1\n    chmod 555 \"$d\" && printf '%s\\n' \"$d\" > '{record}' && printf '%s\\n' \"$d\"\n    exit $?\n  fi\ndone\nexec '{real}' \"$@\"\n",
            real = real.display(),
            record = record.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// When a spill write fails (here: the spill directory is read-only, the
/// shape of a full or read-only TMPDIR), resolve_step.sh stops before jq
/// with exit 1, an empty stdout and a stderr line naming the file it could
/// not write, and the EXIT trap still removes the spill directory. A
/// truncated document must never be read back as a valid, shorter plan.
/// Unix only: the failure is induced through file modes, which root
/// ignores, so the test states its reason and returns under uid 0.
#[cfg(unix)]
#[test]
fn a_failed_spill_write_stops_resolve_step_before_jq_and_names_the_file() {
    if tools_missing() {
        return;
    }
    let uid = Command::new("id").arg("-u").output().expect("id -u");
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        // Root writes through a 555 directory, so the induced failure
        // cannot happen; the mode-based shape has no root equivalent.
        eprintln!("skipping: running as root, read-only directories do not refuse writes");
        return;
    }
    let dir = fixture_dir("spill-write-fails");
    one_step_project(&dir.0);
    let plan_path = dir.0.join("plans/steps/01-first.md");
    let plan_before = fs::read(&plan_path).unwrap();
    let record = dir.0.join("spill-dir.txt");
    let shim_bin = mktemp_with_read_only_dirs(&dir.0, &record);
    let path = format!(
        "{}:{}",
        shim_bin.display(),
        env::var("PATH").unwrap_or_default()
    );
    let tmp = dir.0.join("private-tmp");
    fs::create_dir_all(&tmp).unwrap();

    let (code, stdout, stderr) = run_script(
        "step-runner/scripts/resolve_step.sh",
        &dir.0,
        &serde_json::json!({}),
        &[
            ("PATH", path.as_str()),
            ("TMPDIR", tmp.to_str().unwrap()),
            ("LLM_AGENT_VAR_PROJECT_DIR", dir.0.to_str().unwrap()),
        ],
    );
    let spill = fs::read_to_string(&record)
        .expect("the mktemp shim recorded the spill directory")
        .trim()
        .to_string();
    assert_eq!(code, 1, "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        stdout, "",
        "resolve_step.sh must not emit JSON after a failed spill write"
    );
    let expected = format!("resolve_step.sh: write to {spill}/sp failed");
    assert!(
        stderr.lines().any(|line| line == expected),
        "stderr must carry {expected:?}, got:\n{stderr}"
    );
    assert!(
        !Path::new(&spill).exists(),
        "the EXIT trap must remove the spill directory {spill} on the failure route too"
    );
    assert_eq!(
        entries_in(&tmp),
        Vec::<String>::new(),
        "resolve_step left files in TMPDIR after the failed write"
    );
    // The spill block runs before the plan file is rewritten to
    // `status: in-progress`, so the step is left exactly as it was found.
    assert_eq!(
        fs::read(&plan_path).unwrap(),
        plan_before,
        "a failed spill write must leave the step plan file untouched (no status rewrite)"
    );
}

/// The lines of the markdown bullet that starts with `prefix`, including
/// its indented continuation lines.
fn bullet_lines<'a>(doc: &'a str, prefix: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = doc
        .lines()
        .skip_while(|l| !l.starts_with(prefix))
        .take_while(|l| l.starts_with(prefix) || l.starts_with("  "))
        .collect();
    assert!(!lines.is_empty(), "no bullet starting with {prefix:?}");
    lines
}

/// CODER_FAILED now has three causes (fix budget spent, verification gate
/// crashed twice, routing gate crashed), so the step-runner's
/// blocking_reason for it is cause-neutral: the same label for every
/// cause-shaped coder report, never the old fix-loop wording. The sisyphus
/// instructions that read that result name all three causes.
#[test]
fn step_runner_route_coder_result_failed_label_is_cause_neutral_and_sisyphus_names_the_causes() {
    if tools_missing() {
        return;
    }
    let dir = fixture_dir("route-coder-failed-causes");
    let mut labels = Vec::new();
    for body in [
        "fix-loop exhausted; build/tests still failing\n\nCODER_FAILED",
        "Verification gate error: script 'verify_tests' failed: x\n\nCODER_FAILED",
        "Last script error: script 'route_review_result' failed: y\n\nCODER_FAILED",
        "CODER_FAILED",
    ] {
        let out = run_script_json(
            "step-runner/scripts/route_coder_result.sh",
            &dir.0,
            &serde_json::json!({
                "coder_result": body,
                "gate_error": VERIFY_CRASH,
                "last_script_error": VERIFY_CRASH,
            }),
            &[],
        );
        assert_eq!(str_field(&out, "_next"), "end_failure", "{out}");
        let label = str_field(&out, "blocking_reason").to_string();
        assert_eq!(label, "coder failed; see coder result", "{out}");
        assert!(
            !label.contains("fix-loop") && !label.contains("exhausted"),
            "label names one cause for a three-cause outcome: {label}"
        );
        labels.push(label);
    }
    labels.dedup();
    assert_eq!(
        labels.len(),
        1,
        "the label must not vary by cause: {labels:?}"
    );

    let sisyphus = fs::read_to_string(asset("sisyphus/instructions.md")).unwrap();
    let bullet = bullet_lines(&sisyphus, "- `CODER_FAILED`")
        .join(" ")
        .to_lowercase();
    assert!(
        bullet.contains("budget") || bullet.contains("exhausted"),
        "sisyphus CODER_FAILED bullet must name the spent fix budget: {bullet}"
    );
    assert!(
        bullet.contains("verification gate") && bullet.contains("crashed"),
        "sisyphus CODER_FAILED bullet must name the verification-gate crash: {bullet}"
    );
    assert!(
        bullet.contains("routing") && bullet.contains("crash"),
        "sisyphus CODER_FAILED bullet must name the routing-gate crash: {bullet}"
    );
    // The session-continuity rule is the twin an orchestrator reads first;
    // it must not send a crashed gate back to the coder as a fix prompt.
    let continuity = bullet_lines(&sisyphus, "- Coder returned `CODER_FAILED`").join(" ");
    assert!(
        continuity.contains("Last script error:")
            && continuity.to_lowercase().contains("crashed gate")
            && continuity.contains("surface"),
        "sisyphus session-continuity rule must carve out a crashed gate: {continuity}"
    );
    let step = fs::read_to_string(asset("step-runner/README.md")).unwrap();
    let failed = bullet_lines(&step, "- `STEP_FAILED`").join(" ");
    assert!(
        failed.contains("Last script error:") && failed.to_lowercase().contains("crash"),
        "STEP_FAILED bullet must name the routing-gate crash and `Last script error:`: {failed}"
    );
}

/// verify_format_lint.sh writes the format transcript to a spill file for
/// `--rawfile`. When that write fails (a read-only spill directory stands
/// in for a full or read-only TMPDIR) the gate must stop before any result
/// jq runs: exit 1, an empty stdout and a stderr line naming the file it
/// could not write, with the EXIT trap still removing the spill directory.
/// A truncated transcript handed to jq would otherwise pass as a shorter,
/// valid gate result. Both the no-lint-command branch and the lint branch
/// sit behind the same write, so each is driven. Unix only: the failure is
/// induced through file modes, which root ignores.
#[cfg(unix)]
#[test]
fn a_failed_spill_write_stops_verify_format_lint_before_jq_and_names_the_file() {
    if tools_missing() {
        return;
    }
    let uid = Command::new("id").arg("-u").output().expect("id -u");
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        // Root writes through a 555 directory, so the induced failure
        // cannot happen; the mode-based shape has no root equivalent.
        eprintln!("skipping: running as root, read-only directories do not refuse writes");
        return;
    }
    let dir = fixture_dir("vfl-spill-write-fails");
    let tmp = dir.0.join("private-tmp");
    fs::create_dir_all(&tmp).unwrap();

    for (label, extra) in [
        ("no-lint-command", vec![("FORMAT_CMD", "echo formatted")]),
        (
            "lint-green",
            vec![
                ("FORMAT_CMD", "echo formatted"),
                ("LINT_CMD", "echo linted"),
            ],
        ),
        (
            "lint-red",
            vec![
                ("FORMAT_CMD", "echo formatted"),
                ("LINT_CMD", "echo lint failed; exit 2"),
            ],
        ),
    ] {
        let record = dir.0.join(format!("spill-dir-{label}.txt"));
        let shim_bin = mktemp_with_read_only_dirs(&dir.0.join(label), &record);
        let path = format!(
            "{}:{}",
            shim_bin.display(),
            env::var("PATH").unwrap_or_default()
        );
        let mut envs = vec![("PATH", path.as_str()), ("TMPDIR", tmp.to_str().unwrap())];
        envs.extend(extra);
        let (code, stdout, stderr) = run_script(
            "step-runner/scripts/verify_format_lint.sh",
            &dir.0,
            &serde_json::json!({ "project_dir": dir.0.to_str().unwrap() }),
            &envs,
        );
        let spill = fs::read_to_string(&record)
            .unwrap_or_else(|e| {
                panic!("[{label}] the mktemp shim recorded the spill directory: {e}")
            })
            .trim()
            .to_string();
        assert_eq!(code, 1, "[{label}] stdout: {stdout}\nstderr: {stderr}");
        assert_eq!(
            stdout, "",
            "[{label}] verify_format_lint.sh must not emit JSON after a failed spill write"
        );
        let expected = format!("verify_format_lint.sh: write to {spill}/fo failed");
        assert!(
            stderr.lines().any(|line| line == expected),
            "[{label}] stderr must carry {expected:?}, got:\n{stderr}"
        );
        assert!(
            !stderr.contains("Could not open"),
            "[{label}] jq ran against the unwritten spill file:\n{stderr}"
        );
        assert!(
            !Path::new(&spill).exists(),
            "[{label}] the EXIT trap must remove the spill directory {spill} on the failure route too"
        );
        assert_eq!(
            entries_in(&tmp),
            Vec::<String>::new(),
            "[{label}] verify_format_lint left files in TMPDIR after the failed write"
        );
    }
}

/// Bash joins adjacent quoted and unquoted segments into one word, so
/// `--arg x "$short"$big` or `--arg x "x""$(cat big)"` hands jq the whole
/// expansion while a scan that stopped at the first closing quote would
/// only read the bounded head. Every segment of the operand word is read,
/// so these shapes are refused like their single-segment forms. The
/// accepted shapes stay accepted when the quote merely closes the word.
#[test]
fn jq_argv_pin_reads_every_segment_of_a_concatenated_operand() {
    let rel = "step-runner/scripts/resolve_step.sh";
    let slipped: Vec<&str> = [
        (
            "allow-listed head, bare tail",
            r#"jq -nc --arg r "$project_dir"$big '{}'"#,
        ),
        (
            "allow-listed head, quoted tail",
            r#"jq -nc --arg r "$project_dir""$big" '{}'"#,
        ),
        (
            "literal head, command substitution tail",
            r#"jq -nc --arg r "x""$(cat report)" '{}'"#,
        ),
        (
            "literal head, indirect expansion tail",
            r#"jq -nc --arg r "x"${!ref} '{}'"#,
        ),
        (
            "empty head, process substitution tail",
            r#"jq -nc --rawfile x ""<(printf '%s' "$big") '{}'"#,
        ),
        (
            "argjson with a quoted tail",
            r#"jq -nc --argjson n "$step_number""$big" '{}'"#,
        ),
    ]
    .into_iter()
    .filter(|(_, script)| jq_argv_violations(rel, script).is_empty())
    .map(|(label, _)| label)
    .collect();
    assert!(slipped.is_empty(), "slipped past the pin: {slipped:?}");

    for (label, script) in [
        (
            "two allow-listed segments",
            r#"jq -nc --arg r "$plans_dir/""$step_slug" '{}'"#,
        ),
        (
            "literal tail after an allow-listed head",
            r#"jq -nc --arg r "$project_dir"/plans '{}'"#,
        ),
        (
            "closing quote followed by the next flag",
            r#"jq -nc --arg r "$project_dir" --arg s "$step_slug" '{}'"#,
        ),
    ] {
        let found = jq_argv_violations(rel, script);
        assert!(found.is_empty(), "{label} was flagged: {found:?}");
    }
}

/// The operand grammar past the six concatenated shapes: single-quoted
/// segments (literal in bash), punctuation that merely closes the word,
/// `$'...'` quoting, a lone or escaped `$`, tabs and continuations around
/// the name, an unterminated quote at end of file, multibyte text, and a
/// comment-looking line inside a multi-line quoted value. Every row either
/// hands jq an unbounded expansion (refused) or provably does not
/// (accepted); none may panic the scan.
#[test]
fn jq_argv_pin_holds_at_the_edges_of_the_operand_grammar() {
    let rel = "step-runner/scripts/resolve_step.sh";
    let slipped: Vec<&str> = [
        (
            "ansi-c quoted tail",
            "jq -nc --arg r \"$project_dir\"$'\\n' '{}'",
        ),
        (
            "escaped dollar tail",
            r#"jq -nc --arg r "$project_dir"\$big '{}'"#,
        ),
        (
            "lone trailing dollar",
            r#"jq -nc --arg r "$project_dir"$ '{}'"#,
        ),
        ("tab-separated big", "jq -nc --arg\tr\t\"$big\" '{}'"),
        (
            "continuation after the name",
            "jq -nc --arg r\\\n  \"$big\" '{}'",
        ),
        ("unterminated quote at eof", r#"jq -nc --arg r "$big"#),
        (
            "escaped quote keeps the segment open",
            r#"jq -nc --arg r "a\"$big" '{}'"#,
        ),
        (
            "escaped backslash closes the segment, bare tail",
            r#"jq -nc --arg r "a\\"$big '{}'"#,
        ),
        (
            "multibyte head",
            "jq -nc --arg r \"\u{e9}\u{1F534}$big\" '{}'",
        ),
        (
            "comment-looking line inside a multi-line value",
            "jq -nc --arg r \"\n# $big\n\" '{}'",
        ),
        (
            "rawfile under a quoted spill dir then /proc",
            r#"jq -nc --rawfile x "$spill"/proc/self/fd/3 '{}'"#,
        ),
        (
            "rawfile on a quoted /dev path",
            r#"jq -nc --rawfile x "/dev/stdin" '{}'"#,
        ),
        (
            "slurpfile on /proc",
            r#"jq -nc --slurpfile x /proc/self/fd/0 '{}'"#,
        ),
        ("jsonargs", r#"jq -nc '$ARGS' --jsonargs "$big""#),
        // Bash deletes a backslash-newline glued to the flag, so `--args\<nl>`
        // followed by a blank is the flag `--args` and the operands on the
        // next line are positional: the flag check must see through the
        // continuation the way the name and value readers do.
        (
            "args flag with a continuation glued to it",
            "jq -n '$ARGS' --args\\\n \"$big\"",
        ),
        (
            "jsonargs flag with a continuation glued to it",
            "jq -n '$ARGS' --jsonargs\\\n 7",
        ),
        // Adjacent quoted and bare segments are one word to bash, so a device
        // path split by an empty or partial quote (`/d"ev"/stdin`) is still
        // `/dev/stdin` on jq's argv; the path check reads the joined word.
        (
            "rawfile on a device path split by a quoted segment",
            r#"jq -nc --rawfile x /d"ev"/stdin '{}'"#,
        ),
        (
            "rawfile on a device path led by a single-quoted segment",
            r#"jq -nc --rawfile x '/d'ev/fd/0 '{}'"#,
        ),
        (
            "slurpfile on a device path split by empty quotes",
            r#"jq -nc --slurpfile x /de''v/stdin '{}'"#,
        ),
        // A backslash outside single quotes escapes the next byte and is
        // deleted, so `/de\v/stdin` and `/dev\/stdin` are `/dev/stdin` on
        // jq's argv just as a quote-split path is.
        (
            "rawfile on a device path with a backslash-escaped byte",
            r#"jq -nc --rawfile x /de\v/stdin '{}'"#,
        ),
        (
            "rawfile on a device path with a backslash-escaped slash",
            r#"jq -nc --rawfile x /dev\/stdin '{}'"#,
        ),
        (
            "slurpfile on a proc path with a backslash-escaped byte",
            r#"jq -nc --slurpfile x /pro\c/self/fd/0 '{}'"#,
        ),
        // Quotes and backslashes mixed in one path word, and an ANSI-C
        // path: bash hands jq `/dev/stdin` in every case.
        (
            "rawfile on a device path mixing escaped quotes",
            r#"jq -nc --rawfile x /d\"e\"v/stdin '{}'"#,
        ),
        (
            "rawfile on an ansi-c device path",
            r#"jq -nc --rawfile x $'/dev/stdin' '{}'"#,
        ),
        (
            "rawfile on a device path with a doubled slash",
            r#"jq -nc --rawfile x /dev//stdin '{}'"#,
        ),
        // The name is a shell word like any other: jq binds whatever bash
        // hands it (`$ARGS.named["-r"]`), so a name that is not a plain
        // identifier must not make the reader take the name for the value
        // and skip the expansion that follows it.
        ("dash-led name", r#"jq -nc --arg -r "$big" '{}'"#),
        ("dot-led name", r#"jq -nc --arg .r "$big" '{}'"#),
        (
            "name with a trailing dash",
            r#"jq -nc --arg r- "$big" '{}'"#,
        ),
        (
            "argjson with a dash-led name",
            r#"jq -nc --argjson -n "$big" '{}'"#,
        ),
        (
            "rawfile with a dash-led name on a process substitution",
            r#"jq -nc --rawfile -n <(printf x) '{}'"#,
        ),
        (
            "carriage return then hash leads the name",
            "jq -nc --arg \r#r \"$big\" '{}'",
        ),
        // The expansion can land in the NAME word too (bash joins
        // `r\<nl>"$big"` into one word); the name is on argv all the same.
        (
            "continuation glues the expansion into the name",
            "jq -nc --arg r\\\n\"$big\" '{}'",
        ),
        ("the name is the expansion", r#"jq -nc --arg "$big" x '{}'"#),
    ]
    .into_iter()
    .filter(|(_, script)| jq_argv_violations(rel, script).is_empty())
    .map(|(label, _)| label)
    .collect();
    assert!(slipped.is_empty(), "slipped past the pin: {slipped:?}");

    for (label, script) in [
        (
            "closing paren after the word",
            r#"x=$(jq -nc --arg r "$project_dir") '{}'"#,
        ),
        (
            "semicolon after the word",
            r#"jq -nc --arg r "$project_dir"; true"#,
        ),
        (
            "pipe after the word",
            r#"jq -nc --arg r "$project_dir"| cat"#,
        ),
        (
            "tab-separated allow-listed",
            "jq -nc --arg\tr\t\"$project_dir\" '{}'",
        ),
        (
            "continuation after the name",
            "jq -nc --arg r\\\n  \"$project_dir\" '{}'",
        ),
        (
            "continuation right after the closing quote",
            "jq -nc --arg r \"$project_dir\"\\\n  --arg s \"$step_slug\" '{}'",
        ),
        (
            "empty single-quoted tail",
            r#"jq -nc --arg r "$project_dir"'' '{}'"#,
        ),
        (
            "single-quoted literal",
            r#"jq -nc --arg r 'fixed text' '{}'"#,
        ),
        (
            "length expansion is a small integer",
            r#"jq -nc --arg r "${#big}" '{}'"#,
        ),
        (
            "double-quoted name",
            r#"jq -nc --arg "r" "$project_dir" '{}'"#,
        ),
        (
            "single-quoted name",
            r#"jq -nc --arg 'r' "$project_dir" '{}'"#,
        ),
    ] {
        let found = jq_argv_violations(rel, script);
        assert!(found.is_empty(), "{label} was flagged: {found:?}");
    }

    // A single-quoted segment is literal in bash; the scan may read it
    // conservatively, but it must not panic and must still refuse the
    // unbounded double-quoted segment beside it.
    let found = jq_argv_violations(rel, r#"jq -nc --arg r "$big"'$(cmd)' '{}'"#);
    assert!(
        !found.is_empty(),
        "single-quoted tail hid the unbounded head"
    );
}

/// The comment strip tracks quote state the way bash does: an escaped quote
/// keeps a multi-line value open, an escaped backslash does not, nothing is
/// special inside single quotes, a trailing comment ends the line's quote
/// bookkeeping, and a `#` glued to a word is text. A backslash-newline is
/// deleted by bash's lexer, so the characters on both sides join into ONE
/// word unless the next line starts with a blank; the word reader must keep
/// reading across the pair exactly when bash does.
#[test]
fn jq_argv_pin_tracks_quotes_across_lines_and_continuations() {
    let rel = "step-runner/scripts/resolve_step.sh";
    let slipped: Vec<&str> = [
        (
            "continuation then a bare expansion joins the word",
            "jq -nc --arg r \"$project_dir\"\\\n$big '{}'",
        ),
        (
            "continuation then a quoted expansion joins the word",
            "jq -nc --arg r \"$project_dir\"\\\n\"$big\" '{}'",
        ),
        (
            "escaped quote keeps a multi-line value open across a hash line",
            "jq -nc --arg r \"a \\\" b\n# $big\n\" '{}'",
        ),
        (
            "hash glued to the word is text, not a comment",
            "jq -nc --arg r \"$project_dir\"#\"\n$big\n\" '{}'",
        ),
        (
            "continuation glues a hash line onto the word",
            "jq -nc --arg r \"$project_dir\"\\\n#$big\n'{}'",
        ),
        (
            "continuation between the flag and its name still names a flag",
            "jq -nc --arg\\\n r \"$big\" '{}'",
        ),
        (
            "two continuations in a row still join one word",
            "jq -nc --arg r \"$project_dir\"\\\n\\\n$big '{}'",
        ),
        // Only space, tab and newline end a word in bash; a carriage return
        // or form feed is an ordinary word character, so the bare segment
        // after it is still part of the operand bash hands jq.
        (
            "carriage return inside the word is not a word break",
            "jq -nc --arg r \"$project_dir\"\r$big '{}'",
        ),
        (
            "continuation then a form feed joins the word",
            "jq -nc --arg r \"$project_dir\"\\\n\x0c$big '{}'",
        ),
        // The same blank set decides whether a continuation glues the next
        // line's `#` onto the word: bash deletes the pair and the `#` starts
        // a comment only when a space or tab precedes it. A carriage return,
        // form feed or non-breaking space is a word byte there too, so the
        // hash line is operand text, not prose.
        (
            "a carriage return before the continuation still glues the hash line",
            "jq -nc --arg r \"$project_dir\"\r\\\n#$big\n'{}'",
        ),
        (
            "a form feed before the continuation still glues the hash line",
            "jq -nc --arg r \"$project_dir\"\x0c\\\n#$big\n'{}'",
        ),
        (
            "a non-breaking space before the continuation still glues the hash line",
            "jq -nc --arg r \"$project_dir\"\u{a0}\\\n#$big\n'{}'",
        ),
        // Two deleted pairs in a row leave `"$project_dir"#$big` as one word,
        // so a line holding only the continuation backslash carries the glue.
        (
            "a bare continuation line carries the glue onto the hash line",
            "jq -nc --arg r \"$project_dir\"\\\n\\\n#$big\n'{}'",
        ),
        (
            "an escaped backslash then a continuation still glues the hash line",
            "jq -nc --arg r \"$project_dir\"\\\\\\\n#$big\n'{}'",
        ),
        // The hash line can itself end in a continuation, or open a quote
        // the next hash line closes: bash deletes each pair and the whole
        // run is one word (`"$project_dir"##$big`), so every line of it is
        // operand text.
        (
            "a glued hash line that itself continues glues the next hash line",
            "jq -nc --arg r \"$project_dir\"\\\n#\\\n#$big\n'{}'",
        ),
        (
            "a glued hash line that opens a quote keeps the next hash line",
            "jq -nc --arg r \"$project_dir\"\\\n#\"\\\n#$big\n\" '{}'",
        ),
        // Leading bytes outside bash's blank set do not make a line a
        // comment: `\r#$big` is a word, and after `--arg r \<nl>` bash hands
        // it to jq as the value. The line is live even when another line
        // follows, so the dangling backslash is not what the pin may rely on.
        (
            "a carriage return then a hash leads the value line",
            "jq -nc --arg r \\\n\r#$big '{}'\necho done\n",
        ),
        (
            "a non-breaking space then a hash leads the value line",
            "jq -nc --arg r \\\n\u{a0}#$big '{}'\necho done\n",
        ),
        (
            "a form feed then a hash leads the name line",
            "jq -nc --arg \\\n\x0c#r \"$big\" '{}'\necho done\n",
        ),
        // A quote character is not always a quote to bash: `\'` inside
        // `$'...'` is an escape, a heredoc body is data, and the quotes inside
        // `$( )` nest independently of the quotes around it. If the strip
        // counts those, its quote parity drifts from bash's and the
        // `#`-leading line of a LATER quoted operand is dropped as a comment
        // while bash hands it to jq with the expansion glued on. The quote
        // bookkeeping must not drift on these everyday idioms.
        (
            "an ansi-c string with an escaped quote does not drift the parity",
            "x=$'a\\'b'\njq -nc --arg r '\n#'\"$big\"'\n' '{}'\n",
        ),
        (
            "a heredoc body with an odd apostrophe does not drift the parity",
            "cat <<EOF >/dev/null\nit's\nEOF\njq -nc --arg r '\n#'\"$big\"'\n' '{}'\n",
        ),
        (
            "a heredoc body with an odd double quote does not drift the parity",
            "cat <<EOF >/dev/null\nsay \"hi\nEOF\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        (
            "an odd quote nested in a command substitution does not drift the parity",
            "y=\"$(echo \"a'b\")\"\njq -nc --arg r '\n#'\"$big\"'\n' '{}'\n",
        ),
        // Bash recognises `$'...'` only outside double quotes: inside `"..."`
        // the `$` is a dollar and the `'` is a character, so the `"` that
        // follows still closes. Treating it as ANSI-C swallows that closing
        // quote and the strip's parity drifts the other way: the `#`-led line
        // of the SAME operand is dropped as a comment while bash joins
        // `"$'"` + `'x'` + `"<nl>#$big<nl>"` into one value word.
        (
            "a dollar-quote inside double quotes is not an ansi-c string",
            "jq -nc --arg r \"$'\"'x'\"\n#$big\n\" '{}'\n",
        ),
        // A subshell or grouping paren inside `$( )` is ordinary bash grammar
        // (`out=$( (cd "$d" && make) 2>&1 )`): its `)` closes the group, not
        // the substitution, so the saved quote state must not be restored
        // there. Restoring it early leaves the parity inverted for the rest
        // of the line and the LATER operand's `#`-led line is dropped.
        (
            "a subshell inside a command substitution does not end the substitution early",
            "y=\"$( (echo a) && echo \"it's\" )\"\njq -nc --arg r '\n#'\"$big\"'\n' '{}'\n",
        ),
        (
            "a grouping paren inside a bare command substitution does not end it early",
            "y=$( (cd /) && echo 'it\"s' )\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        // `$( )` nested inside backticks: its `)` closes the inner frame, so
        // the backtick that follows must restore the state saved at the
        // backtick, not the one saved at `$(`. Otherwise the outer `"` is
        // read as an opener and the LATER operand's `#`-led line is dropped.
        (
            "a command substitution inside backticks restores the outer quote state",
            "y=\"`echo $(echo a) b`\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        // The reverse nesting: a backtick opened inside `$( )` must close
        // its own frame, not the one saved at `$(`, or the outer `"` is
        // left open and the LATER operand's `#`-led line is dropped.
        (
            "backticks inside a command substitution restore the outer quote state",
            "y=\"$(echo `date` x)\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        // A paren inside quotes within a frame is a character, not a group
        // opener or closer: `$( echo "(" )` and `$( echo $')' )` are closed
        // by their own `)`. Counting the quoted paren would leave the frame
        // open (or pop it early) and the LATER dq operand's hash line would
        // be dropped.
        (
            "a paren inside double quotes within a command substitution is not a group",
            "y=\"$( echo \"(\" )\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        (
            "a paren inside single quotes within a command substitution is not a group",
            "y=\"$( echo '(' )\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        (
            "a closing paren inside an ansi-c string within a command substitution does not pop",
            "y=\"$( echo $')' )\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        // The group depth is a property of the frame, not of the line: a
        // group opened before a continuation is still open after it.
        (
            "a group depth survives a continuation inside a command substitution",
            "y=\"$( (echo a) \\\n && echo b )\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        // A heredoc body inside a frame is data for the frame too: its `)`
        // and its quotes drive nothing.
        (
            "a heredoc body inside a command substitution does not close it",
            "y=\"$(cat <<EOF\n)\nEOF\n)\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        (
            "an odd quote in a heredoc body inside a command substitution does not drift the parity",
            "y=\"$(cat <<EOF\nit's\nEOF\n)\"\njq -nc --arg r '\n#'\"$big\"'\n' '{}'\n",
        ),
        // Inside a frame, outside quotes, a `#`-leading line is a comment
        // just as it is at the top level: a paren or a quote in it is prose.
        (
            "a comment line inside a command substitution does not count its paren",
            "y=\"$(\n# note (\necho a\n)\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        (
            "a comment line inside a command substitution does not open a quote",
            "y=\"$(\n# it's\necho a\n)\"\njq -nc --arg r '\n#'\"$big\"'\n' '{}'\n",
        ),
        // Three frames deep, each closer pops its own kind.
        (
            "backticks inside a command substitution with an inner substitution pop in order",
            "y=\"$( echo `echo $(echo a)` )\"\njq -nc --arg r \"\n#$big\n\" '{}'\n",
        ),
        // A top-level subshell around the jq call is plain grammar: its
        // parens never touch the quote state.
        (
            "a subshell around the jq call does not hide its operand",
            "( cd /tmp && jq -nc --arg r \"\n#$big\n\" '{}' )\n",
        ),
    ]
    .into_iter()
    .filter(|(_, script)| jq_argv_violations(rel, script).is_empty())
    .map(|(label, _)| label)
    .collect();
    assert!(slipped.is_empty(), "slipped past the pin: {slipped:?}");

    for (label, script) in [
        (
            "a blank before the continuation keeps the hash line a comment",
            "jq -nc --arg r \"$project_dir\" \\\n# --arg x \"$big\"\n'{}'",
        ),
        (
            "escaped backslash closes the quote before a comment line",
            "jq -nc --arg r \"a\\\\\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "a double quote inside single quotes opens nothing",
            "jq -nc --arg r '\"' '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "a backslash inside single quotes is literal",
            "jq -nc --arg r 'a\\' '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "an apostrophe in a trailing comment does not flip parity",
            "jq -nc --arg r \"$project_dir\" '{}' # don't\n# --arg x \"$big\"\n",
        ),
        (
            "a multi-line value closes, then a real comment follows",
            "jq -nc --arg r \"\ntext\n\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "continuation then a literal tail",
            "jq -nc --arg r \"$project_dir\"\\\n'x' '{}'",
        ),
        (
            "continuation then a closing paren",
            "x=$(jq -nc --arg r \"$project_dir\"\\\n)",
        ),
        (
            "continuation then an indented next operand",
            "jq -nc --arg r \"$project_dir\"\\\n\t--arg s \"$step_slug\" '{}'",
        ),
        (
            "continuation between the flag and its name, bounded value",
            "jq -nc --arg\\\n r \"$project_dir\" '{}'",
        ),
        (
            "a tab before the continuation keeps the hash line a comment",
            "jq -nc --arg r \"$project_dir\"\t\\\n#$big\n'{}'",
        ),
        (
            "a blank, then a bare continuation line, keeps the hash line a comment",
            "jq -nc --arg r \"$project_dir\" \\\n\\\n#$big\n'{}'",
        ),
        (
            "a space leading the glued line keeps its hash a comment",
            "jq -nc --arg r \"$project_dir\"\\\n #$big\n'{}'",
        ),
        (
            "a bare continuation line, then a space, keeps the hash line a comment",
            "jq -nc --arg r \"$project_dir\"\\\n\\\n #$big\n'{}'",
        ),
        // A comment ends at its newline whatever it ends in, so its trailing
        // backslash glues nothing onto the next hash line.
        (
            "a comment's own trailing backslash does not glue the next hash line",
            "jq -nc --arg r \"$project_dir\" \\\n# x \\\n#--arg q \"$big\"\n'{}'",
        ),
        // The mirror of the two drift rows above: bash is balanced after
        // `x="$'"` and after `$( (a) && b )`, so the hash line that follows
        // each is a comment.
        (
            "a dollar then a quote inside double quotes closes normally",
            "x=\"$'\"\njq -nc --arg r \"$project_dir\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "a subshell inside a command substitution leaves the parity balanced",
            "y=\"$( (echo a) && echo b )\"\njq -nc --arg r \"$project_dir\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "a quoted paren inside a command substitution leaves the parity balanced",
            "y=\"$( echo \"(\" )\"\njq -nc --arg r \"$project_dir\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "a heredoc inside a command substitution leaves the parity balanced",
            "y=\"$(cat <<EOF\n)\nEOF\n)\"\njq -nc --arg r \"$project_dir\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "a comment line inside a command substitution leaves the parity balanced",
            "y=\"$(\n# note (\necho a\n)\"\njq -nc --arg r \"$project_dir\" '{}'\n# --arg x \"$big\"\n",
        ),
        (
            "three frames deep leave the parity balanced",
            "y=\"$( echo `echo $(echo a)` )\"\njq -nc --arg r \"$project_dir\" '{}'\n# --arg x \"$big\"\n",
        ),
    ] {
        let found = jq_argv_violations(rel, script);
        assert!(found.is_empty(), "{label} was flagged: {found:?}");
    }

    // A single-quoted multi-line value is literal in bash; the scan may read
    // it either way but must not panic on the hash line inside it.
    let _ = jq_argv_violations(rel, "jq -nc --arg r '\n# $big\n' '{}'");

    // The ground truth the slipped rows rest on, taken from the bash that
    // runs the scripts: the pair is deleted and the two sides become one
    // word, two pairs in a row likewise, a pair between the flag and its
    // name leaves the flag intact, a carriage return never ends a word, and
    // a `#` after a deleted pair is a comment only when a blank (not a CR,
    // not a non-breaking space, not a bare continuation line) came before it;
    // a CR-led `#` line is a word even as the value, a glued hash line that
    // continues glues the next, and a comment's trailing backslash glues nothing.
    // A pair glued to `--args` leaves the flag intact, a device path split by
    // quotes is still the device path, and a `#`-led line inside a quoted
    // operand stays operand text after an ANSI-C escape, a heredoc body or a
    // nested command substitution with an odd quote; `$'` inside double
    // quotes is two characters, a subshell inside `$( )` does not end it,
    // a backslash-escaped byte in a bare path is deleted, and `$( )` inside
    // backticks inside `"..."` leaves the outer quote where bash leaves it.
    if tools_missing() {
        return;
    }
    let dir = env::temp_dir().join(format!(
        "coyote-gate-scripts-join-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let script = dir.join("join.sh");
    // Each case is a script fragment and the words bash hands printf for it.
    let cases: [(&str, &str); 23] = [
        ("printf '<%s>' --arg r \"$a\"\\\n$big", "<--arg><r><ABBBB>"),
        (
            "printf '<%s>' --arg r \"$a\"\\\n  --arg s x",
            "<--arg><r><A><--arg><s><x>",
        ),
        (
            "printf '<%s>' --arg r \"$a\"\\\n\\\n$big",
            "<--arg><r><ABBBB>",
        ),
        ("printf '<%s>' --arg\\\n r \"$big\"", "<--arg><r><BBBB>"),
        ("printf '<%s>' --arg r \"$a\"\r$big", "<--arg><r><A\rBBBB>"),
        (
            "printf '<%s>' --arg r \"$a\"\r\\\n#$big",
            "<--arg><r><A\r#BBBB>",
        ),
        (
            "printf '<%s>' --arg r \"$a\"\u{a0}\\\n#$big",
            "<--arg><r><A\u{a0}#BBBB>",
        ),
        (
            "printf '<%s>' --arg r \"$a\"\\\n\\\n#$big",
            "<--arg><r><A#BBBB>",
        ),
        (
            "printf '<%s>' --arg r \"$a\" \\\n\\\n#$big",
            "<--arg><r><A>",
        ),
        ("printf '<%s>' --arg r \\\n\r#$big", "<--arg><r><\r#BBBB>"),
        (
            "printf '<%s>' --arg r \"$a\"\\\n#\\\n#$big",
            "<--arg><r><A##BBBB>",
        ),
        (
            "printf '<%s>' --arg r \"$a\" \\\n# x \\\n#--arg q \"$big\"",
            "<--arg><r><A>",
        ),
        ("printf '<%s>' --arg -r \"$big\"", "<--arg><-r><BBBB>"),
        ("printf '<%s>' --args\\\n \"$big\"", "<--args><BBBB>"),
        (
            "printf '<%s>' /d\"ev\"/stdin '/d'ev/fd/0",
            "</dev/stdin></dev/fd/0>",
        ),
        (
            "x=$'a\\'b'\nprintf '<%s>' --arg r '\n#'\"$big\"'\n'",
            "<--arg><r><\n#BBBB\n>",
        ),
        (
            "cat <<EOF >/dev/null\nit's\nEOF\nprintf '<%s>' --arg r '\n#'\"$big\"'\n'",
            "<--arg><r><\n#BBBB\n>",
        ),
        (
            "y=\"$(echo \"a'b\")\"\nprintf '<%s>' --arg r \"\n#$big\n\"",
            "<--arg><r><\n#BBBB\n>",
        ),
        (
            "printf '<%s>' --arg r \"$'\"'x'\"\n#$big\n\"",
            "<--arg><r><$'x\n#BBBB\n>",
        ),
        (
            "y=\"$( (echo a) && echo \"it's\" )\"\nprintf '<%s>' --arg r '\n#'\"$big\"'\n'",
            "<--arg><r><\n#BBBB\n>",
        ),
        (
            "printf '<%s>' /de\\v/stdin /dev\\/stdin",
            "</dev/stdin></dev/stdin>",
        ),
        (
            "y=\"`echo $(echo a) b`\"\nprintf '<%s>' --arg r \"\n#$big\n\"",
            "<--arg><r><\n#BBBB\n>",
        ),
        (
            "y=\"$(echo `date` x)\"\nprintf '<%s>' --arg r \"\n#$big\n\"",
            "<--arg><r><\n#BBBB\n>",
        ),
    ];
    // Git Bash deletes CR bytes while reading a script, so a CR inside a word
    // never reaches it there; those cases are unix ground truth only.
    let cases = cases
        .iter()
        .filter(|(fragment, _)| !(cfg!(windows) && fragment.contains('\r')));
    let mut source = String::from("a=A\nbig=BBBB\n");
    let mut expected = String::new();
    for (fragment, words) in cases {
        source.push_str(fragment);
        source.push('\n');
        expected.push_str(words);
    }
    fs::write(&script, source).unwrap();
    let (code, stdout, _) = run(bash().arg(&script).output().expect("spawn bash"));
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(code, 0);
    assert_eq!(
        stdout, expected,
        "bash did not join the continued word the way the pin assumes"
    );
}
