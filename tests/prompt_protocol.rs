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
        .env_remove("COYOTE_UI")
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
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
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
        .env_remove("COYOTE_UI")
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

const TUI_NO_CHANNEL: &str = "prompts are not available in the TUI without the reply channel";

/// Under the full-screen UI the terminal belongs to the UI's own reader,
/// so without a reply channel the bash helper fails closed: the message
/// on stderr, a non-zero exit, and no `(y/N)` ever asked on the terminal.
#[test]
fn under_the_tui_confirm_without_channel_fails_closed() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; v=\"$(confirm \"Go? tui-x7x\")\"; echo \"confirm-rc=$? v=[$v]\""
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("confirm-rc=", PTY_DEADLINE).unwrap();
    assert_fails_closed_under_the_tui(&transcript, "confirm-rc=1 v=[]");
}

/// The Python helper, same shape: `confirm` exits the process non-zero.
#[test]
fn under_the_tui_python_confirm_without_channel_fails_closed() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let driver = "import importlib.util, sys\n\
        sys.dont_write_bytecode = True\n\
        spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
        m = importlib.util.module_from_spec(spec)\n\
        spec.loader.exec_module(m)\n\
        print(\"confirm-result=\" + str(m.confirm(\"Go? tui-py-x7x\")))\n";
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; python3 -c '{driver}' \"{PY_HELPER}\"; echo \"confirm-rc=$?\""
        ))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("confirm-rc=", PTY_DEADLINE).unwrap();
    assert_fails_closed_under_the_tui(&transcript, "confirm-rc=1");
    assert!(!transcript.contains("confirm-result="), "{transcript}");
}

/// The TypeScript helper, same shape.
#[test]
fn under_the_tui_typescript_confirm_without_channel_fails_closed() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tui-ts");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(
        &driver,
        "import { pathToFileURL } from \"node:url\";\n\
         const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
         console.log(\"confirm-result=\" + JSON.stringify(prompt.confirm(\"Go? tui-ts-x7x\")));\n",
    )
    .unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"; echo \"confirm-rc=$?\"",
            driver.display(),
            helper.display()
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("confirm-rc=", PTY_DEADLINE).unwrap();
    assert_fails_closed_under_the_tui(&transcript, "confirm-rc=1");
    assert!(!transcript.contains("confirm-result="), "{transcript}");
}

fn assert_fails_closed_under_the_tui(transcript: &str, exit_line: &str) {
    assert!(transcript.contains(TUI_NO_CHANNEL), "{transcript}");
    assert!(transcript.contains(exit_line), "{transcript}");
    assert!(
        !transcript.contains("(y/N)"),
        "the terminal was asked:\n{transcript}"
    );
    assert!(!transcript.contains("coyote_prompt"), "{transcript}");
}

/// The wrapper tail every fail-closed `password` case runs after the
/// helper: the exit code, the length of whatever came back, and the
/// terminal's line discipline, so a refusal that left echo off would show.
const PASSWORD_REPORT: &str = "echo \"password-rc=$rc len=${#v}\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";

/// The refusal is the whole of what the helper says: the one-line message
/// exactly once, the question never painted (so nothing was read from the
/// terminal either), and echo still on afterwards.
fn assert_password_refused_cleanly(transcript: &str, question_marker: &str) {
    assert_eq!(
        transcript.matches(TUI_NO_CHANNEL).count(),
        1,
        "the refusal is not exactly one line:\n{transcript}"
    );
    assert!(
        !transcript.contains(question_marker),
        "the password question was painted on the terminal:\n{transcript}"
    );
    assert!(
        !transcript.contains("password-length="),
        "the helper returned a value instead of failing:\n{transcript}"
    );
    let flags = transcript
        .lines()
        .find_map(|line| line.strip_prefix("tty-flags: "))
        .unwrap_or_else(|| panic!("no tty-flags line:\n{transcript}"));
    let tokens: Vec<&str> = flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
    assert!(
        tokens.contains(&"echo") && !tokens.contains(&"-echo"),
        "the refused password left echo off:\n{transcript}"
    );
}

