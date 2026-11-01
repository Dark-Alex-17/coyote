//! Real-process checks of the tool prompt protocol: each shipped helper
//! (bash, Python, TypeScript) falls back to `/dev/tty` when no reply channel
//! is configured, and a `--macro` run of the real binary tells its tools a
//! human is present (`COYOTE_UI=repl`) and exports the helper paths. Every
//! case goes through a bash wrapper, so the file is unix-only.
#![cfg(unix)]

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process, thread};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::Pty;

const HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.sh"
);
const PY_HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.py"
);
const TS_HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.ts"
);
const PTY_DEADLINE: Duration = Duration::from_secs(10);
const RUN_DEADLINE: Duration = Duration::from_secs(120);
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Without a reply channel `confirm` asks on the controlling terminal: it
/// prints the question, queries the cursor row with `ESC[6n` and reads one
/// key. Stdin is pointed at /dev/null first so only the terminal can
/// deliver the key. No protocol line may be written in that mode.
#[test]
fn confirm_without_channel_reads_the_controlling_terminal() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; v=\"$(confirm \"Go? tty-x7x\")\"; echo \"confirm-result=$v\""
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for("(y/N)", PTY_DEADLINE).unwrap();
    pty.send(b"\x1b[5;1R").unwrap();
    pty.send(b"y").unwrap();
    let transcript = pty.wait_for("confirm-result=1", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("coyote_prompt"),
        "confirm wrote a protocol line without a channel:\n{transcript}"
    );
}

/// The Python helper in the same situation: stdin is /dev/null, so the
/// answer can only arrive through the controlling terminal. It reads a
/// whole line and sends no cursor query.
#[test]
fn python_confirm_without_channel_reads_the_controlling_terminal() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let driver = "import importlib.util, sys\n\
        sys.dont_write_bytecode = True\n\
        spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
        m = importlib.util.module_from_spec(spec)\n\
        spec.loader.exec_module(m)\n\
        print(\"confirm-result=\" + str(m.confirm(\"Go? tty-py-x7x\")))\n";
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; python3 -c '{driver}' \"{PY_HELPER}\""
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for("(y/N)", PTY_DEADLINE).unwrap();
    pty.send(b"y\n").unwrap();
    let transcript = pty.wait_for("confirm-result=True", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("coyote_prompt"),
        "confirm wrote a protocol line without a channel:\n{transcript}"
    );
}

/// The TypeScript helper in the same situation, driven by whichever
/// runtime can execute `.ts` files here.
#[test]
fn typescript_confirm_without_channel_reads_the_controlling_terminal() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(
        &driver,
        "import { pathToFileURL } from \"node:url\";\n\
         const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
         console.log(\"confirm-result=\" + JSON.stringify(prompt.confirm(\"Go? tty-ts-x7x\")));\n",
    )
    .unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for("(y/N)", PTY_DEADLINE).unwrap();
    pty.send(b"y\n").unwrap();
    let transcript = pty.wait_for("confirm-result=true", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("coyote_prompt"),
        "confirm wrote a protocol line without a channel:\n{transcript}"
    );
}

/// The Python helper's `select` fallback lists each option on its own
/// line and reads the pick from the terminal.
#[test]
fn python_select_without_channel_reads_the_controlling_terminal() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let driver = "import importlib.util, sys\n\
        sys.dont_write_bytecode = True\n\
        spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
        m = importlib.util.module_from_spec(spec)\n\
        spec.loader.exec_module(m)\n\
        print(\"select-result=\" + str(m.select(\"Pick? sel-x7x\", [\"a\", \"b\"])))\n";
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; python3 -c '{driver}' \"{PY_HELPER}\""
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let listing = pty.wait_for("2) b", PTY_DEADLINE).unwrap();
    assert_options_on_separate_lines(&listing);
    pty.send(b"2\n").unwrap();
    let transcript = pty.wait_for("select-result=1", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("coyote_prompt"),
        "select wrote a protocol line without a channel:\n{transcript}"
    );
}

/// The TypeScript helper's `select` fallback, same shape.
#[test]
fn typescript_select_without_channel_reads_the_controlling_terminal() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-select");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(
        &driver,
        "import { pathToFileURL } from \"node:url\";\n\
         const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
         console.log(\"select-result=\" + JSON.stringify(prompt.select(\"Pick? sel-x7x\", [\"a\", \"b\"])));\n",
    )
    .unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let listing = pty.wait_for("2) b", PTY_DEADLINE).unwrap();
    assert_options_on_separate_lines(&listing);
    pty.send(b"2\n").unwrap();
    let transcript = pty.wait_for("select-result=1", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("coyote_prompt"),
        "select wrote a protocol line without a channel:\n{transcript}"
    );
}

