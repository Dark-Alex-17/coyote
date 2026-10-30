//! Captures what the REPL turn loop writes to stdout and stderr when the
//! real binary runs a scripted turn, normalises the bytes, and under
//! `UPDATE_GOLDENS=1` records them as golden files for later comparison.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process, thread};

use common::fake_llm::{FakeLlm, Reply};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Capture {
    stdout: String,
    stderr: String,
}

struct Scenario {
    name: &'static str,
    capture: fn(&Path) -> Capture,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "hello",
        capture: hello,
    },
    Scenario {
        name: "tool_call",
        capture: tool_call,
    },
    Scenario {
        name: "undo_redraw",
        capture: undo_redraw,
    },
    Scenario {
        name: "hello_nostream",
        capture: hello_nostream,
    },
];

const RUN_DEADLINE: Duration = Duration::from_secs(120);

fn fresh_config_dir(scenario: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-turn-driver-{scenario}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// Without IS_SANDBOX (scrubbed in `run_coyote`) a run insists on a vault
/// password file, so every fixture provides one. Paths land in single-quoted
/// YAML scalars: double-quoted ones would read the backslashes in a Windows
/// temp path as escapes.
fn write_vault_pass(dir: &Path) -> PathBuf {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    vault_pass
}

/// Lays out a dry-run config dir; `config_extra` is spliced verbatim into
/// config.yaml.
fn write_dry_run_fixture(dir: &Path, config_extra: &str) {
    let vault_pass = write_vault_pass(dir);
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             {config_extra}\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

/// Lays out a config dir whose only model is served by `fake`, with one
/// bash tool for the model to call.
fn write_tool_fixture(dir: &Path, fake: &FakeLlm) {
    let vault_pass = write_vault_pass(dir);
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
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
             memory: false\n",
            vault_pass = vault_pass.display(),
            api_base = fake.api_base(),
        ),
    )
    .unwrap();
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(
        tools.join("fixture_echo.sh"),
        "#!/usr/bin/env bash\n\
         set -e\n\
         \n\
         # @describe Echo a fixed line for the harness.\n\
         # @option --text! The text to echo\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {\n\
         \x20   echo \"fixture stdout: $argc_text\"\n\
         \x20   echo \"fixture result: $argc_text\" >> \"$LLM_OUTPUT\"\n\
         }\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/tool.yaml"),
        "steps:\n  - \"Call the fixture tool with text hello\"\n",
    )
    .unwrap();
}