/// `password` is the fallback a hidden terminal read would hurt most (a
/// secret typed into the UI's composer), so under the full-screen UI the
/// bash helper refuses it before touching the terminal: the one-line
/// message on stderr, exit 1, no `Secret?` painted, echo left on.
#[test]
fn under_the_tui_bash_password_without_channel_fails_closed() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; v=\"$(password \"Secret? tui-pw-sh-r8\")\"; rc=$?; {PASSWORD_REPORT}"
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_fails_closed_under_the_tui(&transcript, "password-rc=1 len=0");
    assert_password_refused_cleanly(&transcript, "Secret?");
}

/// `range` reads the cursor row and keys in subshells; its refusal has to
/// come from the function itself, or an `exit` inside a subshell ends the
/// subshell alone and the key loop spins on the refusal forever. Under
/// the UI it ends at once: one refusal line, no value, nothing painted.
#[test]
fn under_the_tui_bash_range_without_channel_fails_closed_at_once() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; v=\"$(range \"Count? tui-range-sh-r8\" 1 3 9)\"; rc=$?; \
             echo \"range-rc=$rc len=${{#v}}\"; \
             echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done"
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let started = Instant::now();
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "{transcript}");
    assert_fails_closed_under_the_tui(&transcript, "range-rc=1 len=0");
    assert_password_refused_cleanly(&transcript, "Count?");
}

/// `with_validate` evaluates its prompt in a subshell and re-asks until
/// the validator passes; a prompt that fails (refused under the UI) ends
/// the loop with its status instead of leaving an empty value for the
/// validator to reject forever.
#[test]
fn under_the_tui_bash_with_validate_without_channel_fails_closed_at_once() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; \
             v=\"$(with_validate 'input \"Name? tui-validate-sh-r9\"' validate_present)\"; rc=$?; \
             echo \"validate-rc=$rc len=${{#v}}\"; \
             echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done"
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let started = Instant::now();
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "{transcript}");
    assert_fails_closed_under_the_tui(&transcript, "validate-rc=1 len=0");
    assert_password_refused_cleanly(&transcript, "Name?");
}

/// `guard_operation` without a channel falls back to a terminal confirm
/// written to /dev/tty; under the UI that tty is the UI's own screen, so
/// the guard refuses before the redirection and aborts the operation.
#[test]
fn under_the_tui_bash_guard_without_channel_refuses_before_touching_the_tty() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; \
             (guard_operation \"Really? tui-guard-sh-r8\" && echo continued); rc=$?; echo \"guard-rc=$rc\"; \
             echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done"
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(transcript.contains("guard-rc=1"), "{transcript}");
    assert!(
        !transcript.contains("continued"),
        "the guard let the operation through:\n{transcript}"
    );
    assert!(
        !transcript.contains("(y/N)"),
        "the terminal was asked:\n{transcript}"
    );
    assert_password_refused_cleanly(&transcript, "Really?");
}

/// The Python helper's `password` has its own terminal path (getpass, not
/// `_tty_line`), so it is pinned on its own: exit 1, no value, no
/// `Secret?` painted, echo left on.
#[test]
fn under_the_tui_python_password_without_channel_fails_closed() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let driver = python_password_driver("tui-pw-py-r8");
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; v=$(python3 -c '{driver}' \"{PY_HELPER}\"); rc=$?; {PASSWORD_REPORT}"
        ))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_fails_closed_under_the_tui(&transcript, "password-rc=1 len=0");
    assert_password_refused_cleanly(&transcript, "Secret?");
}

/// The TypeScript helper's `password` (the hidden `ttyLine`), same shape.
#[test]
fn under_the_tui_typescript_password_without_channel_fails_closed() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tui-ts-password");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("tui-pw-ts-r8")).unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; v=$(NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"); rc=$?; {PASSWORD_REPORT}",
            driver.display(),
            helper.display()
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_fails_closed_under_the_tui(&transcript, "password-rc=1 len=0");
    assert_password_refused_cleanly(&transcript, "Secret?");
}