fn assert_options_on_separate_lines(transcript: &str) {
    let holds = |needle: &str| transcript.lines().any(|line| line.contains(needle));
    assert!(holds("1) a"), "first option missing:\n{transcript}");
    assert!(holds("2) b"), "second option missing:\n{transcript}");
    assert!(
        !transcript
            .lines()
            .any(|line| line.contains("1) a") && line.contains("2) b")),
        "options rendered on one line:\n{transcript}"
    );
}

/// `--macro` without `--headless` keeps a human in the loop, and the tool
/// sees that as `COYOTE_UI=repl`.
#[test]
fn macro_run_tells_tools_a_human_is_present() {
    let dir = fresh_config_dir("ui");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_ui".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_ui_fixture(&dir, &fake);

    let output = run_coyote(&dir, &["--macro", "ui"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("fixture ui: repl"),
        "the tool did not see COYOTE_UI=repl\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(fake.served(), 2, "stdout:\n{stdout}");
}

/// A Python tool run by the real binary finds `LLM_PROMPT_UTILS_PY` naming
/// an absolute, existing file that the documented import idiom loads with
/// all seven helpers, and sees `COYOTE_UI=repl` under `--macro`.
#[test]
fn macro_run_exports_the_python_prompt_helper_to_python_tools() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("helper-py");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_helper_py".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_helper_py.py",
        "#!/usr/bin/env python3\n\
         import importlib.util\n\
         import os\n\
         \n\
         \n\
         def run():\n\
         \x20   \"\"\"Report the prompt helper the host exported.\"\"\"\n\
         \x20   path = os.environ.get(\"LLM_PROMPT_UTILS_PY\", \"\")\n\
         \x20   spec = importlib.util.spec_from_file_location(\"prompt_utils\", path)\n\
         \x20   prompt_utils = importlib.util.module_from_spec(spec)\n\
         \x20   spec.loader.exec_module(prompt_utils)\n\
         \x20   names = (\"confirm\", \"select\", \"checkbox\", \"input\", \"password\", \"guard_operation\", \"guard_path\")\n\
         \x20   helpers = sum(1 for n in names if callable(getattr(prompt_utils, n, None)))\n\
         \x20   line = f\"fixture py: abs={os.path.isabs(path)} file={os.path.isfile(path)} helpers={helpers} ui={os.environ.get('COYOTE_UI', 'unset')}\"\n\
         \x20   print(line, flush=True)\n\
         \x20   return line\n",
    );
    write_macro(&dir, "helper", "Call the fixture helper py tool");

    let output = run_coyote(&dir, &["--macro", "helper"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("fixture py: abs=True file=True helpers=7 ui=repl"),
        "the python tool did not get a loadable LLM_PROMPT_UTILS_PY and COYOTE_UI=repl\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(fake.served(), 2, "stdout:\n{stdout}");
}

/// The TypeScript twin: `LLM_PROMPT_UTILS_TS` names an absolute, existing
/// file that `await import(...)` loads with every helper (camelCase and
/// snake_case guards both), and the tool sees `COYOTE_UI=repl`.
#[test]
fn macro_run_exports_the_typescript_prompt_helper_to_typescript_tools() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("helper-ts");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_helper_ts".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_helper_ts.ts",
        &format!(
            "#!/usr/bin/env {runtime}\n\
             import * as fs from \"node:fs\";\n\
             import * as path from \"node:path\";\n\
             import {{ pathToFileURL }} from \"node:url\";\n\
             \n\
             /**\n\
             \x20* Report the prompt helper the host exported.\n\
             \x20*/\n\
             export async function run(): Promise<string> {{\n\
             \x20 const p = process.env.LLM_PROMPT_UTILS_TS ?? \"\";\n\
             \x20 const prompt = await import(pathToFileURL(p).href);\n\
             \x20 const names = [\"confirm\", \"select\", \"checkbox\", \"input\", \"password\", \"guard_operation\", \"guard_path\", \"guardOperation\", \"guardPath\"];\n\
             \x20 const helpers = names.filter((n) => typeof (prompt as any)[n] === \"function\").length;\n\
             \x20 const line = `fixture ts: abs=${{path.isAbsolute(p)}} file=${{fs.existsSync(p)}} helpers=${{helpers}} ui=${{process.env.COYOTE_UI ?? \"unset\"}}`;\n\
             \x20 console.log(line);\n\
             \x20 return line;\n\
             }}\n"
        ),
    );
    write_macro(&dir, "helper", "Call the fixture helper ts tool");

    let output = run_coyote(&dir, &["--macro", "helper"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("fixture ts: abs=true file=true helpers=9 ui=repl"),
        "the typescript tool did not get a loadable LLM_PROMPT_UTILS_TS and COYOTE_UI=repl\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(fake.served(), 2, "stdout:\n{stdout}");
}

/// The command that runs a `.ts` tool: `node` when it strips types itself,
/// else `tsx`; None, with the reason printed, when neither is available.
fn ts_runtime() -> Option<&'static str> {
    let dir = fresh_config_dir("ts-probe");
    let _guard = TempDirGuard(dir.clone());
    let probe = dir.join("probe.ts");
    fs::write(&probe, "const n: number = 1;\nconsole.log(n);\n").unwrap();
    let node_runs_ts = Command::new("node")
        .arg(&probe)
        .env("NODE_NO_WARNINGS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if node_runs_ts {
        return Some("node");
    }
    if which::which("tsx").is_ok() {
        return Some("tsx");
    }
    eprintln!("skipping: node cannot run TypeScript natively and tsx is not installed");
    None
}

fn fresh_config_dir(scenario: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-prompt-protocol-{scenario}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// Lays out a config dir whose only model is served by `fake`, with one
/// bash tool that reports the UI mode it was given.
fn write_ui_fixture(dir: &Path, fake: &FakeLlm) {
    write_config(dir, fake);
    write_tool(
        dir,
        "fixture_ui.sh",
        "#!/usr/bin/env bash\n\
         set -e\n\
         \n\
         # @describe Report the UI mode the host advertised.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {\n\
         \x20   echo \"fixture ui: ${COYOTE_UI:-unset}\"\n\
         \x20   echo \"fixture ui: ${COYOTE_UI:-unset}\" >> \"$LLM_OUTPUT\"\n\
         }\n",
    );
    write_macro(dir, "ui", "Call the fixture ui tool");
}

/// Writes a config whose only model is served by `fake`.
fn write_config(dir: &Path, fake: &FakeLlm) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai-compatible\n\
             \x20   name: fake\n\
             \x20   api_base: '{api_base}'\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: fake-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
            api_base = fake.api_base(),
        ),
    )
    .unwrap();
}

