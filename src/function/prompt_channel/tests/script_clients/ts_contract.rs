use super::*;

/// A driver in `dir` that prints `expr` through `stringify`,
/// importing the helper from a copy beside it: under the repo
/// the helper sits below a package.json without a `type` field,
/// and node warns about that on stderr, which the contract
/// requires empty.
fn ts_command(runtime: &str, dir: &Path, expr: &str, stringify: &str) -> Command {
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(
        &driver,
        format!(
            "import {{ pathToFileURL }} from \"node:url\";\n\
             const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
             console.log(({stringify})(prompt.{expr}));\n"
        ),
    )
    .unwrap();
    let mut cmd = Command::new(runtime);
    cmd.arg(&driver).arg(&helper);
    cmd.env("NODE_NO_WARNINGS", "1");
    cmd
}

fn run_ts(
    runtime: &str,
    dir: &Path,
    expr: &str,
    answers: Option<&[Value]>,
    stray_first: bool,
) -> Run {
    drive(
        ts_command(runtime, dir, expr, "JSON.stringify"),
        answers,
        stray_first,
    )
}

fn run_ts_in(
    runtime: &str,
    dir: &Path,
    expr: &str,
    answers: Option<&[Value]>,
    env: &[(&str, &str)],
    cwd: &Path,
) -> Run {
    // Guards return undefined, which JSON.stringify alone would
    // print as an empty line.
    let mut cmd = ts_command(runtime, dir, expr, "(v) => String(JSON.stringify(v))");
    cmd.current_dir(cwd);
    drive_env(cmd, answers, false, env)
}

#[test]
fn helper_honours_the_channel_contract() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-contract");
    check_contract(
        |expr, answers, stray_first| run_ts(runtime, &dir, expr, answers, stray_first),
        &Contract {
            confirm_true: "true",
            confirm_false: "false",
            none: "null",
            checked: "[0,2]",
            typed: "\"typed by hand\"",
        },
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn guards_mirror_the_bash_contract() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-guards");
    let cwd = scratch_dir("ts-guards-cwd");
    check_guards(
        &|expr, answers, env, cwd| run_ts_in(runtime, &dir, expr, answers, env, cwd),
        "undefined",
        &cwd,
    );
    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&cwd).ok();
}

#[test]
fn replies_of_the_wrong_shape_are_unusable() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-shapes");
    check_unusable_shapes(
        |expr, answers, stray_first| run_ts(runtime, &dir, expr, answers, stray_first),
        "null",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_channel_and_no_terminal_falls_closed_without_a_request() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-no-channel");
    check_no_channel(
        drive_without_channel(ts_command(
            runtime,
            &dir,
            "confirm(\"Go?\")",
            "JSON.stringify",
        )),
        "null",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn request_lines_carry_the_text_verbatim_with_fresh_ids() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-wire");
    check_wire(
        |expr, answers, stray_first| run_ts(runtime, &dir, expr, answers, stray_first),
        "prompt.",
        "\"x\"",
        "1",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn requests_escape_a_bash_tools_capture_through_its_saved_stdout() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    if bash_program().is_none() {
        return;
    }
    let dir = scratch_dir("ts-nested");
    check_nested_in_bash(
        &ts_command(
            runtime,
            &dir,
            "select(\"Pick\", [\"a\", \"b\", \"c\"])",
            "JSON.stringify",
        ),
        "1",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn wire_edges_mirror_the_bash_helper() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-edges");
    check_edges(
        |expr| ts_command(runtime, &dir, expr, "(v) => String(JSON.stringify(v))"),
        "null",
        "true",
        "[]",
        "\"\"",
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn dead_channel_and_answerless_reply_fail_closed() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-probe-edges");
    check_probe_edges(
        |expr| ts_command(runtime, &dir, expr, "(v) => String(JSON.stringify(v))"),
        "null",
        "[0]",
        &dir,
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_directory_as_reply_path_fails_closed_without_a_traceback() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = scratch_dir("ts-directory-reply");
    check_directory_reply(
        |expr| ts_command(runtime, &dir, expr, "(v) => String(JSON.stringify(v))"),
        "null",
        &dir,
    );
    fs::remove_dir_all(&dir).ok();
}

/// The host keeps the channel's write end open, so a helper that
/// waited for EOF would hang: the reply must come through a
/// bounded readSync loop on the opened fd.
#[test]
fn helper_reads_the_reply_with_a_readsync_loop() {
    let source = fs::read_to_string(TS_HELPER).unwrap();
    assert!(
        source.contains("fs.readSync("),
        "reply read is not a readSync loop"
    );
    assert!(
        source.contains("fs.openSync(process.env[REPLY_ENV]"),
        "reply path is not opened from the channel env var"
    );
    let read_file_sync_calls: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//") && line.contains("readFileSync("))
        .collect();
    assert!(
        read_file_sync_calls.is_empty(),
        "the reply must not be read to EOF: {read_file_sync_calls:?}"
    );
}

/// A SIGINT between turning echo off and installing the restore
/// would leave the terminal dark, so the only `stty -echo` lives
/// in the sh child, after its trap, and the parent's stty calls
/// are limited to the snapshot and the restore.
#[test]
fn hidden_read_changes_the_terminal_only_inside_the_trapped_child() {
    let source = fs::read_to_string(TS_HELPER).unwrap();
    let echo_off: Vec<&str> = source
        .lines()
        .filter(|line| line.contains("stty -echo"))
        .collect();
    assert_eq!(
        echo_off.len(),
        1,
        "expected exactly one `stty -echo` line: {echo_off:?}"
    );
    let line = echo_off[0];
    assert!(
        line.contains("trap '") && line.contains("EXIT INT TERM"),
        "`stty -echo` is not on the trapped child line: {line}"
    );
    assert!(
        line.find("trap").unwrap() < line.find("stty -echo").unwrap(),
        "the trap must be installed before echo goes off: {line}"
    );
    for line in source
        .lines()
        .filter(|line| line.contains("execFileSync(\"stty\""))
    {
        assert!(
            line.contains("[\"-g\"]") || line.contains("[mode]"),
            "parent-side stty call carries a literal mode: {line}"
        );
    }
    let parent_echo_off: Vec<&str> = source
        .lines()
        .filter(|line| line.contains("stty(tty, \"-echo\")"))
        .collect();
    assert!(
        parent_echo_off.is_empty(),
        "the parent must not turn echo off itself: {parent_echo_off:?}"
    );
}