/// The gate is the TUI's, not every host's: the same `COYOTE_UI` value
/// spelled differently (`repl`, unset) keeps the terminal fallback, so a
/// tool run from the plain REPL still asks on the tty. Pinned so the
/// refusal cannot widen to every run without a channel.
#[test]
fn outside_the_tui_bash_confirm_without_channel_still_asks_the_terminal() {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; source \"{HELPER}\"; v=\"$(confirm \"Go? repl-x7x\")\"; echo \"confirm-result=$v\""
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "repl")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for("(y/N)", PTY_DEADLINE).unwrap();
    pty.send(b"\x1b[5;1R").unwrap();
    pty.send(b"n").unwrap();
    let transcript = pty.wait_for("confirm-result=0", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains(TUI_NO_CHANNEL),
        "the TUI refusal fired outside the TUI:\n{transcript}"
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
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
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
        .env_remove("COYOTE_UI")
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

/// The Python helper's `password` fallback reads the secret from the
/// terminal with echo off: the typed text never appears in the transcript,
/// only the caller's report of its length does.
#[test]
fn python_password_without_channel_hides_the_typed_text() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let driver = "import importlib.util, sys\n\
        sys.dont_write_bytecode = True\n\
        spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
        m = importlib.util.module_from_spec(spec)\n\
        spec.loader.exec_module(m)\n\
        v = m.password(\"Secret? pw-py-x7x\")\n\
        print(\"password-length=\" + str(None if v is None else len(v)))\n";
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; python3 -c '{driver}' \"{PY_HELPER}\""
        ))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for("pw-py-x7x", PTY_DEADLINE).unwrap();
    pty.send(b"hunter2zq\n").unwrap();
    let transcript = pty.wait_for("password-length=9", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("hunter2zq"),
        "password echoed the typed text on the terminal:\n{transcript}"
    );
    assert!(
        !transcript.contains("coyote_prompt"),
        "password wrote a protocol line without a channel:\n{transcript}"
    );
}

/// The TypeScript helper's `password` fallback, same shape: its hidden
/// read goes through a child that turns echo off on the terminal.
#[test]
fn typescript_password_without_channel_hides_the_typed_text() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(
        &driver,
        "import { pathToFileURL } from \"node:url\";\n\
         const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
         const v = prompt.password(\"Secret? pw-ts-x7x\");\n\
         console.log(\"password-length=\" + (v === null ? \"null\" : String(v.length)));\n",
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
        .env_remove("COYOTE_UI")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for("pw-ts-x7x", PTY_DEADLINE).unwrap();
    pty.send(b"hunter2zq\n").unwrap();
    let transcript = pty.wait_for("password-length=9", PTY_DEADLINE).unwrap();
    assert!(
        !transcript.contains("hunter2zq"),
        "password echoed the typed text on the terminal:\n{transcript}"
    );
    assert!(
        !transcript.contains("coyote_prompt"),
        "password wrote a protocol line without a channel:\n{transcript}"
    );
}

/// Both script helpers restore the terminal's echo flag once `password`
/// has read its line: the probe reads the secret the moment the prompt
/// shows, then the wrapper reports `stty -a` on the same terminal.
#[test]
fn python_password_restores_echo_after_the_read() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("tty-py-password-restore");
    let _guard = TempDirGuard(dir.clone());
    let driver = dir.join("driver.py");
    fs::write(&driver, python_password_driver("pw-py-r3s")).unwrap();
    let transcript = run_password_in_pty(
        &format!("python3 \"{}\" \"{PY_HELPER}\"", driver.display()),
        "pw-py-r3s",
        |pty| pty.send(b"hunter2zq\n"),
    );
    assert!(
        transcript.contains("password-length=9"),
        "password did not return the typed text:\n{transcript}"
    );
    assert!(
        !transcript.contains("hunter2zq"),
        "password echoed the typed text on the terminal:\n{transcript}"
    );
    assert_echo_restored(&transcript);
}

#[test]
fn typescript_password_restores_echo_after_the_read() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-restore");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-r3s")).unwrap();
    let transcript = run_password_in_pty(
        &format!(
            "NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ),
        "pw-ts-r3s",
        |pty| pty.send(b"hunter2zq\n"),
    );
    assert!(
        transcript.contains("password-length=9"),
        "password did not return the typed text:\n{transcript}"
    );
    assert!(
        !transcript.contains("hunter2zq"),
        "password echoed the typed text on the terminal:\n{transcript}"
    );
    assert_echo_restored(&transcript);
}

/// Ctrl-C while `password` waits must not leave the terminal with echo
/// off: whatever turned echo off for the read has to turn it back on when
/// the read is interrupted, as bash's `read -s` and Python's getpass do.
#[test]
fn python_password_restores_echo_after_ctrl_c() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("tty-py-password-intr");
    let _guard = TempDirGuard(dir.clone());
    let driver = dir.join("driver.py");
    fs::write(&driver, python_password_driver("pw-py-1nt")).unwrap();
    let transcript = run_password_in_pty(
        &format!("python3 \"{}\" \"{PY_HELPER}\"", driver.display()),
        "pw-py-1nt",
        interrupt_mid_read,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_hidden_read_was_interrupted(&transcript);
    assert_echo_restored(&transcript);
}

