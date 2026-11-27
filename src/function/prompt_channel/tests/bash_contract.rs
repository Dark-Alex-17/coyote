use super::*;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds the hang guards in these bodies allow. A hang is
/// unbounded, so the guard only has to outlast a loaded host's bash
/// start-up; a value near the expected run time turns load into false
/// failures.
const HANG_GUARD_SECS: u64 = 60;

/// A command prefix that kills what it runs after `HANG_GUARD_SECS`.
/// Perl's alarm survives the exec; `timeout` is GNU coreutils and
/// absent from stock macOS.
fn hang_guard() -> String {
    format!("perl -e 'alarm shift; exec @ARGV' {HANG_GUARD_SECS}")
}

fn is_request_line(line: &str) -> bool {
    line.trim_start().as_bytes().starts_with(MARKER)
}

/// A unique scratch directory per test, so `guard_path` has a known pwd.
fn scratch_dir(tag: &str) -> PathBuf {
    // Tests run in parallel and the clock here ticks in microseconds.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "coyote-prompt-utils-{tag}-{}-{nanos}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Run {
    fn request_lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter(|line| is_request_line(line))
            .map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap_or_else(|err| panic!("request line is not JSON ({err}): {line}"))
            })
            .collect()
    }

    fn other_lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| !is_request_line(line))
            .collect()
    }
}

/// Reply lines the harness plants that no request asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stray {
    None,
    /// A foreign-id reply before every real one.
    Before,
    /// A foreign-id reply after every real one: the next helper has
    /// to skip it and the last one must still sit in the pipe when
    /// the tool reads the channel itself.
    After,
    /// A foreign-id reply and a line that is not JSON, then the
    /// channel closes without ever answering the request.
    ThenClose,
}

/// Runs `body` in a bash that sourced the helper with a live reply
/// channel; request lines the shell writes are answered from
/// `answers` in order (`null` once they run out; `None` closes the
/// channel before the first read). `detach` puts the shell in its
/// own session so `/dev/tty` cannot be opened.
fn run_helper(dir: &Path, answers: Option<&[Value]>, body: &str, detach: bool) -> Run {
    run_helper_opts(dir, answers, body, detach, Stray::None)
}

/// `run_helper` with a [`Stray`] policy for unasked-for reply lines.
fn run_helper_opts(
    dir: &Path,
    answers: Option<&[Value]>,
    body: &str,
    detach: bool,
    stray: Stray,
) -> Run {
    let mut server = Server::open().unwrap();
    let mut cmd = Command::new(bash_program().unwrap());
    cmd.arg("-c")
        .arg(format!("source \"$PROMPT_UTILS\"; {body}"))
        .current_dir(dir)
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env(REPLY_ENV, server.env_value())
        .env("PROMPT_UTILS", HELPER)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    server.configure(&mut cmd);
    if detach {
        // SAFETY: setsid is async-signal-safe and touches no shared state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().expect("spawn bash");
    server.spawned();
    let mut server = answers.is_some().then_some(server);
    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
    let mut answers = answers.unwrap_or_default().iter();
    let mut stdout = String::new();
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        let mut close_channel = false;
        if is_request_line(&line)
            && let Some(server) = server.as_ref()
        {
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["coyote_prompt"]["id"].as_str().unwrap();
            match stray {
                Stray::None => server.reply(id, answers.next().unwrap_or(&Value::Null)),
                Stray::Before => {
                    server.reply("not-this-request", &json!(false));
                    server.reply(id, answers.next().unwrap_or(&Value::Null));
                }
                Stray::After => {
                    server.reply(id, answers.next().unwrap_or(&Value::Null));
                    server.reply("not-this-request", &json!(false));
                }
                Stray::ThenClose => {
                    server.reply("not-this-request", &json!(false));
                    let mut writer = &server.writer;
                    writer.write_all(b"this is not json\n").unwrap();
                    writer.flush().unwrap();
                    close_channel = true;
                }
            }
        }
        if close_channel {
            server = None;
        }
        stdout.push_str(&line);
        stdout.push('\n');
    }
    let status = child.wait().unwrap();
    Run {
        stdout,
        stderr: stderr_thread.join().unwrap(),
        code: status.code().unwrap_or(-1),
    }
}

/// `v=[<printed>] rc=<helper rc>` is how every value test reports.
fn capture(helper_call: &str) -> String {
    format!("v=\"$({helper_call})\"; rc=$?; echo \"v=[$v] rc=$rc\"")
}