fn run_coyote(dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let home = dir.join("home");
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    cmd.args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("COLUMNS", "100")
        .env("LINES", "40")
        .env("TERM", "dumb")
        .env("PATH", path)
        .env_remove("NO_COLOR")
        .env_remove("COLORTERM")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("COYOTE_SAVE_SESSION")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_SHOW_TOOL_CALLS")
        .envs(extra_env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = common::pty::detached(&mut cmd).spawn().unwrap();
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + RUN_DEADLINE;
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let stdout = stdout.join().unwrap();
    let stderr = stderr.join().unwrap();
    let Some(status) = status else {
        panic!(
            "coyote {args:?} did not exit within {RUN_DEADLINE:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    };
    Output {
        status,
        stdout,
        stderr,
    }
}

fn drain(mut stream: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        bytes
    })
}

/// Checks the run exited cleanly and returns both streams normalised.
fn finish(label: &str, output: &Output) -> Capture {
    let capture = Capture {
        stdout: normalise(&output.stdout),
        stderr: normalise(&output.stderr),
    };
    let text = format!("stdout:\n{}\nstderr:\n{}", capture.stdout, capture.stderr);
    assert!(
        output.status.success(),
        "{label}: exit {:?}\n{text}",
        output.status
    );
    assert!(
        !text.contains("panicked"),
        "{label}: the binary panicked:\n{text}"
    );
    capture
}

/// Lossy UTF-8 with `\r\n` folded to `\n`, every run of cursor/erase CSI
/// sequences folded to `<clear>` and every run of two or more box-drawing
/// dashes folded to `<rule>`.
fn normalise(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(mut end) = clear_sequence_end(&chars, i) {
            while let Some(next) = clear_sequence_end(&chars, end) {
                end = next;
            }
            out.push_str("<clear>");
            i = end;
        } else if chars[i] == '─' && chars.get(i + 1) == Some(&'─') {
            while chars.get(i) == Some(&'─') {
                i += 1;
            }
            out.push_str("<rule>");
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The index just past an `ESC [ <params> K|J|H` sequence beginning at
/// `start`, where the parameters are digits and semicolons.
fn clear_sequence_end(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start) != Some(&'\x1b') || chars.get(start + 1) != Some(&'[') {
        return None;
    }
    let mut i = start + 2;
    while matches!(chars.get(i), Some(c) if c.is_ascii_digit() || *c == ';') {
        i += 1;
    }
    matches!(chars.get(i), Some('K' | 'J' | 'H')).then_some(i + 1)
}

fn hello(dir: &Path) -> Capture {
    write_dry_run_fixture(dir, "");
    finish("hello", &run_coyote(dir, &["hello"], &[]))
}

fn hello_nostream(dir: &Path) -> Capture {
    write_dry_run_fixture(dir, "stream: false\n");
    finish("hello_nostream", &run_coyote(dir, &["hello"], &[]))
}

fn tool_call(dir: &Path) -> Capture {
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_echo".to_string(),
            arguments: r#"{"text":"hello"}"#.to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_tool_fixture(dir, &fake);
    let output = run_coyote(
        dir,
        &["--macro", "tool"],
        &[("COYOTE_SHOW_TOOL_CALLS", "1")],
    );
    let capture = finish("tool_call", &output);
    assert_eq!(
        fake.served(),
        2,
        "tool_call: the fake answered the tool call and the follow-up:\n{}",
        capture.stdout
    );
    assert!(
        capture.stdout.contains("Call fixture_echo"),
        "tool_call: the call was narrated:\n{}",
        capture.stdout
    );
    assert!(
        capture.stdout.contains("fixture stdout: hello"),
        "tool_call: the tool ran end to end:\n{}",
        capture.stdout
    );
    capture
}

fn undo_redraw(dir: &Path) -> Capture {
    write_dry_run_fixture(dir, "");
    fs::write(
        dir.join("sessions/notes.yaml"),
        "model: dryrun:dry-model\n\
         messages:\n\
         \x20 - role: user\n\
         \x20   content: hello\n\
         \x20 - role: assistant\n\
         \x20   content: hi there\n\
         \x20 - role: user\n\
         \x20   content: second\n\
         \x20 - role: assistant\n\
         \x20   content: second reply\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/undo-redraw.yaml"),
        "isolated: false\nsteps:\n  - \".undo\"\n  - \".redraw\"\n",
    )
    .unwrap();
    let output = run_coyote(dir, &["--session", "notes", "--macro", "undo-redraw"], &[]);
    let capture = finish("undo_redraw", &output);
    assert!(
        capture.stdout.contains("<rule> ↑ redrawn ↑ <rule>"),
        "undo_redraw: the redraw footer was printed:\n{}",
        capture.stdout
    );
    capture
}

fn capture_scenario(scenario: &Scenario) -> Capture {
    let dir = fresh_config_dir(scenario.name);
    let _guard = TempDirGuard(dir.clone());
    (scenario.capture)(&dir)
}

#[test]
fn turn_loop_goldens() {
    if env::var_os("UPDATE_GOLDENS").is_none() {
        eprintln!(
            "turn_loop_goldens: the compare arm is not wired yet; set UPDATE_GOLDENS=1 to capture the goldens"
        );
        return;
    }
    let golden_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/turn_driver");
    fs::create_dir_all(&golden_dir).unwrap();
    for scenario in SCENARIOS {
        let first = capture_scenario(scenario);
        let second = capture_scenario(scenario);
        assert_eq!(
            first.stdout, second.stdout,
            "{}: stdout differs between two runs",
            scenario.name
        );
        assert_eq!(
            first.stderr, second.stderr,
            "{}: stderr differs between two runs",
            scenario.name
        );
        for (ext, text) in [("stdout", &first.stdout), ("stderr", &first.stderr)] {
            let path = golden_dir.join(format!("{}.{ext}", scenario.name));
            fs::write(&path, text).unwrap();
            println!("wrote {}", path.display());
        }
    }
}