#[test]
fn typescript_password_restores_echo_after_ctrl_c() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-intr");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-1nt")).unwrap();
    let transcript = run_password_in_pty(
        &format!(
            "NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ),
        "pw-ts-1nt",
        interrupt_mid_read,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_hidden_read_was_interrupted(&transcript);
    assert_echo_restored(&transcript);
}

/// `password` puts the terminal back the way it found it, not into some
/// fixed mode: a terminal whose echo was already off before the prompt must
/// still have echo off once the read completes.
#[test]
fn typescript_password_keeps_a_pre_prompt_echo_off_after_the_read() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-keep-noecho");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-k33p")).unwrap();
    let transcript = run_password_in_pty_with(
        ECHO_OFF_PRELUDE,
        &format!(
            "NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ),
        "pw-ts-k33p",
        |pty| pty.send(b"hunter2zq\n"),
    );
    assert!(
        transcript.contains("password-length=9"),
        "password did not return the typed text:\n{transcript}"
    );
    assert_echo_still_off(&transcript);
}

#[test]
fn python_password_keeps_a_pre_prompt_echo_off_after_the_read() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("tty-py-password-keep-noecho");
    let _guard = TempDirGuard(dir.clone());
    let driver = dir.join("driver.py");
    fs::write(&driver, python_password_driver("pw-py-k33p")).unwrap();
    let transcript = run_password_in_pty_with(
        ECHO_OFF_PRELUDE,
        &format!("python3 \"{}\" \"{PY_HELPER}\"", driver.display()),
        "pw-py-k33p",
        |pty| pty.send(b"hunter2zq\n"),
    );
    assert!(
        transcript.contains("password-length=9"),
        "password did not return the typed text:\n{transcript}"
    );
    assert_python_took_the_hidden_path(&transcript);
    assert_echo_still_off(&transcript);
}

/// The interrupt path restores the snapshot too, so Ctrl-C on a terminal
/// that began with echo off must not switch echo on.
#[test]
fn typescript_password_keeps_a_pre_prompt_echo_off_after_ctrl_c() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-keep-noecho-intr");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-k1nt")).unwrap();
    let transcript = run_password_in_pty_with(
        ECHO_OFF_PRELUDE,
        &format!(
            "NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ),
        "pw-ts-k1nt",
        interrupt_mid_read,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_echo_still_off(&transcript);
}

#[test]
fn python_password_keeps_a_pre_prompt_echo_off_after_ctrl_c() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("tty-py-password-keep-noecho-intr");
    let _guard = TempDirGuard(dir.clone());
    let driver = dir.join("driver.py");
    fs::write(&driver, python_password_driver("pw-py-k1nt")).unwrap();
    let transcript = run_password_in_pty_with(
        ECHO_OFF_PRELUDE,
        &format!("python3 \"{}\" \"{PY_HELPER}\"", driver.display()),
        "pw-py-k1nt",
        interrupt_mid_read,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_python_took_the_hidden_path(&transcript);
    assert_echo_still_off(&transcript);
}

/// The interrupt character arriving the very instant the prompt shows, with
/// nothing typed first, must leave the terminal as it was: no window exists
/// in which the hidden read has changed the mode but cannot yet undo it.
#[test]
fn typescript_password_restores_echo_after_an_immediate_ctrl_c() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-intr-0ms");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-0ms1")).unwrap();
    let transcript = run_password_in_pty(
        &format!(
            "NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ),
        "pw-ts-0ms1",
        interrupt_immediately,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert!(
        !transcript.contains("Warning: Password input may be echoed."),
        "the helper fell back to an echoed read:\n{transcript}"
    );
    assert_echo_restored(&transcript);
}

#[test]
fn python_password_restores_echo_after_an_immediate_ctrl_c() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("tty-py-password-intr-0ms");
    let _guard = TempDirGuard(dir.clone());
    let driver = dir.join("driver.py");
    fs::write(&driver, python_password_driver("pw-py-0ms1")).unwrap();
    let transcript = run_password_in_pty(
        &format!("python3 \"{}\" \"{PY_HELPER}\"", driver.display()),
        "pw-py-0ms1",
        interrupt_immediately,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_python_took_the_hidden_path(&transcript);
    assert_echo_restored(&transcript);
}

/// Same instant interrupt on a terminal that began with echo off: the
/// restore must put back the snapshot, not switch echo on.
#[test]
fn typescript_password_keeps_a_pre_prompt_echo_off_after_an_immediate_ctrl_c() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-keep-noecho-intr-0ms");
    let _guard = TempDirGuard(dir.clone());
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-0msk")).unwrap();
    let transcript = run_password_in_pty_with(
        ECHO_OFF_PRELUDE,
        &format!(
            "NODE_NO_WARNINGS=1 {runtime} \"{}\" \"{}\"",
            driver.display(),
            helper.display()
        ),
        "pw-ts-0msk",
        interrupt_immediately,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_echo_still_off(&transcript);
}

#[test]
fn python_password_keeps_a_pre_prompt_echo_off_after_an_immediate_ctrl_c() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("tty-py-password-keep-noecho-intr-0ms");
    let _guard = TempDirGuard(dir.clone());
    let driver = dir.join("driver.py");
    fs::write(&driver, python_password_driver("pw-py-0msk")).unwrap();
    let transcript = run_password_in_pty_with(
        ECHO_OFF_PRELUDE,
        &format!("python3 \"{}\" \"{PY_HELPER}\"", driver.display()),
        "pw-py-0msk",
        interrupt_immediately,
    );
    assert!(
        !transcript.contains("password-length="),
        "password returned despite the interrupt:\n{transcript}"
    );
    assert_python_took_the_hidden_path(&transcript);
    assert_echo_still_off(&transcript);
}

