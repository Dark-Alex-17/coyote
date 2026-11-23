use super::*;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const PY_HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.py"
);
const TS_HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.ts"
);

fn scratch_dir(tag: &str) -> PathBuf {
    // Tests run in parallel and the clock here ticks in microseconds.
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "coyote-prompt-script-{tag}-{}-{nanos}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

fn python_available() -> bool {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return false;
    }
    true
}

/// The command that runs a `.ts` file: `node` when it strips types
/// itself without a word on stderr (the contract cases require it
/// empty), else `tsx`; None, with the reason printed, when neither.
fn ts_runtime() -> Option<&'static str> {
    let dir = scratch_dir("ts-probe");
    let probe = dir.join("probe.ts");
    fs::write(&probe, "const n: number = 1;\nconsole.log(n);\n").unwrap();
    let run_probe = || {
        Command::new("node")
            .arg(&probe)
            .env("NODE_NO_WARNINGS", "1")
            .stdin(Stdio::null())
            .output()
    };
    let clean = |out: &std::io::Result<std::process::Output>| {
        out.as_ref()
            .is_ok_and(|out| out.status.success() && out.stderr.is_empty())
    };
    // A cold node start can print a one-off warning; one retry absorbs it.
    let mut probe_result = run_probe();
    if !clean(&probe_result) {
        probe_result = run_probe();
    }
    fs::remove_dir_all(&dir).ok();
    if clean(&probe_result) {
        return Some("node");
    }
    if which::which("tsx").is_ok() {
        return Some("tsx");
    }
    let (status, stderr) = match &probe_result {
        Ok(out) => (
            out.status.to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ),
        Err(err) => ("not spawned".to_string(), err.to_string()),
    };
    eprintln!(
        "skipping: node cannot run TypeScript cleanly ({status}; stderr: {stderr}) and tsx is not installed"
    );
    None
}

/// Copies a shipped helper into the active config's utils dir, where
/// the shim's env var points, writes `body` as a tool source and has
/// the real shim builder turn it into a runnable binary.
fn install_script_tool(helper: &str, tool_file: &str, body: &str) {
    let utils = crate::config::paths::global_utils_dir();
    fs::create_dir_all(&utils).unwrap();
    fs::copy(helper, utils.join(Path::new(helper).file_name().unwrap())).unwrap();
    let tools = crate::config::paths::global_tools_dir();
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(tool_file), body).unwrap();
    fs::create_dir_all(crate::config::paths::functions_bin_dir()).unwrap();
    crate::function::Functions::build_global_function_binary(tool_file, None).unwrap();
}

/// Like `run_tool`, with the JSON argument the shims insist on. A
/// shim built moments ago can still be open for writing in a child
/// forked elsewhere in the process, making exec fail with ETXTBSY;
/// that spawn is retried briefly instead of flaking.
fn run_shim_tool(name: &str, io: ToolIo) -> Value {
    let mut attempts = 0;
    let run = loop {
        match run_llm_function(
            name.to_string(),
            vec!["{}".to_string()],
            tool_envs(),
            None,
            None,
            io.clone(),
            None,
        ) {
            Err(err) if err.to_string().contains("Text file busy") && attempts < 10 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            run => break run,
        }
    };
    let result = run.unwrap().output.unwrap_or_default();
    match serde_json::from_str::<Value>(&result) {
        Ok(error) if error.get("tool_call_error").is_some() => error,
        _ => json!({"output": result}),
    }
}

fn round_trip_prompter() -> Arc<ScriptedPrompter> {
    Arc::new(ScriptedPrompter::new(vec![
        PromptAnswer::Confirmed(true),
        PromptAnswer::Selected("b".to_string()),
        PromptAnswer::Checked(vec!["a".to_string(), "c".to_string()]),
        PromptAnswer::Text("hello".to_string()),
        PromptAnswer::Text("s3cret".to_string()),
    ]))
}

/// Reaps `child`, killing it after 10 s so a wedged helper fails the
/// test instead of hanging it. Drain the child's pipes first.
fn wait_or_kill(child: &mut Child, what: &str) -> ExitStatus {
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > std::time::Duration::from_secs(10) {
            child.kill().ok();
            child.wait().ok();
            panic!("{what}: child still running after 10 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Hands each stdout line of `child` to `on_line` from a forwarding
/// thread, so a helper that has not closed its stdout within 10 s of
/// starting is killed instead of parking the test on a blocking read.
fn pump_stdout(child: &mut Child, what: &str, mut on_line: impl FnMut(String)) {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => on_line(line),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                child.kill().ok();
                child.wait().ok();
                panic!("{what}: the child did not finish within 10 s");
            }
        }
    }
    reader.join().unwrap();
}

fn assert_round_trip(
    tool: &str,
    prompter: &ScriptedPrompter,
    sink: &RecordingSink,
    output: &Value,
    expected: &str,
) {
    assert_eq!(output["output"], expected, "{output}");
    let requests = prompter.requests();
    assert_eq!(requests.len(), 5, "{requests:?}");
    let actions: Vec<PromptAction> = requests.iter().map(|req| req.action).collect();
    assert_eq!(
        actions,
        vec![
            PromptAction::Confirm,
            PromptAction::Select,
            PromptAction::Checkbox,
            PromptAction::Input,
            PromptAction::Password,
        ]
    );
    for req in &requests {
        assert_eq!(req.tool.as_deref(), Some(tool));
        assert_eq!(req.call_id.as_deref(), Some("call-x7x"));
        assert_eq!(req.job_id, None);
    }
    assert_eq!(requests[0].question, format!("Delete? {tool}"));
    assert!(requests[0].options.is_empty());
    assert_eq!(requests[1].options, vec!["a", "b", "c"]);
    assert_eq!(requests[2].options, vec!["a", "b", "c"]);
    let chunks = stdout_chunks(&sink.drain(), "call-x7x");
    assert!(!chunks.contains("coyote_prompt"), "{chunks}");
}