/// Drops one tool source into the config's `functions/tools` dir; the
/// binary builds its shim on startup.
fn write_tool(dir: &Path, file: &str, body: &str) {
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(file), body).unwrap();
}

fn write_macro(dir: &Path, name: &str, step: &str) {
    fs::write(
        dir.join(format!("macros/{name}.yaml")),
        format!("steps:\n  - \"{step}\"\n"),
    )
    .unwrap();
}

fn run_coyote(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("COYOTE_SHOW_TOOL_CALLS", "1")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("COLUMNS", "100")
        .env("LINES", "40")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        // Same rule as the in-crate probe: a TS shim under node must not fail on warnings.
        .env("NODE_NO_WARNINGS", "1")
        .env("PATH", path)
        .env_remove("COLORTERM")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = common::pty::detached(&mut cmd).spawn().unwrap();
    let pid = child.id();
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + RUN_DEADLINE;
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                kill_session_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let (stdout, stdout_drained) = collect(stdout);
    let (stderr, stderr_drained) = collect(stderr);
    let Some(status) = status else {
        panic!(
            "coyote {args:?} did not exit within {RUN_DEADLINE:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    };
    // A pipe still open after the child exited belongs to something it
    // spawned; take the group down before failing so it cannot outlive
    // the test.
    if !(stdout_drained && stderr_drained) {
        kill_session_group(pid);
    }
    assert!(
        stdout_drained && stderr_drained,
        "coyote {args:?}: a pipe was still held open {DRAIN_DEADLINE:?} after exit\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    Output {
        status,
        stdout,
        stderr,
    }
}

/// The child runs in its own session (see `common::pty::detached`), so the
/// whole group is the child and whatever it spawned.
fn kill_session_group(pid: u32) {
    // SAFETY: kill with a negative pid signals a process group and touches
    // no memory; the group is the one `detached` created for this child, and
    // its id stays reserved while any member is alive, so it cannot have
    // been recycled for an unrelated process.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn drain(mut stream: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let _ = tx.send(bytes);
    });
    rx
}

/// The drained bytes and whether EOF arrived in time; a stream still held
/// open past the deadline yields nothing rather than hanging the test.
fn collect(rx: mpsc::Receiver<Vec<u8>>) -> (Vec<u8>, bool) {
    match rx.recv_timeout(DRAIN_DEADLINE) {
        Ok(bytes) => (bytes, true),
        Err(mpsc::RecvTimeoutError::Timeout) => (Vec::new(), false),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("drain thread died before EOF"),
    }
}