/// When `stty` cannot be found the TypeScript helper must say so and still
/// read the secret in the clear rather than fail: the tool runs with a PATH
/// that holds only the interpreter and `sh`, while the wrapper keeps its own
/// PATH for the `stty -a` report.
#[test]
fn typescript_password_without_stty_warns_and_still_reads() {
    let Some(runtime) = ts_runtime() else {
        return;
    };
    let Ok(runtime_path) = which::which(runtime) else {
        eprintln!("skipping: {runtime} not on PATH");
        return;
    };
    let dir = fresh_config_dir("tty-ts-password-nostty");
    let _guard = TempDirGuard(dir.clone());
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&runtime_path, bin.join(runtime)).unwrap();
    std::os::unix::fs::symlink(which::which("sh").unwrap(), bin.join("sh")).unwrap();
    // tsx is a node program; it needs node reachable on the restricted PATH.
    if runtime != "node" {
        let Ok(node_path) = which::which("node") else {
            eprintln!("skipping: {runtime} needs node on PATH and node is not available");
            return;
        };
        std::os::unix::fs::symlink(node_path, bin.join("node")).unwrap();
    }
    let helper = dir.join("prompt-utils.ts");
    fs::copy(TS_HELPER, &helper).unwrap();
    let driver = dir.join("driver.ts");
    fs::write(&driver, typescript_password_driver("pw-ts-n0st")).unwrap();
    let transcript = run_password_in_pty(
        &format!(
            "PATH=\"{}\" NODE_NO_WARNINGS=1 \"{}\" \"{}\" \"{}\"",
            bin.display(),
            runtime_path.display(),
            driver.display(),
            helper.display()
        ),
        "pw-ts-n0st",
        |pty| pty.send(b"hunter2zq\n"),
    );
    assert!(
        transcript.contains("Password input may be echoed"),
        "no warning about the missing stty:\n{transcript}"
    );
    assert!(
        transcript.contains("password-length=9"),
        "password did not return the typed text without stty:\n{transcript}"
    );
    assert_echo_restored(&transcript);
}

/// Shell run by the pty wrapper before the tool: turns echo off on the
/// terminal and proves it, so a later `echo` in the report is the helper's
/// doing.
const ECHO_OFF_PRELUDE: &str =
    "stty -echo </dev/tty; echo \"pre-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\";";