fn assert_cancelled(output: &Value, prompter: &ScriptedPrompter, question: &str) {
    assert!(
        output["tool_call_error"]
            .as_str()
            .unwrap_or_default()
            .contains("exited with code 1"),
        "{output}"
    );
    assert!(
        output["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("Operation aborted!"),
        "{output}"
    );
    assert_eq!(output.get("output"), None, "{output}");
    assert_eq!(prompter.requests().len(), 1);
    assert_eq!(prompter.requests()[0].question, question);
}

const PY_IMPORT: &str = "import importlib.util, os, sys\n\
    sys.dont_write_bytecode = True\n\
    spec = importlib.util.spec_from_file_location(\"prompt_utils\", os.environ[\"LLM_PROMPT_UTILS_PY\"])\n\
    prompt_utils = importlib.util.module_from_spec(spec); spec.loader.exec_module(prompt_utils)\n";

fn ts_tool(runtime: &str, body: &str) -> String {
    let shebang = if runtime == "node" {
        "#!/usr/bin/env node\n"
    } else {
        ""
    };
    format!(
        "{shebang}import {{ pathToFileURL }} from \"node:url\";\n\
         export async function run(): Promise<string> {{\n\
         \x20 const prompt = await import(pathToFileURL(process.env.LLM_PROMPT_UTILS_TS!).href);\n\
         {body}\n\
         }}\n"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn python_tool_round_trips_every_action_through_the_shim() {
    if !python_available() {
        return;
    }
    let guard = TestConfigDirGuard::new("prompt-py");
    let _functions = crate::testing::EnvVarGuard::set(
        crate::utils::get_env_name("functions_dir"),
        guard.path.join("functions"),
    );
    install_script_tool(
        PY_HELPER,
        "ask-py-x7x.py",
        &format!(
            "#!/usr/bin/env python3\n{PY_IMPORT}\n\
             def run(**kwargs):\n\
             \x20   answers = [\n\
             \x20       prompt_utils.confirm(\"Delete? ask-py-x7x\"),\n\
             \x20       prompt_utils.select(\"Pick one ask-py-x7x\", [\"a\", \"b\", \"c\"]),\n\
             \x20       prompt_utils.checkbox(\"Pick many ask-py-x7x\", [\"a\", \"b\", \"c\"]),\n\
             \x20       prompt_utils.input(\"Name? ask-py-x7x\"),\n\
             \x20       prompt_utils.password(\"Secret? ask-py-x7x\"),\n\
             \x20   ]\n\
             \x20   return \"\".join(f\"{{answer}}\\n\" for answer in answers)\n"
        ),
    );
    let prompter = round_trip_prompter();
    let sink = RecordingSink::install();

    let output = run_shim_tool(
        "ask-py-x7x",
        io_with(&prompter, Arc::new(PromptMemory::default())),
    );

    // Python reprs: bool, index, list of indices, then the two texts.
    assert_round_trip(
        "ask-py-x7x",
        &prompter,
        &sink,
        &output,
        "True\n1\n[0, 2]\nhello\ns3cret\n",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn typescript_tool_round_trips_every_action_through_the_shim() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let guard = TestConfigDirGuard::new("prompt-ts");
    let _functions = crate::testing::EnvVarGuard::set(
        crate::utils::get_env_name("functions_dir"),
        guard.path.join("functions"),
    );
    install_script_tool(
        TS_HELPER,
        "ask-ts-x7x.ts",
        &ts_tool(
            runtime,
            "  const answers = [\n\
             \x20   prompt.confirm(\"Delete? ask-ts-x7x\"),\n\
             \x20   prompt.select(\"Pick one ask-ts-x7x\", [\"a\", \"b\", \"c\"]),\n\
             \x20   prompt.checkbox(\"Pick many ask-ts-x7x\", [\"a\", \"b\", \"c\"]),\n\
             \x20   prompt.input(\"Name? ask-ts-x7x\"),\n\
             \x20   prompt.password(\"Secret? ask-ts-x7x\"),\n\
             \x20 ];\n\
             \x20 return answers.map((a) => `${JSON.stringify(a)}\\n`).join(\"\");",
        ),
    );
    let prompter = round_trip_prompter();
    let sink = RecordingSink::install();

    let output = run_shim_tool(
        "ask-ts-x7x",
        io_with(&prompter, Arc::new(PromptMemory::default())),
    );

    // JSON: bool, index, array of indices, then the two quoted texts.
    assert_round_trip(
        "ask-ts-x7x",
        &prompter,
        &sink,
        &output,
        "true\n1\n[0,2]\n\"hello\"\n\"s3cret\"\n",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_fails_the_guarded_python_tool_closed() {
    if !python_available() {
        return;
    }
    let guard = TestConfigDirGuard::new("prompt-py-cancel");
    let _functions = crate::testing::EnvVarGuard::set(
        crate::utils::get_env_name("functions_dir"),
        guard.path.join("functions"),
    );
    install_script_tool(
        PY_HELPER,
        "cancel-py-x7x.py",
        &format!(
            "#!/usr/bin/env python3\n{PY_IMPORT}\n\
             def run(**kwargs):\n\
             \x20   os.environ.pop(\"AUTO_CONFIRM\", None)\n\
             \x20   os.environ.pop(\"LLM_AGENT_VAR_AUTO_CONFIRM\", None)\n\
             \x20   prompt_utils.guard_operation(\"Delete? cancel-py-x7x\")\n\
             \x20   return \"reached\\n\"\n"
        ),
    );
    let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Cancelled]));

    let output = run_shim_tool(
        "cancel-py-x7x",
        io_with(&prompter, Arc::new(PromptMemory::default())),
    );

    assert_cancelled(&output, &prompter, "Delete? cancel-py-x7x");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn cancel_fails_the_guarded_typescript_tool_closed() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let guard = TestConfigDirGuard::new("prompt-ts-cancel");
    let _functions = crate::testing::EnvVarGuard::set(
        crate::utils::get_env_name("functions_dir"),
        guard.path.join("functions"),
    );
    install_script_tool(
        TS_HELPER,
        "cancel-ts-x7x.ts",
        &ts_tool(
            runtime,
            "  delete process.env[\"AUTO_CONFIRM\"];\n\
             \x20 delete process.env[\"LLM_AGENT_VAR_AUTO_CONFIRM\"];\n\
             \x20 prompt.guardOperation(\"Delete? cancel-ts-x7x\");\n\
             \x20 return \"reached\\n\";",
        ),
    );
    let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Cancelled]));

    let output = run_shim_tool(
        "cancel-ts-x7x",
        io_with(&prompter, Arc::new(PromptMemory::default())),
    );

    assert_cancelled(&output, &prompter, "Delete? cancel-ts-x7x");
}