#[test]
fn confirm_emits_one_request_and_maps_booleans_to_one_and_zero() {
    require_tools();
    let dir = scratch_dir("confirm");
    for (answer, printed) in [(true, "1"), (false, "0")] {
        let run = run_helper(
            &dir,
            Some(&[json!(answer)]),
            &capture("confirm \"Go?\""),
            false,
        );
        assert_eq!(run.code, 0, "stdout={}\nstderr={}", run.stdout, run.stderr);
        let requests = run.request_lines();
        assert_eq!(
            requests.len(),
            1,
            "exactly one request line: {}",
            run.stdout
        );
        let req = &requests[0]["coyote_prompt"];
        assert_eq!(req["action"], "confirm");
        assert_eq!(req["question"], "Go?");
        assert!(
            req["id"].as_str().is_some_and(|id| !id.is_empty()),
            "request carries a non-empty id: {req}"
        );
        assert_eq!(req["options"], json!([]));
        assert_eq!(run.other_lines(), vec![format!("v=[{printed}] rc=0")]);
        assert!(
            run.stderr.is_empty(),
            "protocol path writes nothing to stderr: {}",
            run.stderr
        );
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_prints_the_index_of_the_chosen_option_and_typed_text_as_is_with_exit_two() {
    require_tools();
    let dir = scratch_dir("list");
    let run = run_helper(
        &dir,
        Some(&[json!("b")]),
        &capture("list \"Pick\" a b c"),
        false,
    );
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(requests[0]["coyote_prompt"]["action"], "select");
    assert_eq!(
        requests[0]["coyote_prompt"]["options"],
        json!(["a", "b", "c"])
    );
    assert_eq!(run.other_lines(), vec!["v=[1] rc=0"]);

    // An answer that is not one of the offered options is text the
    // user typed, so it is printed as is and the exit status tells
    // it apart from an index.
    let run = run_helper(
        &dir,
        Some(&[json!("typed by hand")]),
        &capture("list \"Pick\" a b c"),
        false,
    );
    assert_eq!(
        run.other_lines(),
        vec!["v=[typed by hand] rc=2"],
        "{}",
        run.stdout
    );
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_answered_with_a_typed_digit_exits_two_so_it_is_not_an_index() {
    require_tools();
    let dir = scratch_dir("list-digit");
    let run = run_helper(
        &dir,
        Some(&[json!("1")]),
        &capture("list \"Pick\" a b c"),
        false,
    );
    assert_eq!(run.other_lines(), vec!["v=[1] rc=2"], "{}", run.stdout);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_answered_with_an_options_exact_text_prints_its_index() {
    require_tools();
    let dir = scratch_dir("list-exact");
    let run = run_helper(
        &dir,
        Some(&[json!("two words")]),
        &capture("list \"Pick\" one \"two words\" three"),
        false,
    );
    assert_eq!(run.other_lines(), vec!["v=[1] rc=0"], "{}", run.stdout);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_answered_with_blank_text_is_a_cancel() {
    require_tools();
    let dir = scratch_dir("list-blank");
    for blank in ["", "  \t "] {
        let run = run_helper(
            &dir,
            Some(&[json!(blank)]),
            &capture("list \"Pick\" a b c"),
            false,
        );
        assert_eq!(
            run.other_lines(),
            vec!["v=[] rc=1"],
            "{blank:?}: {}",
            run.stdout
        );
        assert!(run.stderr.is_empty(), "{blank:?}: {}", run.stderr);
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn options_starting_with_a_dash_keep_their_indices() {
    require_tools();
    let dir = scratch_dir("dash");
    let run = run_helper(
        &dir,
        Some(&[json!("-x")]),
        &capture("list \"Mode?\" --force -x plain"),
        false,
    );
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(
        requests[0]["coyote_prompt"]["options"],
        json!(["--force", "-x", "plain"])
    );
    assert_eq!(run.other_lines(), vec!["v=[1] rc=0"], "{}", run.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn checkbox_prints_space_separated_indices() {
    require_tools();
    let dir = scratch_dir("checkbox");
    let run = run_helper(
        &dir,
        Some(&[json!(["a", "c"])]),
        &capture("checkbox \"Pick many\" a b c"),
        false,
    );
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(requests[0]["coyote_prompt"]["action"], "checkbox");
    assert_eq!(
        requests[0]["coyote_prompt"]["options"],
        json!(["a", "b", "c"])
    );
    assert_eq!(run.other_lines(), vec!["v=[0 2] rc=0"]);

    // Nothing ticked is a valid, non-cancelled answer: empty output, rc 0.
    let run = run_helper(
        &dir,
        Some(&[json!([])]),
        &capture("checkbox \"Pick many\" a b c"),
        false,
    );
    assert_eq!(run.other_lines(), vec!["v=[] rc=0"], "{}", run.stdout);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn input_and_password_print_the_text_verbatim() {
    require_tools();
    let dir = scratch_dir("text");
    // Quotes, a backslash and non-ASCII letters must survive the round trip.
    let text = r#"Alice "Q" O'Brien \ ünï"#; // debris-exempt: the fixture is the non-ASCII input under test
    for (helper, action) in [("input", "input"), ("password", "password")] {
        let run = run_helper(
            &dir,
            Some(&[json!(text)]),
            &capture(&format!("{helper} \"Value?\"")),
            false,
        );
        let requests = run.request_lines();
        assert_eq!(requests.len(), 1, "{}", run.stdout);
        assert_eq!(requests[0]["coyote_prompt"]["action"], action);
        assert_eq!(requests[0]["coyote_prompt"]["question"], "Value?");
        assert_eq!(run.other_lines(), vec![format!("v=[{text}] rc=0")]);
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn question_text_is_json_escaped_in_the_request() {
    require_tools();
    let dir = scratch_dir("escape");
    let question = "Delete \"it\"? back\\slash tab\there";
    let body = capture(&format!("confirm '{question}'"));
    let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(requests[0]["coyote_prompt"]["question"], question);
    assert_eq!(run.other_lines(), vec!["v=[1] rc=0"]);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn null_answer_makes_every_helper_return_one() {
    require_tools();
    let dir = scratch_dir("null");
    for call in [
        "confirm \"Go?\"",
        "list \"Pick\" a b c",
        "checkbox \"Pick\" a b c",
        "input \"Name?\"",
        "password \"Secret?\"",
    ] {
        let run = run_helper(&dir, Some(&[Value::Null]), &capture(call), false);
        assert_eq!(run.request_lines().len(), 1, "{call}: {}", run.stdout);
        assert_eq!(
            run.other_lines(),
            vec!["v=[] rc=1"],
            "{call}: {}",
            run.stdout
        );
        assert!(run.stderr.is_empty(), "{call}: {}", run.stderr);
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn guard_operation_fails_closed_unless_the_answer_is_true() {
    require_tools();
    let dir = scratch_dir("guard-op");
    let body = "guard_operation \"Really?\"; echo \"continued rc=$?\"";

    let run = run_helper(&dir, Some(&[json!(true)]), body, false);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert_eq!(run.other_lines(), vec!["continued rc=0"]);

    // false, cancel, a non-boolean "truthy" value and a closed reply
    // channel all abort: only a boolean `true` lets the tool go on.
    let refusals: Vec<Option<Vec<Value>>> = vec![
        Some(vec![json!(false)]),
        Some(vec![Value::Null]),
        Some(vec![json!("1")]),
        Some(vec![json!(1)]),
        None,
    ];
    for answers in &refusals {
        let run = run_helper(&dir, answers.as_deref(), body, false);
        assert_eq!(
            run.code, 1,
            "answers={answers:?}\nstdout={}\nstderr={}",
            run.stdout, run.stderr
        );
        assert_eq!(
            run.request_lines().len(),
            1,
            "answers={answers:?}: {}",
            run.stdout
        );
        assert!(
            run.stderr.contains("Operation aborted!"),
            "answers={answers:?}: {}",
            run.stderr
        );
        assert!(
            !run.stdout.contains("continued"),
            "answers={answers:?}: the guarded body must not run: {}",
            run.stdout
        );
    }
    let run = run_helper(&dir, None, body, false);
    assert!(
        run.stderr.contains("prompt reply channel closed"),
        "{}",
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn guards_work_without_a_controlling_tty() {
    require_tools();
    let dir = scratch_dir("notty");
    // The pre-protocol guards redirected the prompt to /dev/tty; a detached
    // tool has none. On the protocol path the guard must still ask and
    // still honour the answer.
    let body = "(echo probe 2>/dev/tty) && echo has-tty || echo no-tty; \
                guard_operation \"Really?\"; echo \"continued rc=$?\"";
    let run = run_helper(&dir, Some(&[json!(true)]), body, true);
    assert!(
        run.stdout.contains("no-tty"),
        "the detached shell must have no tty: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert!(run.stdout.contains("continued rc=0"), "{}", run.stdout);

    let run = run_helper(&dir, Some(&[json!(false)]), body, true);
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains("Operation aborted!"),
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(!run.stdout.contains("continued"), "{}", run.stdout);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn guard_path_prompts_only_outside_the_working_directory_and_fails_closed() {
    require_tools();
    let dir = scratch_dir("guard-path");
    let outside = std::env::temp_dir().join("coyote-prompt-utils-outside.txt");
    let outside = outside.to_string_lossy().into_owned();
    // Note: the scratch dir lives under temp_dir too, so `outside` must be a
    // sibling of it, not a child; `pwd` is the scratch dir itself.
    let body = format!(
        "guard_path ./inside.txt \"Inside?\"; echo \"inside rc=$?\"; \
         guard_path \"{outside}\" \"Outside?\"; echo \"outside rc=$?\"; \
         guard_path \"{outside}\" \"Again?\"; echo \"again rc=$?\""
    );
    // One `true` then nothing: the second outside prompt is answered
    // null and aborts.
    let run = run_helper(&dir, Some(&[json!(true)]), &body, true);
    let requests = run.request_lines();
    assert_eq!(
        requests.len(),
        2,
        "inside path asks nothing: {}",
        run.stdout
    );
    assert_eq!(
        requests[0]["coyote_prompt"]["question"],
        format!("Outside?\n{outside}")
    );
    assert_eq!(
        requests[1]["coyote_prompt"]["question"],
        format!("Again?\n{outside}")
    );
    assert_eq!(
        run.other_lines(),
        vec!["inside rc=0", "outside rc=0"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("Operation aborted!"), "{}", run.stderr);

    let run = run_helper(
        &dir,
        Some(&[json!(false)]),
        &format!("guard_path \"{outside}\" \"Outside?\"; echo \"outside rc=$?\""),
        false,
    );
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(!run.stdout.contains("outside rc="), "{}", run.stdout);
    fs::remove_dir_all(&dir).ok();
}

/// "Yes for this session" is remembered against the question, so the
/// question must name the operation or path a guard lets through.
#[test]
fn guards_name_the_operation_or_path_they_guard() {
    require_tools();
    let dir = scratch_dir("guard-names");
    let outside = std::env::temp_dir().join("coyote-prompt-utils-named.txt");
    let outside = outside.to_string_lossy().into_owned();
    let body = format!(
        "echo \"$PWD\"; \
         guard_operation \"Run?\" \"rm -rf build\"; \
         guard_operation \"\" \"ls\"; \
         guard_operation \"Run?\"; \
         guard_path \"{outside}\" \"Remove '{outside}'?\"; \
         guard_path \"{outside}\" \"Touch?\"; \
         guard_path \"$PWD/../files.txt\" \"Remove $PWD/../files.txt?\"; \
         guard_path \"$PWD-other/f\" \"Touch?\"; echo done"
    );
    let yes = vec![json!(true); 7];
    let run = run_helper(&dir, Some(&yes), &body, false);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let other = run.other_lines();
    let [cwd, done] = other.as_slice() else {
        panic!("{other:?}");
    };
    assert_eq!(*done, "done");
    let parent = Path::new(cwd).parent().unwrap().display().to_string();
    let questions: Vec<Value> = run
        .request_lines()
        .iter()
        .map(|req| req["coyote_prompt"]["question"].clone())
        .collect();
    assert_eq!(
        questions,
        vec![
            json!("Run?\nrm -rf build"),
            json!("Are you sure you want to continue?\nls"),
            json!("Run?"),
            json!(format!("Remove '{outside}'?")),
            json!(format!("Touch?\n{outside}")),
            json!(format!("Remove {cwd}/../files.txt?\n{parent}/files.txt")),
            json!(format!("Touch?\n{cwd}-other/f")),
        ]
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn auto_confirm_short_circuit_emits_no_request() {
    require_tools();
    let dir = scratch_dir("auto");
    for var in ["AUTO_CONFIRM", "LLM_AGENT_VAR_AUTO_CONFIRM"] {
        let body = format!(
            "export {var}=1; guard_operation \"Really?\"; echo \"op rc=$?\"; \
             guard_path /etc/hosts \"Path?\"; echo \"path rc=$?\""
        );
        // No channel: any emitted request would hit EOF and abort.
        let run = run_helper(&dir, None, &body, false);
        assert_eq!(run.code, 0, "{var}: {}\n{}", run.stdout, run.stderr);
        assert!(run.request_lines().is_empty(), "{var}: {}", run.stdout);
        assert_eq!(run.other_lines(), vec!["op rc=0", "path rc=0"], "{var}");
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn request_goes_to_the_real_stdout_even_when_the_helper_is_redirected() {
    require_tools();
    let dir = scratch_dir("real-stdout");
    // The request must bypass whatever the caller does with the helper's
    // stdout; the printed value, by contrast, follows the redirect.
    let run = run_helper(
        &dir,
        Some(&[json!(true)]),
        "v=\"$(confirm \"Go?\" >/dev/null)\"; echo \"v=[$v]\"",
        false,
    );
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert_eq!(run.other_lines(), vec!["v=[]"]);

    // And a request written inside a nested substitution still surfaces.
    let run = run_helper(
        &dir,
        Some(&[json!(true)]),
        "outer=\"$(inner=\"$(confirm \"Go?\")\"; echo \"inner=[$inner]\")\"; echo \"$outer\"",
        false,
    );
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert_eq!(run.other_lines(), vec!["inner=[1]"]);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn successive_prompts_consume_replies_in_order_with_distinct_ids() {
    require_tools();
    let dir = scratch_dir("order");
    let run = run_helper(
        &dir,
        Some(&[json!(true), json!("c"), json!("Bob")]),
        "a=\"$(confirm \"One?\")\"; b=\"$(list \"Two?\" a b c)\"; c=\"$(input \"Three?\")\"; \
         echo \"a=[$a] b=[$b] c=[$c]\"",
        false,
    );
    let requests = run.request_lines();
    assert_eq!(requests.len(), 3, "{}", run.stdout);
    let questions: Vec<&str> = requests
        .iter()
        .map(|req| req["coyote_prompt"]["question"].as_str().unwrap())
        .collect();
    assert_eq!(questions, vec!["One?", "Two?", "Three?"]);
    let mut ids: Vec<&str> = requests
        .iter()
        .map(|req| req["coyote_prompt"]["id"].as_str().unwrap())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "request ids must be distinct: {}", run.stdout);
    assert_eq!(run.other_lines(), vec!["a=[1] b=[2] c=[Bob]"]);
    fs::remove_dir_all(&dir).ok();
}

// Replies are read until the id matches; a cancel makes the helper
// exit 1 with its usual stderr message. The shipped tools
// (`execute_command.sh`, `fs_rm.sh`, ...) and the `run-tool.sh` shim
// all run under `set -e`, so the guards are exercised that way too.

#[test]
fn reply_lines_for_other_ids_are_skipped_until_ours_arrives() {
    require_tools();
    let dir = scratch_dir("stray-id");
    let run = run_helper_opts(
        &dir,
        Some(&[json!(true), json!("b")]),
        "a=\"$(confirm \"One?\")\"; b=\"$(list \"Two?\" a b c)\"; echo \"a=[$a] b=[$b]\"",
        false,
        Stray::Before,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 2, "{}", run.stdout);
    // The stray `false` for a foreign id must not be taken as ours.
    assert_eq!(run.other_lines(), vec!["a=[1] b=[1]"]);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn guards_under_set_e_abort_with_their_message_on_cancel() {
    require_tools();
    let dir = scratch_dir("set-e");
    let outside = scratch_dir("set-e-outside").join("victim.txt");
    fs::write(&outside, "x").unwrap();
    let bodies = [
        "set -e; guard_operation \"Really?\"; echo continued".to_string(),
        format!(
            "set -e; guard_path \"{}\" \"Touch?\"; echo continued",
            outside.display()
        ),
    ];
    for body in &bodies {
        // A plain `false` keeps its message under set -e...
        let run = run_helper(&dir, Some(&[json!(false)]), body, false);
        assert_eq!(run.code, 1, "{body}\n{}\n{}", run.stdout, run.stderr);
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{body}: stderr={}",
            run.stderr
        );
        assert!(!run.stdout.contains("continued"), "{body}: {}", run.stdout);

        // ...and so must a cancel (`null`): the tool exits 1 AND says why.
        let run = run_helper(&dir, Some(&[Value::Null]), body, false);
        assert_eq!(run.code, 1, "{body}\n{}\n{}", run.stdout, run.stderr);
        assert!(!run.stdout.contains("continued"), "{body}: {}", run.stdout);
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{body}: a cancelled prompt under set -e must still report \
             'Operation aborted!' on stderr; got stderr={:?}",
            run.stderr
        );
    }
    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(outside.parent().unwrap()).ok();
}

const TOOLS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/functions/tools");

/// Sources a shipped tool the way the `run-tool.sh` shim does (env
/// prepared, `main` invoked) so the probe runs the real consumers.
fn shipped(tool: &str, pre: &str) -> String {
    format!(
        "export LLM_PROMPT_UTILS_FILE=\"$PROMPT_UTILS\" LLM_OUTPUT=/dev/stdout; \
         {pre}; source \"{TOOLS}/{tool}\"; main"
    )
}

#[test]
fn shipped_execute_command_honours_every_answer() {
    require_tools();
    let dir = scratch_dir("shipped-exec");
    let pre = "export LLM_TOOL_RAW_JSON='{\"command\":\"echo ran-x7x\"}'";
    let body = shipped("execute_command.sh", pre);

    let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(
        requests[0]["coyote_prompt"]["question"],
        "Run this command?\necho ran-x7x"
    );
    assert_eq!(run.other_lines(), vec!["ran-x7x"]);

    for answer in [json!(false), Value::Null] {
        let run = run_helper(&dir, Some(std::slice::from_ref(&answer)), &body, false);
        assert_eq!(run.code, 1, "{answer}: {}\n{}", run.stdout, run.stderr);
        assert!(run.other_lines().is_empty(), "{answer}: {}", run.stdout);
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{answer}: execute_command must say why it stopped; stderr={:?}",
            run.stderr
        );
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn shipped_fs_rm_outside_the_working_directory_asks_and_fails_closed() {
    require_tools();
    let dir = scratch_dir("shipped-rm");
    let victim_dir = scratch_dir("shipped-rm-victim");
    let victim = victim_dir.join("victim.txt");
    let body = shipped("fs_rm.sh", &format!("argc_path=\"{}\"", victim.display()));

    for answer in [json!(false), Value::Null] {
        fs::write(&victim, "x").unwrap();
        let run = run_helper(&dir, Some(std::slice::from_ref(&answer)), &body, false);
        assert_eq!(run.code, 1, "{answer}: {}\n{}", run.stdout, run.stderr);
        let requests = run.request_lines();
        assert_eq!(requests.len(), 1, "{answer}: {}", run.stdout);
        assert_eq!(
            requests[0]["coyote_prompt"]["question"],
            format!("Remove '{}'?", victim.display())
        );
        assert!(victim.exists(), "{answer}: the refused path must survive");
        assert!(
            !run.stdout.contains("Path removed"),
            "{answer}: {}",
            run.stdout
        );
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{answer}: fs_rm must say why it stopped; stderr={:?}",
            run.stderr
        );
    }

    fs::write(&victim, "x").unwrap();
    let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert!(!victim.exists(), "an approved removal must remove the path");
    assert_eq!(
        run.other_lines(),
        vec![format!("Path removed: {}", victim.display())]
    );
    fs::remove_dir_all(&dir).ok();
    fs::remove_dir_all(&victim_dir).ok();
}

/// Installs shipped `tool` under its own name the way the shim runs
/// it, with the call's JSON arguments read from `LLM_TOOL_RAW_JSON`.
fn install_shipped(tool: &str) {
    install_tool(
        tool,
        &format!(
            "unset AUTO_CONFIRM LLM_AGENT_VAR_AUTO_CONFIRM\n\
             export LLM_PROMPT_UTILS_FILE=\"{HELPER}\"\n\
             source \"{TOOLS}/{tool}.sh\"\nmain"
        ),
    );
}

fn run_shipped(tool: &str, args: Value, io: ToolIo) -> Value {
    let mut envs = tool_envs();
    envs.insert("LLM_TOOL_RAW_JSON".to_string(), args.to_string());
    run_tool(tool, envs, io)
}

#[test]
#[serial]
fn shipped_execute_command_session_yes_covers_only_that_command() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-shipped-exec-session");
    install_shipped("execute_command");

    check_session_yes_covers_one_operation(
        "execute_command",
        |command, io| run_shipped("execute_command", json!({"command": command}), io),
        "echo one-x7x",
        "echo two-x7x",
    );
}

#[test]
#[serial]
fn shipped_fs_write_and_fs_patch_session_yes_covers_only_that_path() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-shipped-write-session");
    let dir = scratch_dir("shipped-write-session");
    let first = dir.join("one-x7x.txt").to_string_lossy().into_owned();
    let second = dir.join("two-x7x.txt").to_string_lossy().into_owned();
    install_shipped("fs_write");
    install_shipped("fs_patch");

    // An existing file takes the overwrite branch.
    check_session_yes_covers_one_operation(
        "fs_write",
        |path, io| {
            fs::write(path, "old\n").unwrap();
            run_shipped("fs_write", json!({"path": path, "content": "new\n"}), io)
        },
        &first,
        &second,
    );
    check_session_yes_covers_one_operation(
        "fs_patch",
        |path, io| {
            fs::write(path, "old\n").unwrap();
            run_shipped(
                "fs_patch",
                json!({"path": path, "content": "@@\n-old\n+new\n"}),
                io,
            )
        },
        &first,
        &second,
    );
    assert_eq!(fs::read_to_string(&second).unwrap(), "new\n");

    // A `%` in a name is shown as it is, so two such names differ.
    check_session_yes_covers_one_operation(
        "fs_write",
        |path, io| {
            fs::write(path, "old\n").unwrap();
            run_shipped("fs_write", json!({"path": path, "content": "new\n"}), io)
        },
        &dir.join("100%s.txt").to_string_lossy(),
        &dir.join("100%d.txt").to_string_lossy(),
    );

    // A backslash in a name is kept, not read as an escape, so
    // `a\qb` and `aqb` are two paths.
    check_session_yes_covers_one_operation(
        "fs_write",
        |path, io| {
            fs::write(path, "old\n").unwrap();
            run_shipped("fs_write", json!({"path": path, "content": "new\n"}), io)
        },
        &dir.join("a\\qb.txt").to_string_lossy(),
        &dir.join("aqb.txt").to_string_lossy(),
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
#[serial]
fn shipped_execute_sql_code_session_yes_covers_only_that_statement() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-shipped-sql-session");
    // A stand-in for the database client, so only the guard runs.
    install_tool(
        "execute_sql_code",
        &format!(
            "unset AUTO_CONFIRM LLM_AGENT_VAR_AUTO_CONFIRM\n\
             export LLM_PROMPT_UTILS_FILE=\"{HELPER}\"\n\
             usql() {{ echo ran; }}\n\
             source \"{TOOLS}/execute_sql_code.sh\"\nmain"
        ),
    );

    check_session_yes_covers_one_operation(
        "execute_sql_code",
        |code, io| run_shipped("execute_sql_code", json!({"code": code}), io),
        "delete from one_x7x",
        "delete from two_x7x",
    );
}

#[test]
fn checkbox_and_input_accept_empty_answers() {
    require_tools();
    let dir = scratch_dir("empty");
    let run = run_helper(
        &dir,
        Some(&[json!([]), json!("")]),
        &format!(
            "{}; {}",
            capture("checkbox \"Pick\" a b c"),
            capture("input \"Name?\"")
        ),
        false,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 2, "{}", run.stdout);
    // Nothing picked / nothing typed is a valid answer, not a cancel.
    assert_eq!(run.other_lines(), vec!["v=[] rc=0", "v=[] rc=0"]);
    fs::remove_dir_all(&dir).ok();
}

// The helpers' return conventions must hold whatever shell options
// and option texts a tool author brings along, and the channel the
// host hands a tool must still be there one `bash` deeper.

#[test]
fn helpers_keep_their_conventions_under_strict_mode_and_a_custom_ifs() {
    require_tools();
    let dir = scratch_dir("strict");
    // `set -euo pipefail` plus a whitespace-free IFS is the common
    // "strict mode" prologue; an unset variable or a word-splitting
    // assumption on the protocol path would surface here.
    // `set +u` before the guard: `guard_path` reaches `_to_real_path`,
    // whose bare `$OS` read predates the protocol and is not nounset
    // clean; that path is outside the protocol contract.
    let body = format!(
        "set -euo pipefail; IFS=$'\\n\\t'; {}; {}; {}; {}; \
         set +u; guard_operation \"Go on?\"; echo \"guard rc=$?\"",
        capture("confirm \"Go?\""),
        capture("list \"Pick\" a b c"),
        capture("checkbox \"Pick many\" a b c"),
        capture("input \"Name?\"")
    );
    let run = run_helper(
        &dir,
        Some(&[
            json!(true),
            json!("c"),
            json!(["a", "b", "c"]),
            json!("Bob Smith"),
            json!(true),
        ]),
        &body,
        false,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 5, "{}", run.stdout);
    assert_eq!(
        run.other_lines(),
        vec![
            "v=[1] rc=0",
            "v=[2] rc=0",
            "v=[0 1 2] rc=0",
            "v=[Bob Smith] rc=0",
            "guard rc=0",
        ],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(run.stderr.is_empty(), "{}", run.stderr);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn options_are_matched_exactly_whatever_their_text() {
    require_tools();
    let dir = scratch_dir("options");
    // Spaces, quotes, regex metacharacters, non-ASCII and an option that
    // is a prefix of another: the request must carry them verbatim and
    // the printed index must come from an exact match.
    let options = [
        "foo",
        "foo.bar",
        "two words",
        "say \"hi\"",
        "a+b*",
        "ünï", // debris-exempt: the fixture is the non-ASCII option under test
        "10",
        "1",
    ];
    let quoted: Vec<String> = options
        .iter()
        .map(|opt| format!("'{}'", opt.replace('\'', "'\\''")))
        .collect();
    let quoted = quoted.join(" ");
    for (answer, index) in [
        ("foo", 0),
        ("foo.bar", 1),
        ("two words", 2),
        ("say \"hi\"", 3),
        ("a+b*", 4),
        ("ünï", 5), // debris-exempt: see the fixture above
        ("1", 7),
    ] {
        let run = run_helper(
            &dir,
            Some(&[json!(answer)]),
            &capture(&format!("list \"Pick\" {quoted}")),
            false,
        );
        let requests = run.request_lines();
        assert_eq!(requests.len(), 1, "{answer}: {}", run.stdout);
        assert_eq!(
            requests[0]["coyote_prompt"]["options"],
            json!(options),
            "{answer}: options must reach the host verbatim"
        );
        assert_eq!(
            run.other_lines(),
            vec![format!("v=[{index}] rc=0")],
            "{answer}: {}\n{}",
            run.stdout,
            run.stderr
        );
    }

    let run = run_helper(
        &dir,
        Some(&[json!(["1", "two words", "foo.bar"])]),
        &capture(&format!("checkbox \"Pick many\" {quoted}")),
        false,
    );
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    let printed = run.other_lines();
    assert_eq!(printed.len(), 1, "{}", run.stdout);
    let mut indices: Vec<&str> = printed[0]
        .trim_start_matches("v=[")
        .split("] rc=")
        .next()
        .unwrap()
        .split(' ')
        .collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec!["1", "2", "7"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(printed[0].ends_with("rc=0"), "{}", run.stdout);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_option_with_a_control_character_is_sent_verbatim_and_index_mapped() {
    require_tools();
    let dir = scratch_dir("tab-option");

    let run = run_helper(
        &dir,
        Some(&[json!("a\tb")]),
        &capture("list \"Pick\" $'a\\tb' plain"),
        false,
    );

    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(
        requests[0]["coyote_prompt"]["options"],
        json!(["a\tb", "plain"])
    );
    assert_eq!(
        run.other_lines(),
        vec!["v=[0] rc=0"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_nested_bash_script_inherits_the_channel() {
    require_tools();
    let dir = scratch_dir("nested");
    // A tool that shells out to another script which sources the helper
    // itself: the reply fd and the real stdout are inherited, so the
    // inner prompt must round-trip exactly like an outer one.
    let inner = dir.join("inner.sh");
    fs::write(
        &inner,
        "source \"$PROMPT_UTILS\"\nv=\"$(confirm \"Inner?\")\"; echo \"inner v=[$v] rc=$?\"\n",
    )
    .unwrap();
    let body = format!(
        "a=\"$(confirm \"Outer?\")\"; echo \"outer a=[$a]\"; bash \"{}\"; echo \"after rc=$?\"",
        inner.display()
    );
    let run = run_helper(&dir, Some(&[json!(true), json!(false)]), &body, false);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let requests = run.request_lines();
    assert_eq!(requests.len(), 2, "{}", run.stdout);
    assert_eq!(requests[1]["coyote_prompt"]["question"], "Inner?");
    assert_eq!(
        run.other_lines(),
        vec!["outer a=[1]", "inner v=[0] rc=0", "after rc=0"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn sourcing_the_helper_twice_keeps_a_single_channel() {
    require_tools();
    let dir = scratch_dir("resource");
    // Tool libraries routinely re-source the helper. The second
    // source must neither save fd 9 over the already-saved copy nor
    // lose the channel: one request, the usual answer, no noise.
    let body = format!(
        "source \"$PROMPT_UTILS\"; {}",
        capture("confirm \"Again?\"")
    );
    let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(requests[0]["coyote_prompt"]["question"], "Again?");
    assert_eq!(
        run.other_lines(),
        vec!["v=[1] rc=0"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.is_empty(),
        "re-sourcing must stay silent: {}",
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

// The request is ONE stdout line whatever the question text, answers
// are printed as text, secrets stay off
// stderr, the terminal-only helpers never touch the channel, and an
// unusable reply path fails closed instead of hanging the tool.

#[test]
fn question_with_a_newline_stays_one_request_line() {
    require_tools();
    let dir = scratch_dir("newline-question");
    // A tool writes ONE stdout line: a raw newline in the question
    // must be JSON-escaped, never split the request in two.
    let run = run_helper(
        &dir,
        Some(&[json!(true)]),
        &capture("confirm $'first line\\nsecond line'"),
        false,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(
        requests[0]["coyote_prompt"]["question"],
        "first line\nsecond line"
    );
    assert_eq!(
        run.other_lines(),
        vec!["v=[1] rc=0"],
        "a fragment of the request must not leak as plain output: {}",
        run.stdout
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn multi_line_input_answer_is_printed_as_text() {
    require_tools();
    let dir = scratch_dir("newline-answer");
    // input/password print the text: the reply is one JSON line, so a
    // newline inside the answer arrives escaped and must be decoded.
    let run = run_helper(
        &dir,
        Some(&[json!("line one\nline two")]),
        &capture("input \"Notes?\""),
        false,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert_eq!(
        run.other_lines(),
        vec!["v=[line one", "line two] rc=0"],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn password_answer_never_reaches_stderr() {
    require_tools();
    let dir = scratch_dir("password-stderr");
    // The terminal path echoes asterisks and the prompt to stderr; on
    // the protocol path nothing about the secret may surface there,
    // and the request itself carries no default that could leak one.
    let secret = "hunter2-x7x";
    let run = run_helper(
        &dir,
        Some(&[json!(secret)]),
        &capture("password \"Token?\""),
        false,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let requests = run.request_lines();
    assert_eq!(requests.len(), 1, "{}", run.stdout);
    assert_eq!(requests[0]["coyote_prompt"]["action"], "password");
    assert!(
        !requests[0].to_string().contains(secret),
        "the request precedes the answer and cannot contain it: {}",
        requests[0]
    );
    assert_eq!(run.other_lines(), vec![format!("v=[{secret}] rc=0")]);
    assert!(
        run.stderr.is_empty(),
        "protocol password path must write nothing to stderr: {:?}",
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn editor_and_range_stay_terminal_only() {
    require_tools();
    let dir = scratch_dir("terminal-only");
    // `editor` and `range` stay terminal-only. With a live
    // channel and no tty they must still not emit a protocol request;
    // `range` falls through to its current value on EOF and `editor`
    // returns an empty buffer. `timeout` is only a hang guard.
    let body = format!(
        "export EDITOR=true; \
         r=\"$({guard} bash -c 'source \"$PROMPT_UTILS\"; range \"How many?\" 1 3 5' 2>/dev/null)\"; echo \"range=[$r] rc=$?\"; \
         e=\"$({guard} bash -c 'source \"$PROMPT_UTILS\"; editor \"Notes?\"' 2>/dev/null)\"; echo \"editor=[$e] rc=$?\"",
        guard = hang_guard(),
    );
    let run = run_helper(&dir, Some(&[json!(true)]), &body, true);
    assert!(
        run.request_lines().is_empty(),
        "editor/range must never use the prompt channel: {}",
        run.stdout
    );
    let printed = run.other_lines();
    assert_eq!(printed.len(), 2, "{}\n{}", run.stdout, run.stderr);
    assert!(
        printed[0].starts_with("range=[") && !printed[0].contains("rc=124"),
        "range must terminate without a tty: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        printed[1].starts_with("editor=[") && !printed[1].contains("rc=124"),
        "editor must terminate without a tty: {}\n{}",
        run.stdout,
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_unopenable_reply_path_fails_closed_without_hanging() {
    require_tools();
    let dir = scratch_dir("bad-reply-path");
    // A stale or broken COYOTE_PROMPT_REPLY (an fd the child did not
    // inherit) is "fail closed for prompting": the helper returns 1
    // and a guard aborts with its usual message, and nothing blocks.
    // The outer shell keeps the real channel so a request that does
    // escape is still answered rather than deadlocking the harness.
    let dead = dead_reply_path();
    let body = format!(
        "{}; \
         COYOTE_PROMPT_REPLY={dead} {guard} bash -c '{}'; echo \"outer rc=$?\"; \
         COYOTE_PROMPT_REPLY={dead} {guard} bash -c 'source \"$PROMPT_UTILS\"; guard_operation \"Really?\"; echo continued'; echo \"guard rc=$?\"",
        capture("confirm \"Warm up?\""),
        "source \"$PROMPT_UTILS\"; v=\"$(confirm \"Go?\")\"; rc=$?; echo \"v=[$v] rc=$rc\"; exit 0",
        guard = hang_guard(),
    );
    let started = std::time::Instant::now();
    let run = run_helper(
        &dir,
        Some(&[json!(true), json!(true), json!(true)]),
        &body,
        false,
    );
    // Strictly below the guard, so a helper that blocks until
    // `timeout` kills it still fails here.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(HANG_GUARD_SECS / 2),
        "the helper must not block on an unopenable reply path: {:?}",
        started.elapsed()
    );
    let printed = run.other_lines();
    assert!(
        printed.contains(&"v=[1] rc=0"),
        "the warm-up on the real channel must work: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        printed.contains(&"v=[] rc=1"),
        "confirm over a dead reply path must print nothing and return 1: {}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        printed.contains(&"outer rc=0") && printed.contains(&"guard rc=1"),
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    assert!(
        !run.stdout.contains("continued"),
        "the guarded body must not run: {}",
        run.stdout
    );
    assert!(
        run.stderr.contains("Operation aborted!"),
        "the guard must say why it stopped: {:?}",
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_channel_or_the_reserved_fd_keeps_the_helpers_on_the_terminal_path() {
    require_tools();
    let dir = scratch_dir("no-channel");
    // Without COYOTE_PROMPT_REPLY, or with it naming the fd the helper
    // reserves for its own request stream, the helpers must behave as
    // before the protocol existed: no request line, the terminal path
    // (which, with no tty, settles on the confirm default) and no hang.
    // The outer shell keeps the real channel so a request that does
    // escape is answered rather than deadlocking the harness.
    let inner = "source \"$PROMPT_UTILS\"; v=\"$(confirm \"Go?\")\"; echo \"{tag} v=[$v] rc=$?\"";
    let body = format!(
        "env -u {reply} -u {out} {guard} bash -c '{unset}' 2>/dev/null; echo \"unset outer=$?\"; \
         {reply}=/dev/fd/9 env -u {out} {guard} bash -c '{fd9}' 2>/dev/null; echo \"fd9 outer=$?\"",
        reply = REPLY_ENV,
        out = OUT_FD_ENV,
        guard = hang_guard(),
        unset = inner.replace("{tag}", "unset"),
        fd9 = inner.replace("{tag}", "fd9"),
    );
    let run = run_helper(&dir, Some(&[json!(true), json!(true)]), &body, true);
    assert!(
        run.request_lines().is_empty(),
        "no channel means no protocol request: {}",
        run.stdout
    );
    let printed = run.other_lines();
    assert_eq!(
        printed,
        vec![
            "unset v=[0] rc=0",
            "unset outer=0",
            "fd9 v=[0] rc=0",
            "fd9 outer=0"
        ],
        "{}\n{}",
        run.stdout,
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn replies_of_the_wrong_shape_are_unusable_for_every_helper() {
    require_tools();
    let dir = scratch_dir("wrong-shape");
    // The reply must fit the action it answers: a string is not a
    // confirm, an index is not a select, a bare string is not a
    // checkbox and a number or boolean is not text. Each helper prints
    // nothing, returns 1 and says why on stderr, so callers fail closed
    // exactly as they do for null.
    let cases: Vec<(&str, Value)> = vec![
        ("confirm \"Go?\"", json!("yes")),
        ("confirm \"Go?\"", json!(1)),
        ("list \"Pick\" a b", json!(0)),
        ("list \"Pick\" a b", json!(["a"])),
        ("checkbox \"Pick\" a b", json!("a")),
        ("checkbox \"Pick\" a b", json!(true)),
        ("input \"Name?\"", json!(42)),
        ("input \"Name?\"", json!(["x"])),
        ("password \"Secret?\"", json!(true)),
        ("password \"Secret?\"", json!({"text": "x"})),
    ];
    for (call, answer) in &cases {
        let run = run_helper(
            &dir,
            Some(std::slice::from_ref(answer)),
            &capture(call),
            false,
        );
        assert_eq!(
            run.request_lines().len(),
            1,
            "{call} <- {answer}: {}",
            run.stdout
        );
        assert_eq!(
            run.other_lines(),
            vec!["v=[] rc=1"],
            "{call} <- {answer}: {}\n{}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stderr.contains("prompt reply unusable"),
            "{call} <- {answer}: {}",
            run.stderr
        );
        if call.starts_with("password") {
            assert!(
                !run.stderr.contains("\"x\"") && !run.stderr.contains("true"),
                "{call}: a rejected password reply must not be echoed: {}",
                run.stderr
            );
        }
    }
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn each_helper_consumes_exactly_one_reply_line() {
    require_tools();
    let dir = scratch_dir("one-line");
    // A reply the helper did not ask for sits behind every real one.
    // The second helper must skip the first decoy and stop at its own
    // reply; the decoy behind that must still be in the pipe when the
    // tool reads the channel itself, and nothing may follow it.
    let body = "a=\"$(confirm \"One?\")\"; b=\"$(list \"Two?\" a b c)\"; \
                IFS= read -r -t 5 rest <\"$COYOTE_PROMPT_REPLY\"; rc=$?; \
                IFS= read -r -t 1 more <\"$COYOTE_PROMPT_REPLY\"; \
                echo \"a=[$a] b=[$b] rc=$rc more=[$more]\"; echo \"rest=$rest\"";
    let run = run_helper_opts(
        &dir,
        Some(&[json!(true), json!("b")]),
        body,
        false,
        Stray::After,
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(run.request_lines().len(), 2, "{}", run.stdout);
    let lines = run.other_lines();
    assert_eq!(lines.len(), 2, "{}\n{}", run.stdout, run.stderr);
    assert_eq!(lines[0], "a=[1] b=[1] rc=0 more=[]", "{}", run.stderr);
    let rest: Value = serde_json::from_str(lines[1].trim_start_matches("rest="))
        .unwrap_or_else(|err| panic!("leftover is not the decoy ({err}): {}", lines[1]));
    assert_eq!(rest["id"], json!("not-this-request"), "{}", run.stdout);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn strays_and_junk_before_a_closed_channel_still_fail_closed() {
    require_tools();
    let dir = scratch_dir("stray-eof");
    // The id-matching loop must give up at EOF rather than spin or
    // take a foreign reply: a stray id, a line that is not JSON and
    // then a closed channel leave every helper on the cancel path.
    for call in [
        "confirm \"Go?\"",
        "list \"Pick\" a b",
        "checkbox \"Pick\" a b",
        "input \"Name?\"",
        "password \"Secret?\"",
    ] {
        let run = run_helper_opts(&dir, Some(&[]), &capture(call), false, Stray::ThenClose);
        assert_eq!(run.request_lines().len(), 1, "{call}: {}", run.stdout);
        assert_eq!(
            run.other_lines(),
            vec!["v=[] rc=1"],
            "{call}: {}\n{}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stderr.contains("prompt reply channel closed"),
            "{call}: {}",
            run.stderr
        );
    }
    let run = run_helper_opts(
        &dir,
        Some(&[]),
        "guard_operation \"Really?\"; echo continued",
        false,
        Stray::ThenClose,
    );
    assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
    assert!(run.stderr.contains("Operation aborted!"), "{}", run.stderr);
    assert!(!run.stdout.contains("continued"), "{}", run.stdout);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_reply_longer_than_the_request_carry_cap_is_printed_whole() {
    require_tools();
    let dir = scratch_dir("big-reply");
    // The 64 KiB carry cap bounds request lines on the host side; the
    // reply path has no such limit and the helper prints all of it.
    let big = "x".repeat(CARRY_CAP + 6_000);
    let run = run_helper(
        &dir,
        Some(&[json!(big)]),
        "v=\"$(input \"Big?\")\"; rc=$?; echo \"len=${#v} rc=$rc\"",
        false,
    );
    assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
    assert_eq!(
        run.other_lines(),
        vec![format!("len={} rc=0", CARRY_CAP + 6_000).as_str()],
        "{}",
        run.stderr
    );
    fs::remove_dir_all(&dir).ok();
}