fn python_password_driver(marker: &str) -> String {
    format!(
        "import importlib.util, sys\n\
         sys.dont_write_bytecode = True\n\
         spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
         m = importlib.util.module_from_spec(spec)\n\
         spec.loader.exec_module(m)\n\
         v = m.password(\"Secret? {marker}\")\n\
         print(\"password-length=\" + str(None if v is None else len(v)))\n"
    )
}

fn typescript_password_driver(marker: &str) -> String {
    format!(
        "import {{ pathToFileURL }} from \"node:url\";\n\
         const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
         const v = prompt.password(\"Secret? {marker}\");\n\
         console.log(\"password-length=\" + (v === null ? \"null\" : String(v.length)));\n"
    )
}

/// Runs `tool` on a pty behind a bash wrapper that survives the tool's
/// SIGINT (a handler, not an ignore, so the tool itself keeps the default
/// disposition), waits for `marker` in the prompt, lets `interact` drive
/// the terminal and finally reports the terminal's line discipline flags.
/// The tool's own stdio is kept off the terminal (and replayed afterwards)
/// so an interpreter's exit-time reset of its standard fds cannot stand in
/// for the helper restoring what it changed.
fn run_password_in_pty(
    tool: &str,
    marker: &str,
    interact: impl FnOnce(&mut Pty) -> std::io::Result<()>,
) -> String {
    run_password_in_pty_with("", tool, marker, interact)
}

/// `run_password_in_pty` with a shell `prelude` the wrapper runs on the
/// terminal before starting the tool (for example to change its mode).
fn run_password_in_pty_with(
    prelude: &str,
    tool: &str,
    marker: &str,
    interact: impl FnOnce(&mut Pty) -> std::io::Result<()>,
) -> String {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "exec </dev/null; trap ':' INT; {prelude} out=$(mktemp); {tool} >\"$out\" 2>&1; rc=$?; \
             cat \"$out\"; rm -f \"$out\"; echo \"tool-exit=$rc\"; \
             echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done"
        ))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env("TERM", "xterm");
    let mut pty = Pty::spawn(cmd).unwrap();
    pty.wait_for(marker, PTY_DEADLINE).unwrap();
    interact(&mut pty).unwrap();
    pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap()
}

/// Types part of a secret, then sends the terminal's interrupt character.
fn interrupt_mid_read(pty: &mut Pty) -> std::io::Result<()> {
    pty.send(TYPED_BEFORE_INTERRUPT)?;
    thread::sleep(Duration::from_millis(100));
    pty.send(b"\x03")
}

/// Sends the interrupt character the moment the prompt text is seen, with
/// no keystrokes and no delay before it.
fn interrupt_immediately(pty: &mut Pty) -> std::io::Result<()> {
    pty.send(b"\x03")
}

/// Keystrokes `interrupt_mid_read` types: a sequence no temp-dir name or
/// traceback would contain by chance.
const TYPED_BEFORE_INTERRUPT: &[u8] = b"zq9xq";

/// getpass falls back to an echoed read with a warning when it cannot
/// control the terminal; the echo checks only mean something on the hidden
/// path, so that warning must be absent.
fn assert_python_took_the_hidden_path(transcript: &str) {
    assert!(
        !transcript.contains("Warning: Password input may be echoed."),
        "getpass fell back to an echoed read:\n{transcript}"
    );
}

/// The keystrokes typed before the interrupt must not show on the terminal
/// (echo was really off when it arrived) and the helper must have taken the
/// hidden path, not the warned, echoed one. Only the region the terminal
/// echoed is inspected: after the prompt marker, before the replayed tool
/// output, with interpreter traceback lines left out.
fn assert_hidden_read_was_interrupted(transcript: &str) {
    let start = transcript.find("Secret? ").unwrap_or(0);
    let end = transcript.find("tool-exit=").unwrap_or(transcript.len());
    let echoed: String = transcript[start..end]
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            !(line.starts_with("Traceback") || line.starts_with("File \""))
        })
        .collect::<Vec<_>>()
        .join("\n");
    let typed = std::str::from_utf8(TYPED_BEFORE_INTERRUPT).unwrap();
    assert!(
        !echoed.contains(typed),
        "the terminal echoed keystrokes typed before the interrupt:\n{transcript}"
    );
    assert!(
        !echoed.contains("Warning: Password input may be echoed."),
        "the helper fell back to an echoed read:\n{transcript}"
    );
}