/// Runs shim tool `name` with `target` in `GUARD_X7X`.
fn run_guarded(name: &'static str) -> impl Fn(&str, ToolIo) -> Value {
    move |target, io| {
        let _target = crate::testing::EnvVarGuard::set("GUARD_X7X", target);
        run_shim_tool(name, io)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn python_session_yes_covers_only_that_operation_or_path() {
    if !python_available() {
        return;
    }
    let guard = TestConfigDirGuard::new("prompt-py-session");
    let _functions = crate::testing::EnvVarGuard::set(
        crate::utils::get_env_name("functions_dir"),
        guard.path.join("functions"),
    );
    for (tool, call) in [
        ("op-py-x7x", "guard_operation(\"Run?\", target)"),
        ("path-py-x7x", "guard_path(target, \"Touch?\")"),
    ] {
        install_script_tool(
            PY_HELPER,
            &format!("{tool}.py"),
            &format!(
                "#!/usr/bin/env python3\n{PY_IMPORT}\n\
                 def run(**kwargs):\n\
                 \x20   os.environ.pop(\"AUTO_CONFIRM\", None)\n\
                 \x20   os.environ.pop(\"LLM_AGENT_VAR_AUTO_CONFIRM\", None)\n\
                 \x20   target = os.environ[\"GUARD_X7X\"]\n\
                 \x20   prompt_utils.{call}\n\
                 \x20   return \"ok\\n\"\n"
            ),
        );
    }
    let outside = std::env::temp_dir();

    check_session_yes_covers_one_operation(
        "op-py-x7x",
        run_guarded("op-py-x7x"),
        "echo one-x7x",
        "echo two-x7x",
    );
    check_session_yes_covers_one_operation(
        "path-py-x7x",
        run_guarded("path-py-x7x"),
        &outside.join("one-x7x.txt").to_string_lossy(),
        &outside.join("two-x7x.txt").to_string_lossy(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn typescript_session_yes_covers_only_that_operation_or_path() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let guard = TestConfigDirGuard::new("prompt-ts-session");
    let _functions = crate::testing::EnvVarGuard::set(
        crate::utils::get_env_name("functions_dir"),
        guard.path.join("functions"),
    );
    for (tool, call) in [
        ("op-ts-x7x", "guardOperation(\"Run?\", target)"),
        ("path-ts-x7x", "guardPath(target, \"Touch?\")"),
    ] {
        install_script_tool(
            TS_HELPER,
            &format!("{tool}.ts"),
            &ts_tool(
                runtime,
                &format!(
                    "  delete process.env[\"AUTO_CONFIRM\"];\n\
                     \x20 delete process.env[\"LLM_AGENT_VAR_AUTO_CONFIRM\"];\n\
                     \x20 const target = process.env[\"GUARD_X7X\"] ?? \"\";\n\
                     \x20 prompt.{call};\n\
                     \x20 return \"ok\\n\";"
                ),
            ),
        );
    }
    let outside = std::env::temp_dir();

    check_session_yes_covers_one_operation(
        "op-ts-x7x",
        run_guarded("op-ts-x7x"),
        "echo one-x7x",
        "echo two-x7x",
    );
    check_session_yes_covers_one_operation(
        "path-ts-x7x",
        run_guarded("path-ts-x7x"),
        &outside.join("one-x7x.txt").to_string_lossy(),
        &outside.join("two-x7x.txt").to_string_lossy(),
    );
}

struct Run {
    /// Stdout minus the request lines.
    printed: Vec<String>,
    requests: usize,
    /// The `coyote_prompt` object of every request line, in order.
    wire: Vec<Value>,
    stderr: String,
    code: i32,
}

/// Runs `cmd` with a live reply channel; each request line it writes
/// is answered from `answers` in order (`null` once they run out,
/// `None` closes the channel before the first read), preceded by a
/// foreign-id reply when `stray_first` is set.
fn drive(cmd: Command, answers: Option<&[Value]>, stray_first: bool) -> Run {
    drive_env(cmd, answers, stray_first, &[])
}

/// `drive` with extra environment set after the auto-confirm
/// variables are scrubbed, so a case can opt back into one of them.
fn drive_env(
    mut cmd: Command,
    answers: Option<&[Value]>,
    stray_first: bool,
    env: &[(&str, &str)],
) -> Run {
    let mut server = Server::open().unwrap();
    cmd.env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .envs(env.iter().copied())
        .env(REPLY_ENV, server.env_value())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    server.configure(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    server.spawned();
    let server = answers.is_some().then_some(server);
    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
    let mut answers = answers.unwrap_or_default().iter();
    let mut printed = Vec::new();
    let mut requests = 0;
    let mut wire = Vec::new();
    pump_stdout(&mut child, "drive_env", |line| {
        if line.trim_start().as_bytes().starts_with(MARKER) {
            requests += 1;
            let req: Value = serde_json::from_str(&line).unwrap();
            wire.push(req["coyote_prompt"].clone());
            if let Some(server) = server.as_ref() {
                let id = req["coyote_prompt"]["id"].as_str().unwrap();
                if stray_first {
                    server.reply("not-this-request", &json!(false));
                }
                server.reply(id, answers.next().unwrap_or(&Value::Null));
            }
        } else {
            printed.push(line);
        }
    });
    let code = wait_or_kill(&mut child, "drive_env").code().unwrap_or(-1);
    Run {
        printed,
        requests,
        wire,
        stderr: stderr_thread.join().unwrap(),
        code,
    }
}

/// Runs `cmd` with no reply channel in its environment and no
/// controlling terminal (its own session, stdin at /dev/null), the
/// shape of a headless run, and counts any request line it emits.
fn drive_without_channel(mut cmd: Command) -> Run {
    cmd.env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env_remove(REPLY_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let mut printed = Vec::new();
    let mut requests = 0;
    let mut wire = Vec::new();
    for line in out.stdout.lines() {
        let line = line.unwrap();
        if line.trim_start().as_bytes().starts_with(MARKER) {
            requests += 1;
            let req: Value = serde_json::from_str(&line).unwrap();
            wire.push(req["coyote_prompt"].clone());
        } else {
            printed.push(line);
        }
    }
    Run {
        printed,
        requests,
        wire,
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

/// Without a channel or a terminal a helper must stay silent on
/// stdout (no request line) and give the missing answer.
fn check_no_channel(run: Run, none: &str) {
    assert_eq!(run.requests, 0, "request sent without a channel");
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.printed, vec![none], "{}", run.stderr);
}

/// What a language prints for the two booleans, a missing answer,
/// the checkbox indices and a select answered with typed text;
/// everything else prints the same. A select answered with blank
/// text prints the missing answer.
struct Contract {
    confirm_true: &'static str,
    confirm_false: &'static str,
    none: &'static str,
    checked: &'static str,
    typed: &'static str,
}

fn check_contract(eval: impl Fn(&str, Option<&[Value]>, bool) -> Run, shape: &Contract) {
    let confirm = "confirm(\"Go?\")";
    for (answer, printed) in [(true, shape.confirm_true), (false, shape.confirm_false)] {
        let run = eval(confirm, Some(&[json!(answer)]), false);
        assert_eq!(run.code, 0, "{}", run.stderr);
        assert_eq!(run.requests, 1);
        assert_eq!(run.printed, vec![printed]);
        assert!(run.stderr.is_empty(), "{}", run.stderr);
    }

    let select = "select(\"Pick\", [\"a\", \"b\", \"c\"])";
    let run = eval(select, Some(&[json!("b")]), false);
    assert_eq!(run.printed, vec!["1"], "{}", run.stderr);

    let run = eval(
        "checkbox(\"Pick\", [\"a\", \"b\", \"c\"])",
        Some(&[json!(["c", "a"])]),
        false,
    );
    assert_eq!(run.printed, vec![shape.checked], "{}", run.stderr);

    let run = eval(confirm, Some(&[Value::Null]), false);
    assert_eq!(run.printed, vec![shape.none], "{}", run.stderr);
    assert!(run.stderr.is_empty(), "cancel is silent: {}", run.stderr);

    let run = eval(confirm, Some(&[json!(true)]), true);
    assert_eq!(run.printed, vec![shape.confirm_true], "{}", run.stderr);

    let run = eval(select, Some(&[json!("typed by hand")]), false);
    assert_eq!(run.printed, vec![shape.typed], "{}", run.stderr);
    assert!(run.stderr.is_empty(), "{}", run.stderr);

    for blank in ["", "  \t "] {
        let run = eval(select, Some(&[json!(blank)]), false);
        assert_eq!(run.printed, vec![shape.none], "{blank:?}: {}", run.stderr);
        assert!(
            run.stderr.is_empty(),
            "blank is a silent cancel: {}",
            run.stderr
        );
    }

    let run = eval(confirm, None, false);
    assert_eq!(run.printed, vec![shape.none], "{}", run.stderr);
    assert!(
        run.stderr.contains("prompt reply channel closed"),
        "{}",
        run.stderr
    );
}

mod py_contract;

mod ts_contract;

/// One guard call evaluated with `env` added and `cwd` as the working
/// directory; `continued` is what the language prints for the guard's
/// return value when it lets the tool carry on.
type GuardEval<'a> = &'a dyn Fn(&str, Option<&[Value]>, &[(&str, &str)], &Path) -> Run;

/// The bash guards' contract: the auto-confirm variables short-circuit
/// before any request is sent, anything but an exact `true` aborts
/// with exit 1 and `Operation aborted!` on stderr, and `guard_path`
/// stays silent for paths inside the working directory. Like bash's,
/// the containment check compares path components, so a sibling
/// sharing the prefix still asks; unlike bash's it looks at where a
/// path really lands, so a new file under a symlink that points
/// outside asks too. Like bash, a guard's question names the
/// operation or the resolved path it guards.
fn check_guards(eval: GuardEval<'_>, continued: &str, cwd: &Path) {
    fs::create_dir_all(cwd.join("sub")).unwrap();
    let sibling = PathBuf::from(format!("{}-sibling", cwd.display()));
    fs::create_dir_all(&sibling).unwrap();
    let _sibling_guard = RemoveOnDrop(sibling.clone());
    std::os::unix::fs::symlink(&sibling, cwd.join("link")).unwrap();
    std::os::unix::fs::symlink(sibling.join("missing.txt"), cwd.join("dangling")).unwrap();
    std::os::unix::fs::symlink(&sibling, cwd.join("files")).unwrap();
    let op = "guard_operation(\"Really?\")";

    let run = eval(op, Some(&[json!(true)]), &[], cwd);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 1);
    assert_eq!(run.printed, vec![continued]);
    assert!(run.stderr.is_empty(), "{}", run.stderr);

    for (label, answers) in [
        ("false", Some(&[json!(false)][..])),
        ("cancelled", Some(&[Value::Null][..])),
        ("string yes", Some(&[json!("yes")][..])),
        ("channel closed", None),
    ] {
        let run = eval(op, answers, &[], cwd);
        assert_eq!(run.code, 1, "{label}: {}", run.stderr);
        assert_eq!(run.requests, 1, "{label}");
        assert!(run.printed.is_empty(), "{label}: {:?}", run.printed);
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{label}: {}",
            run.stderr
        );
    }

    for var in ["AUTO_CONFIRM", "LLM_AGENT_VAR_AUTO_CONFIRM"] {
        let run = eval(op, Some(&[json!(false)]), &[(var, "1")], cwd);
        assert_eq!(run.code, 0, "{var}: {}", run.stderr);
        assert_eq!(
            run.requests, 0,
            "{var} must short-circuit before any request"
        );
        assert_eq!(run.printed, vec![continued], "{var}");
        assert!(run.stderr.is_empty(), "{var}: {}", run.stderr);
    }

    let run = eval(
        "guard_operation(\"Run?\", \"rm -rf build\")",
        Some(&[json!(true)]),
        &[],
        cwd,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.wire[0]["question"], "Run?\nrm -rf build");

    let run = eval(
        "guard_operation(\"\", \"ls\")",
        Some(&[json!(true)]),
        &[],
        cwd,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.wire[0]["question"],
        "Are you sure you want to continue?\nls"
    );

    let resolved = fs::canonicalize(cwd)
        .unwrap()
        .parent()
        .unwrap()
        .join("outside.txt");
    let named = format!("Touch {}?", resolved.display());
    for (question, sent) in [
        ("Touch?", format!("Touch?\n{}", resolved.display())),
        (
            "Touch ../outside.txt?",
            format!("Touch ../outside.txt?\n{}", resolved.display()),
        ),
        (named.as_str(), named.clone()),
    ] {
        let expr = format!("guard_path(\"../outside.txt\", \"{question}\")");
        let run = eval(&expr, Some(&[json!(true)]), &[], cwd);
        assert_eq!(run.code, 0, "{question}: {}", run.stderr);
        assert_eq!(run.wire[0]["question"], sent.as_str(), "{question}");
    }

    // `files` is a link out of the working directory, and the
    // wording happens to contain the argument as given.
    let run = eval(
        "guard_path(\"files\", \"Delete files?\")",
        Some(&[json!(true)]),
        &[],
        cwd,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.wire[0]["question"],
        format!(
            "Delete files?\n{}",
            fs::canonicalize(&sibling).unwrap().display()
        )
    );

    let inside = [
        "sub/file.txt".to_string(),
        "sub/../sub/./x".to_string(),
        ".".to_string(),
        cwd.join("sub").display().to_string(),
    ];
    for path in &inside {
        let run = eval(
            &format!("guard_path(\"{path}\", \"Touch?\")"),
            Some(&[json!(false)]),
            &[],
            cwd,
        );
        assert_eq!(run.code, 0, "{path}: {}", run.stderr);
        assert_eq!(run.requests, 0, "{path} is inside cwd and must not ask");
        assert_eq!(run.printed, vec![continued], "{path}");
    }

    let outside = [
        "../outside.txt".to_string(),
        "sub/../../x".to_string(),
        format!("{}-sibling/f", cwd.display()),
        "/".to_string(),
        "link/new.txt".to_string(),
        "link/../escaped.txt".to_string(),
        "dangling".to_string(),
    ];
    for path in &outside {
        let expr = format!("guard_path(\"{path}\", \"Touch?\")");
        let run = eval(&expr, Some(&[json!(true)]), &[], cwd);
        assert_eq!(run.code, 0, "{path}: {}", run.stderr);
        assert_eq!(run.requests, 1, "{path} is outside cwd and must ask");
        assert_eq!(run.printed, vec![continued], "{path}");

        let run = eval(&expr, Some(&[json!(false)]), &[], cwd);
        assert_eq!(run.code, 1, "{path}: {}", run.stderr);
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{path}: {}",
            run.stderr
        );

        let run = eval(&expr, Some(&[json!(false)]), &[("AUTO_CONFIRM", "1")], cwd);
        assert_eq!(run.code, 0, "{path}: {}", run.stderr);
        assert_eq!(
            run.requests, 0,
            "{path}: AUTO_CONFIRM must skip the question"
        );
    }
}

/// Replies that fit no action: a string for `confirm`, an unknown
/// option inside a `checkbox` list, a number for `input`. Each prints
/// the missing answer and names the problem on stderr.
fn check_unusable_shapes(eval: impl Fn(&str, Option<&[Value]>, bool) -> Run, none: &str) {
    for (expr, answer) in [
        ("confirm(\"Go?\")", json!("yes")),
        (
            "checkbox(\"Pick\", [\"a\", \"b\", \"c\"])",
            json!(["a", "zzz"]),
        ),
        ("input(\"Name?\")", json!(42)),
        ("select(\"Pick\", [\"a\", \"b\", \"c\"])", json!(1)),
    ] {
        let run = eval(expr, Some(std::slice::from_ref(&answer)), false);
        assert_eq!(run.code, 0, "{expr}: {}", run.stderr);
        assert_eq!(run.printed, vec![none], "{expr} with {answer}");
        assert!(
            run.stderr.contains("prompt reply unusable"),
            "{expr}: {}",
            run.stderr
        );
    }
}

/// The request line carries the tool's text verbatim however awkward
/// it is (quotes, a newline, non-ASCII, a backslash), as one JSON
/// line the host can parse, and every prompt in a process mints a
/// fresh `p<pid>-<16 hex>` id. `second` is how the language names
/// the helper module for a second call inside one expression.
fn check_wire(
    eval: impl Fn(&str, Option<&[Value]>, bool) -> Run,
    second: &str,
    text_printed: &str,
    one: &str,
) {
    let question = "He said \"hi\"\nline2 \u{e9}\u{2713} \\ back";
    let run = eval(
        "input(\"He said \\\"hi\\\"\\nline2 \\u00e9\\u2713 \\\\ back\")",
        Some(&[json!("x")]),
        false,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 1, "{:?}", run.wire);
    assert_eq!(run.wire[0]["action"], "input");
    assert_eq!(run.wire[0]["question"], question, "{}", run.wire[0]);
    assert_eq!(run.wire[0]["options"], json!([]));
    assert_eq!(run.printed, vec![text_printed], "{}", run.stderr);

    let run = eval(
        "select(\"Pick\", [\"a\", \"b\\\"q\", \"\\u00e9\"])",
        Some(&[json!("b\"q")]),
        false,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.wire[0]["options"], json!(["a", "b\"q", "\u{e9}"]));
    assert_eq!(run.printed, vec![one], "{}", run.stderr);

    let run = eval(
        &format!("confirm(\"A\"), {second}confirm(\"B\")"),
        Some(&[json!(true), json!(false)]),
        false,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 2, "{:?}", run.wire);
    let ids: Vec<&str> = run
        .wire
        .iter()
        .map(|req| req["id"].as_str().unwrap())
        .collect();
    assert_ne!(ids[0], ids[1], "two prompts shared an id");
    for id in &ids {
        let (pid, hex) = id
            .strip_prefix('p')
            .and_then(|rest| rest.split_once('-'))
            .unwrap_or_else(|| panic!("id {id} is not p<pid>-<hex>"));
        assert!(
            !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()),
            "{id}"
        );
        assert!(
            hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "{id}"
        );
    }
    assert_eq!(
        ids[0].split_once('-').unwrap().0,
        ids[1].split_once('-').unwrap().0
    );
    assert_eq!(run.wire[0]["question"], "A");
    assert_eq!(run.wire[1]["question"], "B");
}

/// `inner`, run by a bash tool inside `$(...)` after it sourced
/// prompt-utils.sh: the request must leave through the fd the bash
/// helper saved (`_COYOTE_PROMPT_OUT`), not the capture, and the
/// helper's answer must be what the capture receives.
fn nested_in_bash(inner: &Command) -> Command {
    fn quote(arg: &std::ffi::OsStr) -> String {
        format!("'{}'", arg.to_str().unwrap().replace('\'', "'\\''"))
    }
    let words: Vec<String> = std::iter::once(inner.get_program())
        .chain(inner.get_args())
        .map(quote)
        .collect();
    let mut cmd = Command::new(bash_program().unwrap());
    for (key, value) in inner.get_envs() {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    cmd.arg("-c").arg(format!(
        "source {}; out=\"$({})\"; echo \"captured: $out\"",
        quote(HELPER.as_ref()),
        words.join(" ")
    ));
    cmd
}

fn check_nested_in_bash(inner: &Command, one: &str) {
    let run = drive(nested_in_bash(inner), Some(&[json!("b")]), false);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 1, "{:?}", run.wire);
    assert_eq!(run.wire[0]["action"], "select");
    assert_eq!(
        run.printed,
        vec![format!("captured: {one}")],
        "{}",
        run.stderr
    );
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

/// Like `drive`, but every reply is preceded, in the same write, by
/// lines the helper must skip: text that is not JSON, a blank line,
/// an object without an id and a reply for another id. The bash
/// helper loops until `.id` matches; the twins must do the same.
fn drive_through_junk(mut cmd: Command, answers: &[Value]) -> Run {
    let mut server = Server::open().unwrap();
    cmd.env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env(REPLY_ENV, server.env_value())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    server.configure(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    server.spawned();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
    let mut answers = answers.iter();
    let mut printed = Vec::new();
    let mut requests = 0;
    let mut wire = Vec::new();
    pump_stdout(&mut child, "drive_through_junk", |line| {
        if line.trim_start().as_bytes().starts_with(MARKER) {
            requests += 1;
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req["coyote_prompt"]["id"].as_str().unwrap().to_string();
            wire.push(req["coyote_prompt"].clone());
            let answer = answers.next().unwrap_or(&Value::Null);
            let batch = format!(
                "this is not json\n\n{{\"answer\":true}}\n\
                 {{\"id\":\"p1-0000000000000000\",\"answer\":false}}\n\
                 {}\n",
                json!({"id": id, "answer": answer})
            );
            let mut writer = &server.writer;
            writer.write_all(batch.as_bytes()).unwrap();
            writer.flush().unwrap();
        } else {
            printed.push(line);
        }
    });
    let code = wait_or_kill(&mut child, "drive_through_junk")
        .code()
        .unwrap_or(-1);
    Run {
        printed,
        requests,
        wire,
        stderr: stderr_thread.join().unwrap(),
        code,
    }
}

/// `drive_without_channel` with `COYOTE_PROMPT_REPLY` naming the fd
/// the bash helper reserves for its saved stdout, which every helper
/// treats as no channel at all.
fn drive_with_reserved_fd(mut cmd: Command) -> Run {
    cmd.env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env(REPLY_ENV, "/dev/fd/9")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    let printed = out
        .stdout
        .lines()
        .map(Result::unwrap)
        .filter(|line| !line.trim_start().as_bytes().starts_with(MARKER))
        .collect();
    let requests = out
        .stdout
        .lines()
        .map(Result::unwrap)
        .filter(|line| line.trim_start().as_bytes().starts_with(MARKER))
        .count();
    Run {
        printed,
        requests,
        wire: Vec::new(),
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

/// Edges of the wire contract the broad checks leave out: junk on
/// the channel is skipped until our id; options are matched by text
/// (a duplicate yields its first index, a dash or empty option is an
/// option like any other); an empty checkbox list and an empty input
/// are valid answers, not cancels; a number is unusable for confirm
/// and a bare string for checkbox; `guard_operation` asks its default
/// question when given none.
fn check_edges(
    command: impl Fn(&str) -> Command,
    none: &str,
    confirm_true: &str,
    empty_list: &str,
    empty_text: &str,
) {
    let run = drive_through_junk(command("confirm(\"Go?\")"), &[json!(true)]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 1, "{:?}", run.wire);
    assert_eq!(run.printed, vec![confirm_true], "{}", run.stderr);
    assert!(run.stderr.is_empty(), "{}", run.stderr);

    for (expr, answer, printed) in [
        ("select(\"Pick\", [\"dup\", \"dup\"])", json!("dup"), "0"),
        (
            "select(\"Pick\", [\"-x\", \"--y\", \"\"])",
            json!("--y"),
            "1",
        ),
        ("select(\"Pick\", [\"-x\", \"--y\", \"\"])", json!(""), "2"),
        ("checkbox(\"Pick\", [\"a\", \"b\"])", json!([]), empty_list),
        ("input(\"Name?\")", json!(""), empty_text),
    ] {
        let run = drive(command(expr), Some(std::slice::from_ref(&answer)), false);
        assert_eq!(run.code, 0, "{expr}: {}", run.stderr);
        assert_eq!(
            run.printed,
            vec![printed],
            "{expr} with {answer}: {}",
            run.stderr
        );
        assert!(run.stderr.is_empty(), "{expr}: {}", run.stderr);
    }

    for (expr, answer) in [
        ("confirm(\"Go?\")", json!(1)),
        ("checkbox(\"Pick\", [\"a\", \"b\"])", json!("a")),
    ] {
        let run = drive(command(expr), Some(std::slice::from_ref(&answer)), false);
        assert_eq!(run.code, 0, "{expr}: {}", run.stderr);
        assert_eq!(run.printed, vec![none], "{expr} with {answer}");
        assert!(
            run.stderr.contains("prompt reply unusable"),
            "{expr}: {}",
            run.stderr
        );
    }

    let run = drive(command("guard_operation()"), Some(&[json!(true)]), false);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 1, "{:?}", run.wire);
    assert_eq!(run.wire[0]["action"], "confirm");
    assert_eq!(
        run.wire[0]["question"], "Are you sure you want to continue?",
        "{}",
        run.wire[0]
    );

    check_no_channel(drive_with_reserved_fd(command("confirm(\"Go?\")")), none);
}

/// `drive` with a reply channel the host holds open, answering each
/// request with the raw line `line(id)` instead of a well-formed
/// reply, so a case can send an object the host never would.
fn drive_raw(mut cmd: Command, line: impl Fn(&str) -> String) -> Run {
    let mut server = Server::open().unwrap();
    cmd.env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env(REPLY_ENV, server.env_value())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    server.configure(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    server.spawned();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
    let mut printed = Vec::new();
    let mut requests = 0;
    let mut wire = Vec::new();
    pump_stdout(&mut child, "drive_raw", |out| {
        if out.trim_start().as_bytes().starts_with(MARKER) {
            requests += 1;
            let req: Value = serde_json::from_str(&out).unwrap();
            let id = req["coyote_prompt"]["id"].as_str().unwrap().to_string();
            wire.push(req["coyote_prompt"].clone());
            let mut writer = &server.writer;
            writer.write_all(line(&id).as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();
        } else {
            printed.push(out);
        }
    });
    let code = wait_or_kill(&mut child, "drive_raw").code().unwrap_or(-1);
    Run {
        printed,
        requests,
        wire,
        stderr: stderr_thread.join().unwrap(),
        code,
    }
}

/// Runs `cmd` with `COYOTE_PROMPT_REPLY` naming `reply`, a path the
/// helper cannot open, in its own session with no terminal. Nothing
/// answers a request here, so a helper that sends one before it
/// finds the channel dead would block; the child is killed after
/// ten seconds and the exit code reports it.
fn drive_with_dead_reply(mut cmd: Command, reply: &str) -> Run {
    cmd.env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env(REPLY_ENV, reply)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_thread = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).unwrap();
        text
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
        text
    });
    let status = wait_or_kill(&mut child, "drive_with_dead_reply");
    let stdout = stdout_thread.join().unwrap();
    let stderr = stderr_thread.join().unwrap();
    let mut printed = Vec::new();
    let mut requests = 0;
    for out in stdout.lines() {
        if out.trim_start().as_bytes().starts_with(MARKER) {
            requests += 1;
        } else {
            printed.push(out.to_string());
        }
    }
    Run {
        printed,
        requests,
        wire: Vec::new(),
        code: status.code().unwrap_or(-1),
        stderr,
    }
}

/// Edges the bash contract pins that the broad checks leave out for
/// the other two languages: a reply path the helper cannot open (a
/// missing file, an fd the tool never inherited) or a request fd it
/// never inherited fails closed without a request and without
/// blocking, and a guard over it still aborts; a reply for our id
/// with no `answer` key is a cancel, not an error; a checkbox answer
/// that repeats an option yields each index once.
fn check_probe_edges(
    command: impl Fn(&str) -> Command,
    none: &str,
    checked_once: &str,
    dir: &Path,
) {
    let missing = dir.join("missing").join("reply");
    let dead = dead_reply_path();
    for reply in [missing.to_str().unwrap(), dead.as_str()] {
        let run = drive_with_dead_reply(command("confirm(\"Go?\")"), reply);
        assert_eq!(run.requests, 0, "{reply}: request sent over a dead channel");
        assert_eq!(run.code, 0, "{reply}: {}", run.stderr);
        assert_eq!(run.printed, vec![none], "{reply}: {}", run.stderr);
        assert_eq!(
            run.stderr.trim_end(),
            "prompt request channel unavailable",
            "{reply}: a dead channel is reported on stderr: {}",
            run.stderr
        );

        let run = drive_with_dead_reply(command("guard_operation(\"Really?\")"), reply);
        assert_eq!(run.requests, 0, "{reply}: request sent over a dead channel");
        assert_eq!(run.code, 1, "{reply}: a guard over a dead channel aborts");
        assert!(
            run.stderr.contains("Operation aborted!"),
            "{reply}: {}",
            run.stderr
        );
    }

    // The request descriptor the test points at is dead the same
    // way the reply one is: a number past the process limit.
    let dead_out_fd = dead_fd().to_string();
    let dead_out = [(OUT_FD_ENV, dead_out_fd.as_str())];
    let run = drive_env(command("confirm(\"Go?\")"), Some(&[]), false, &dead_out);
    assert_eq!(run.requests, 0, "request sent over a dead request fd");
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.printed, vec![none], "{}", run.stderr);
    assert_eq!(
        run.stderr.trim_end(),
        "prompt request channel unavailable",
        "a dead request fd is reported on stderr: {}",
        run.stderr
    );

    let run = drive_env(
        command("guard_operation(\"Go?\")"),
        Some(&[]),
        false,
        &dead_out,
    );
    assert_eq!(run.requests, 0, "request sent over a dead request fd");
    assert_eq!(run.code, 1, "a guard over a dead request fd aborts");
    assert!(run.stderr.contains("Operation aborted!"), "{}", run.stderr);

    let run = drive_raw(command("confirm(\"Go?\")"), |id| {
        format!("{{\"id\":\"{id}\"}}")
    });
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.requests, 1, "{:?}", run.wire);
    assert_eq!(run.printed, vec![none], "{}", run.stderr);
    assert!(
        run.stderr.is_empty(),
        "a reply without an answer is a silent cancel: {}",
        run.stderr
    );

    let run = drive(
        command("checkbox(\"Pick\", [\"a\", \"b\", \"c\"])"),
        Some(&[json!(["a", "a"])]),
        false,
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.printed, vec![checked_once], "{}", run.stderr);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

/// A reply path that opens but cannot be read (a directory) fails
/// closed like a dead one: the helper returns its "none", notes the
/// channel on stderr, and never surfaces a traceback or stack trace
/// to the tool. Where the failure lands depends on the runtime:
/// Python's `open` refuses a directory up front, so no request
/// escapes and the note is the request-channel one; node's
/// `openSync` succeeds and `readSync` fails, so one request escapes
/// and the note is the reply-channel one. A guard over it aborts.
fn check_directory_reply(command: impl Fn(&str) -> Command, none: &str, dir: &Path) {
    let reply_dir = dir.join("reply-is-a-directory");
    fs::create_dir_all(&reply_dir).unwrap();
    let reply = reply_dir.to_str().unwrap();

    let run = drive_with_dead_reply(command("confirm(\"Go?\")"), reply);
    assert_eq!(run.code, 0, "directory reply path: {}", run.stderr);
    assert_eq!(
        run.printed,
        vec![none],
        "directory reply path: {}",
        run.stderr
    );
    assert!(
        run.requests <= 1,
        "directory reply path: {} requests escaped",
        run.requests
    );
    let expected_note = if run.requests == 1 {
        "prompt reply channel closed"
    } else {
        "prompt request channel unavailable"
    };
    assert!(
        run.stderr.contains(expected_note),
        "directory reply path: expected {expected_note:?} on stderr: {}",
        run.stderr
    );
    for leak in ["Traceback", "IsADirectoryError", "EISDIR", "\n    at "] {
        assert!(
            !run.stderr.contains(leak),
            "directory reply path leaked {leak:?} to stderr: {}",
            run.stderr
        );
    }

    let run = drive_with_dead_reply(command("guard_operation(\"Really?\")"), reply);
    assert_eq!(run.code, 1, "a guard over a directory reply path aborts");
    assert!(
        run.stderr.contains("Operation aborted!"),
        "directory reply path: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("Traceback") && !run.stderr.contains("\n    at "),
        "directory reply path leaked a stack trace: {}",
        run.stderr
    );
}