/// The `stty -a` report the wrapper printed must list `echo` as on.
fn assert_echo_restored(transcript: &str) {
    let tokens = tty_flag_tokens(transcript, "tty-flags: ");
    assert!(
        !tokens.contains(&"-echo"),
        "the terminal was left with echo off after password:\n{transcript}"
    );
    assert!(
        tokens.contains(&"echo"),
        "stty did not report the echo flag at all:\n{transcript}"
    );
}

/// The prelude's report must show echo off (the precondition held) and the
/// final report must still show it off.
fn assert_echo_still_off(transcript: &str) {
    let before = tty_flag_tokens(transcript, "pre-flags: ");
    assert!(
        before.contains(&"-echo"),
        "the prelude did not turn echo off before the prompt:\n{transcript}"
    );
    let after = tty_flag_tokens(transcript, "tty-flags: ");
    assert!(
        after.contains(&"-echo"),
        "password switched echo on although it was off before the prompt:\n{transcript}"
    );
}

fn tty_flag_tokens<'a>(transcript: &'a str, prefix: &str) -> Vec<&'a str> {
    let flags = transcript
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no {prefix}line in the transcript:\n{transcript}"));
    flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect()
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

/// A Python tool that really asks through the channel while nobody can
/// answer (the binary runs detached from any terminal with stdin on
/// /dev/null) must get the cancel answer and fail closed: `confirm` returns
/// None, the run finishes, and it never reports a confirmed answer.
#[test]
fn macro_run_cancels_a_python_tools_prompt_when_nobody_can_answer() {
    if which::which("python3").is_err() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = fresh_config_dir("helper-py-prompt");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_prompt_py".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_prompt_py.py",
        "#!/usr/bin/env python3\n\
         import importlib.util\n\
         import os\n\
         \n\
         \n\
         def run():\n\
         \x20   \"\"\"Ask the user through the prompt helper and report the answer.\"\"\"\n\
         \x20   spec = importlib.util.spec_from_file_location(\"prompt_utils\", os.environ[\"LLM_PROMPT_UTILS_PY\"])\n\
         \x20   prompt_utils = importlib.util.module_from_spec(spec)\n\
         \x20   spec.loader.exec_module(prompt_utils)\n\
         \x20   line = f\"fixture confirm: {prompt_utils.confirm('Go ahead? pw-py-m4cro')!r}\"\n\
         \x20   print(line, flush=True)\n\
         \x20   return line\n",
    );
    write_macro(&dir, "prompt", "Call the fixture prompt py tool");

    let output = run_coyote(&dir, &["--macro", "prompt"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.code().is_some(),
        "the run did not finish on its own\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("fixture confirm: True") && !stderr.contains("fixture confirm: True"),
        "the prompt was answered yes with nobody there\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("coyote_prompt") && !stderr.contains("coyote_prompt"),
        "a raw request line leaked into the user-facing output\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("fixture confirm: None"),
        "the helper did not report the cancelled prompt as None\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
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
    let run_probe = || {
        Command::new("node")
            .arg(&probe)
            .env("NODE_NO_WARNINGS", "1")
            .stdin(Stdio::null())
            .output()
    };
    let clean = |out: &std::io::Result<Output>| {
        out.as_ref()
            .is_ok_and(|out| out.status.success() && out.stderr.is_empty())
    };
    // A cold node start can print a one-off warning; one retry absorbs it.
    let mut probe_result = run_probe();
    if !clean(&probe_result) {
        probe_result = run_probe();
    }
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

fn fresh_config_dir(scenario: &str) -> PathBuf {
    // The clock alone is not unique: tests run in parallel and macOS reports
    // whole microseconds, so two callers can land in the same tick.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-prompt-protocol-{scenario}-{}-{unique}-{sequence}",
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
        .env_remove("_COYOTE_PROMPT_OUT")
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

// ---------------------------------------------------------------------------
// The bash helpers refused as a tool's FIRST act under the UI, in the shape
// a host captures a tool in: stdout and stderr are files (pipes), stdin is
// closed, and the controlling terminal is the UI's own screen.
// ---------------------------------------------------------------------------

/// Runs a `set -e` bash tool whose body is `body` under `COYOTE_UI=tui`
/// with no reply channel, stdout and stderr captured to files, from `dir`
/// as the working directory. Returns the wrapper's transcript (which ends
/// with the captured stderr) and the raw terminal bytes, after asserting
/// the tool ended non-zero within a few seconds with nothing on stdout
/// and exactly one stderr line, the refusal.
fn run_tool_refused_as_its_first_act(scenario: &str, body: &str) -> (String, Vec<u8>) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    let tool = dir.join("tool.sh");
    fs::write(
        &tool,
        format!(
            "#!/usr/bin/env bash\nset -e\nsource \"{HELPER}\"\n{body}\necho \"tool-finished\"\n"
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(format!(
            "cd '{dir}'; exec </dev/null; '{tool}' >out 2>err; rc=$?; \
             echo \"tool-rc=$rc stdout-bytes=$(wc -c <out | tr -d ' ') stderr-lines=$(wc -l <err | tr -d ' ')\"; \
             echo \"stderr: $(cat err)\"; \
             echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done",
            dir = dir.display(),
            tool = tool.display()
        ))
        .env_remove("COYOTE_PROMPT_REPLY")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm");
    let started = Instant::now();
    let mut pty = Pty::spawn(cmd).unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE)
        .unwrap_or_else(|e| {
            panic!("the tool never came back (spinning or reading the tty?): {e:?}")
        });
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "the refusal took {took:?}:\n{transcript}"
    );
    assert!(
        transcript.contains("tool-rc=1 stdout-bytes=0 stderr-lines=1"),
        "not a one-line refusal with a non-zero exit and empty stdout:\n{transcript}"
    );
    assert!(
        transcript.contains(&format!("stderr: {TUI_NO_CHANNEL}")),
        "the one stderr line is not the refusal:\n{transcript}"
    );
    assert!(
        !transcript.contains("tool-finished"),
        "the tool ran on past the refused helper:\n{transcript}"
    );
    assert!(
        !transcript.contains("(y/N)"),
        "the terminal was asked:\n{transcript}"
    );
    assert!(!transcript.contains("coyote_prompt"), "{transcript}");
    let raw = pty.raw().to_vec();
    assert!(
        !raw.windows(4).any(|w| w == b"\x1b[6n"),
        "the helper queried the cursor on the UI's terminal:\n{transcript}"
    );
    (transcript, raw)
}

/// A tool whose first act is `range` (the helper with a cursor query and a
/// key loop in subshells) ends at once under the UI: exit 1, one stderr
/// line, empty stdout, no question painted, no cursor query, no spin.
#[test]
fn under_the_tui_a_tool_whose_first_act_is_range_ends_at_once_with_one_stderr_line() {
    let (transcript, _) = run_tool_refused_as_its_first_act(
        "tool-range",
        "v=\"$(range \"Count? tui-range-tool-r9\" 1 3 9)\"\necho \"value=$v\"",
    );
    assert!(
        !transcript.contains("Count?"),
        "the range question was painted on the terminal:\n{transcript}"
    );
    assert!(
        !transcript.contains("value="),
        "range handed the tool a value:\n{transcript}"
    );
}

/// `guard_operation` called directly (not in a command substitution) as
/// the tool's first act: the tool ends with exit 1 before any `(y/N)`
/// could reach the UI's screen, and the guarded operation never runs.
#[test]
fn under_the_tui_a_tool_whose_first_act_is_guard_operation_refuses_with_one_stderr_line() {
    let (transcript, _) = run_tool_refused_as_its_first_act(
        "tool-guard-op",
        "guard_operation \"Really wipe it? tui-guard-op-r9\"\necho \"continued\"",
    );
    assert!(
        !transcript.contains("Really wipe it?"),
        "the guard's question was painted on the terminal:\n{transcript}"
    );
    assert!(
        !transcript.contains("continued"),
        "the guard let the operation through:\n{transcript}"
    );
}

/// `guard_path` on a path outside the working directory, the case that
/// asks: under the UI it refuses as its first act, exit 1, one stderr
/// line, no question on the screen, and the operation never runs.
#[test]
fn under_the_tui_a_tool_whose_first_act_is_guard_path_refuses_with_one_stderr_line() {
    let (transcript, _) = run_tool_refused_as_its_first_act(
        "tool-guard-path",
        "guard_path /etc/hosts \"Touch hosts? tui-guard-path-r9\"\necho \"continued\"",
    );
    assert!(
        !transcript.contains("Touch hosts?"),
        "the guard's question was painted on the terminal:\n{transcript}"
    );
    assert!(
        !transcript.contains("continued"),
        "the guard let the operation through:\n{transcript}"
    );
}
