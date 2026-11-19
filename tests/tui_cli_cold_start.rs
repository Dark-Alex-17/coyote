//! The binary's cold-start decisions that need no terminal: which shell an
//! interactive run is headed for (the `ui:` key, overridden by `--tui` /
//! `--repl`; `COYOTE_UI` in the environment is what coyote exports to its
//! tools and chooses nothing), what a prompt run refuses, and what
//! `--info`, `--help` and `--completions` report. Each case runs the
//! built binary with stdin on the null device or a pipe and both output
//! streams piped, so no pty is involved and the file runs on every CI
//! leg. The cases that need a real tty to watch the chosen shell come up
//! live in `tui_binary_menus.rs`.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::pty::strip_ansi;
use serial_test::serial;

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_config_dir(scenario: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-cli-cold-start-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// A dry-run config that never asks to save a session, so every exit path
/// is prompt-free.
fn write_dry_run_config(dir: &Path) {
    write_config(dir, "save_session: false\n");
}

/// The same config with `extra` top-level YAML appended by the caller.
/// Paths land in single-quoted YAML scalars.
fn write_config(dir: &Path, extra: &str) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             stream: true\n\
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
             {extra}\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

/// The build's own binary; every run goes through [`run_with_stdin`].
fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

/// The environment every run of the binary shares: working dir, config
/// and cache dirs and the home dir (`HOME` and `USERPROFILE`) all under
/// `dir`, the fixture bin first on `PATH`, and no inherited `COYOTE_*` or
/// color variables.
fn base_env(cmd: &mut Command, dir: &Path) {
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", dir.join("home"))
        .env("USERPROFILE", dir.join("home"))
        .env("TERM", "xterm-256color")
        .env("PATH", path)
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR");
}

/// The binary with no terminal at all (stdin from the null device, stdout
/// and stderr piped), the way a script runs it.
fn run_piped(dir: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    run_with_stdin(dir, extra_env, args, None)
}

/// [`run_piped`] with `stdin` fed through a pipe when given (a prompt
/// arriving on stdin, the way `echo prompt | coyote` runs). The file value
/// or the flags decide the shell.
fn run_with_stdin(
    dir: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
    stdin: Option<&str>,
) -> std::process::Output {
    let mut cmd = Command::new(built_binary());
    base_env(&mut cmd, dir);
    cmd.args(args)
        .envs(extra_env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Some(input) = stdin else {
        cmd.stdin(Stdio::null());
        return cmd.output().unwrap();
    };
    cmd.stdin(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(input.as_bytes()).unwrap();
        pipe.flush().unwrap();
    }
    child.wait_with_output().unwrap()
}

const ENTER_ALT: &[u8] = b"\x1b[?1049h";

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

/// `main` starts the full-screen UI only for an interactive run, whatever
/// the `ui:` key says: a positional prompt is the plain command path,
/// which has no business with the alternate screen and must work without
/// a tty.
#[test]
#[serial]
fn a_positional_prompt_under_ui_tui_is_the_plain_command_path() {
    let dir = fresh_config_dir("cmd-mode");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: tui\n");

    let out = run_piped(&dir, &[], &["zebra quartz probe-marker"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "a command-mode run entered the alternate screen:\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        out.status.success(),
        "command mode under ui: tui did not exit 0 ({:?}):\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    assert!(
        stdout.contains("probe-marker"),
        "the dry-run reply did not reach stdout:\nstdout={stdout}\nstderr={stderr}"
    );
}

/// An inspection flag is the plain command path too.
#[test]
#[serial]
fn the_info_flag_under_ui_tui_prints_the_table_without_a_tty() {
    let dir = fresh_config_dir("info-flag");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: tui\n");

    let out = run_piped(&dir, &[], &["--info"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "--info entered the alternate screen:\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        out.status.success(),
        "--info under ui: tui did not exit 0 ({:?}):\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    assert!(
        stdout.contains("dryrun:dry-model"),
        "--info did not print the model row:\nstdout={stdout}\nstderr={stderr}"
    );
}

/// A positional prompt makes the run a one-shot command, which neither
/// shell flag can host: `--tui` and `--repl` with a prompt are refused
/// before the config is read (a `ui: bogus` key is never seen), and
/// neither shell starts.
#[test]
#[serial]
fn a_shell_flag_with_a_prompt_is_refused_before_the_config_is_read() {
    let dir = fresh_config_dir("shell-flag-prompt");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: bogus\n");

    for flag in ["--tui", "--repl"] {
        let out = run_piped(&dir, &[], &[flag, "hello"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");

        assert!(
            !out.status.success(),
            "{flag} with a prompt exited 0 instead of refusing:\n{all}"
        );
        assert!(
            all.contains(&format!(
                "{flag} requires the interactive mode; drop the prompt"
            )),
            "the refusal did not name the flag and the prompt:\n{all}"
        );
        assert!(
            !all.contains("Invalid value"),
            "{flag} with a prompt read the config before refusing:\n{all}"
        );
        assert!(
            find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
            "{flag} with a prompt entered the alternate screen:\n{all}"
        );
        assert!(
            !all.contains("Welcome to"),
            "{flag} with a prompt started the REPL:\n{all}"
        );
    }
}

/// The shell flags exclude the non-interactive modes and each other at
/// the parser, so the clash is a usage error rather than a late bail.
#[test]
#[serial]
fn the_shell_flags_conflict_with_execute_and_each_other_in_clap() {
    let dir = fresh_config_dir("tui-conflict");
    let _guard = TempDirGuard(dir.clone());

    for args in [&["--tui", "-e"][..], &["--tui", "--repl"][..]] {
        let out = run_piped(&dir, &[], args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");

        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} did not exit with clap's usage error code:\n{all}"
        );
        assert!(
            all.contains("cannot be used with"),
            "{args:?} was not reported as a clap conflict:\n{all}"
        );
    }
}

/// The `ui` row of `--info`, or a panic naming the whole output.
fn info_ui_row(out: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "--info failed:\n{stdout}\n{stderr}");
    stdout
        .lines()
        .find(|line| line.split_whitespace().next() == Some("ui"))
        .unwrap_or_else(|| panic!("no ui row in --info:\n{stdout}"))
        .split_whitespace()
        .last()
        .unwrap()
        .to_string()
}

/// `--info` shows the `ui` the run resolved to, whichever source set it.
#[test]
#[serial]
fn info_reports_the_resolved_ui() {
    let dir = fresh_config_dir("info-ui");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let out = run_piped(&dir, &[], &["--info"]);
    assert_eq!(info_ui_row(&out), "repl", "the default ui was not reported");

    let out = run_piped(&dir, &[], &["--tui", "--info"]);
    assert_eq!(info_ui_row(&out), "tui", "--tui was not reported");

    write_config(&dir, "save_session: false\nui: tui\n");
    let out = run_piped(&dir, &[], &["--info"]);
    assert_eq!(info_ui_row(&out), "tui", "ui: tui was not reported");

    let out = run_piped(&dir, &[], &["--repl", "--info"]);
    assert_eq!(
        info_ui_row(&out),
        "repl",
        "--repl over ui: tui was not reported"
    );
}

/// `COYOTE_UI` in the environment (what a nested run inherits from its
/// parent) chooses nothing: `--info` keeps reporting the file's value or
/// the default, and the flags decide as they would without it.
#[test]
#[serial]
fn info_ignores_coyote_ui_in_the_environment() {
    let dir = fresh_config_dir("info-ignores-env");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: tui\n");

    for value in ["repl", "headless", ""] {
        let out = run_piped(&dir, &[("COYOTE_UI", value)], &["--info"]);
        assert_eq!(
            info_ui_row(&out),
            "tui",
            "COYOTE_UI={value:?} displaced ui: tui"
        );
    }

    write_dry_run_config(&dir);
    for value in ["tui", "TUI", "bogus", "repl,tui", "tui.", "--tui"] {
        let out = run_piped(&dir, &[("COYOTE_UI", value)], &["--info"]);
        assert_eq!(
            info_ui_row(&out),
            "repl",
            "COYOTE_UI={value:?} displaced the default"
        );
    }

    let out = run_piped(&dir, &[("COYOTE_UI", "repl")], &["--tui", "--info"]);
    assert_eq!(
        info_ui_row(&out),
        "tui",
        "--tui under COYOTE_UI=repl was not reported"
    );
}

/// A typo in the `ui:` key is refused by name with the accepted values;
/// the run never reaches `--info`.
#[test]
#[serial]
fn a_bogus_ui_key_is_a_config_error_naming_the_accepted_values() {
    let dir = fresh_config_dir("bogus-ui-key");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: bogus\n");

    let out = run_piped(&dir, &[], &["--info"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "ui: bogus was accepted:\n{stdout}");
    assert!(
        stderr.contains("Invalid value 'bogus', expected one of: repl, tui"),
        "the error did not name the value and the accepted values:\n{stderr}"
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.split_whitespace().next() == Some("ui")),
        "--info still printed a ui row:\n{stdout}"
    );
}

/// `COYOTE_UI` is never parsed on start: a value no shell is named after
/// stops neither a prompt run nor a headless run, and the model answers.
#[test]
#[serial]
fn a_bogus_coyote_ui_value_never_stops_prompt_or_headless_runs() {
    let dir = fresh_config_dir("bogus-ui-env-prompt");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    for args in [
        &["say hello-sentinel"][..],
        &["--headless", "say hello-sentinel"][..],
    ] {
        let out = run_piped(&dir, &[("COYOTE_UI", "bogus")], args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");
        assert!(
            out.status.success(),
            "{args:?} failed under COYOTE_UI=bogus:\n{all}"
        );
        assert!(
            !all.contains("Invalid value 'bogus'"),
            "{args:?} parsed COYOTE_UI=bogus as a setting:\n{all}"
        );
        assert!(
            stdout.contains("hello-sentinel"),
            "{args:?} never reached the model under COYOTE_UI=bogus:\n{all}"
        );
    }
}

/// Both shell flags exclude every non-interactive mode at the parser:
/// usage error, exit 2, nothing started and no file read (the `-f`
/// path need not exist).
#[test]
#[serial]
fn the_shell_flags_conflict_with_every_prompt_path_in_clap() {
    let dir = fresh_config_dir("shell-flag-conflict");
    let _guard = TempDirGuard(dir.clone());

    for args in [
        &["--repl", "-e"][..],
        &["--repl", "--headless"],
        &["--repl", "--acp-server"],
        &["--repl", "-f", "prompt.txt"],
        &["--repl", "--macro", "m"],
        &["--tui", "--headless"],
        &["--tui", "--acp-server"],
        &["--tui", "-f", "prompt.txt"],
        &["--tui", "--macro", "m"],
    ] {
        let out = run_piped(&dir, &[], args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?} did not exit with clap's usage error code:\n{all}"
        );
        assert!(
            all.contains("cannot be used with"),
            "{args:?} was not reported as a clap conflict:\n{all}"
        );
        assert!(
            !all.contains("Welcome to"),
            "{args:?} started the REPL:\n{all}"
        );
    }
}

#[cfg(unix)]
mod sandbox_relaunch {
    use super::*;

    const SBX_CALL_END: &str = "<<END>>";
    const SBX_INNER_STDOUT: &str = "INNER-STDOUT";

    /// A stub `sbx` first on `PATH` that logs each call one argument per line,
    /// lists `box` and `default_name` as running sandboxes so no create runs,
    /// and on `exec -i` prints a marker instead of starting anything. Returns
    /// the `PATH` value to run under and the call log.
    pub(super) fn install_stub_sbx(
        dir: &Path,
        default_name: &str,
    ) -> (std::ffi::OsString, PathBuf) {
        let bin = dir.join("stub-bin");
        fs::create_dir_all(&bin).unwrap();
        let log = dir.join("stub-sbx-calls");
        let stub = bin.join("sbx");
        fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\" >> '{log}'; done\n\
             printf '%s\\n' '{SBX_CALL_END}' >> '{log}'\n\
             if [ \"$1\" = \"version\" ]; then echo 'sbx version v0.49.1 c5b1995'; exit 0; fi\n\
             if [ \"$1\" = \"ls\" ]; then printf 'NAME STATUS\\nbox running\\n{default_name} running\\n'; exit 0; fi\n\
             if [ \"$1\" = \"exec\" ] && [ \"$2\" = \"-i\" ]; then echo '{SBX_INNER_STDOUT}'; exit 0; fi\n\
             exit 0\n",
            log = log.display()
        ),
    )
    .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
        let path = env::join_paths(
            [bin, fixture_bin]
                .into_iter()
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        (path, log)
    }

    pub(super) fn sbx_calls(log: &Path) -> Vec<Vec<String>> {
        let text = fs::read_to_string(log).unwrap_or_default();
        let mut calls = Vec::new();
        let mut current = Vec::new();
        for line in text.lines() {
            if line == SBX_CALL_END {
                calls.push(std::mem::take(&mut current));
            } else {
                current.push(line.to_string());
            }
        }
        calls
    }

    /// The sandbox flags are split off before the parser and everything else
    /// is forwarded, so a shell flag next to `--sandbox` (named or not, with
    /// or without `--fresh`, in either order) relaunches into the sandbox and
    /// the inner Coyote gets the shell flag alone: `--sandbox`,
    /// `--sandbox-name` and `--fresh` never reach it.
    #[test]
    #[serial]
    fn the_shell_flags_are_forwarded_into_the_sandbox_relaunch() {
        let dir = fresh_config_dir("shell-flag-sandbox-forward");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let default_name = dir.file_name().unwrap().to_str().unwrap().to_string();

        let cases: [(&[&str], &str, &str); 8] = [
            (&["--sandbox", "--tui"], &default_name, "--tui"),
            (&["--sandbox", "--repl"], &default_name, "--repl"),
            (
                &["--sandbox", "--sandbox-name", "box", "--tui"],
                "box",
                "--tui",
            ),
            (
                &["--sandbox", "--sandbox-name", "box", "--repl"],
                "box",
                "--repl",
            ),
            (&["--sandbox", "--fresh", "--tui"], &default_name, "--tui"),
            (&["--sandbox", "--fresh", "--repl"], &default_name, "--repl"),
            (&["--tui", "--sandbox"], &default_name, "--tui"),
            (&["--repl", "--sandbox"], &default_name, "--repl"),
        ];
        let (path, log) = install_stub_sbx(&dir, &default_name);
        let path = path.to_str().unwrap().to_string();
        for (args, name, shell_flag) in cases {
            let _ = fs::remove_file(&log);
            let out = run_piped(&dir, &[("PATH", &path)], args);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
            let all = format!("{stdout}\n{stderr}");
            let execs: Vec<Vec<String>> = sbx_calls(&log)
                .into_iter()
                .filter(|c| c.len() > 1 && c[0] == "exec" && c[1] == "-i")
                .collect();
            assert_eq!(
                execs.len(),
                1,
                "{args:?} did not relaunch into the sandbox exactly once: {execs:?}\n{all}"
            );
            let exec = &execs[0];
            let coyote = exec
                .iter()
                .position(|arg| arg == "coyote")
                .unwrap_or_else(|| panic!("{args:?}: no inner coyote in {exec:?}"));
            assert_eq!(
                exec[coyote - 1],
                name,
                "{args:?}: wrong sandbox in {exec:?}"
            );
            assert_eq!(
                exec[coyote + 1..],
                [shell_flag],
                "{args:?}: the inner argv is not the shell flag alone"
            );
            assert_eq!(out.status.code(), Some(0), "{args:?}:\n{all}");
            assert_eq!(stdout, format!("{SBX_INNER_STDOUT}\n"), "{args:?}:\n{all}");
            assert!(
                find(&out.stdout, ENTER_ALT, 0).is_none(),
                "{args:?} started a shell on the host:\n{all}"
            );
        }
    }
}

/// `--fresh` without `--sandbox` is a usage error that never calls `sbx`
/// and never starts a shell, whatever shell flag rides along.
#[test]
#[serial]
fn fresh_without_the_sandbox_flag_is_a_usage_error() {
    let dir = fresh_config_dir("fresh-without-sandbox");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    #[cfg(unix)]
    let (path, log) = sandbox_relaunch::install_stub_sbx(&dir, "unused");
    #[cfg(unix)]
    let extra_env = [("PATH", path.to_str().unwrap())];
    #[cfg(not(unix))]
    let extra_env: [(&str, &str); 0] = [];
    let args = ["--fresh", "--tui"];
    let out = run_piped(&dir, &extra_env, &args);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    let all = format!("{stdout}\n{stderr}");
    assert_eq!(
        out.status.code(),
        Some(2),
        "{args:?} did not exit with clap's usage error code:\n{all}"
    );
    assert!(
        stderr.contains("the following required arguments were not provided:\n  --sandbox\n"),
        "{args:?} was not refused for the missing --sandbox:\n{all}"
    );
    #[cfg(unix)]
    assert!(
        sandbox_relaunch::sbx_calls(&log).is_empty(),
        "{args:?} called sbx: {:?}",
        sandbox_relaunch::sbx_calls(&log)
    );
    assert!(
        !all.contains("Welcome to") && find(&out.stdout, ENTER_ALT, 0).is_none(),
        "{args:?} started a shell:\n{all}"
    );
}

/// A shell preference is only a preference for interactive runs: a
/// one-shot prompt or `-e` with `ui: tui` in the file runs to completion
/// on the normal screen like any other prompt run.
#[test]
#[serial]
fn a_tui_preference_never_blocks_a_one_shot_run() {
    let dir = fresh_config_dir("tui-pref-prompt");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: tui\n");

    for args in [&["say hello-sentinel"][..], &["-e", "say hello-sentinel"]] {
        let out = run_piped(&dir, &[], args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");
        let what = format!("{args:?} with ui: tui");
        assert!(out.status.success(), "{what} failed:\n{all}");
        assert!(
            stdout.contains("hello-sentinel"),
            "{what} never reached the model:\n{all}"
        );
        assert!(
            find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
            "{what} entered the alternate screen:\n{all}"
        );
        assert!(
            !all.contains("requires the interactive mode"),
            "{what} was refused like a shell flag:\n{all}"
        );
    }
}

/// A prompt arriving on a piped stdin is a one-shot run too, so both
/// shell flags refuse it with the same message as a positional prompt.
#[test]
#[serial]
fn a_prompt_on_stdin_is_refused_by_the_shell_flags_like_a_positional_one() {
    let dir = fresh_config_dir("shell-flag-stdin");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    for flag in ["--tui", "--repl"] {
        let out = run_with_stdin(&dir, &[], &[flag], Some("say hello-sentinel\n"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");
        assert!(
            !out.status.success(),
            "{flag} with a prompt on stdin exited 0 instead of refusing:\n{all}"
        );
        assert!(
            all.contains(&format!(
                "{flag} requires the interactive mode; drop the prompt"
            )),
            "{flag} with a prompt on stdin did not give the positional refusal:\n{all}"
        );
        assert!(
            !stdout.contains("hello-sentinel"),
            "{flag} with a prompt on stdin reached the model:\n{all}"
        );
        assert!(
            find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
            "{flag} with a prompt on stdin entered the alternate screen:\n{all}"
        );
    }
}

/// The two accepted values are matched as written: a differently cased
/// spelling in the file is reported by name with the accepted values.
#[test]
#[serial]
fn a_differently_cased_ui_value_is_rejected_by_name() {
    let dir = fresh_config_dir("ui-case");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: Tui\n");
    let out = run_piped(&dir, &[], &["--info"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "ui: Tui was accepted");
    assert!(
        stderr.contains("Invalid value 'Tui', expected one of: repl, tui"),
        "ui: Tui was not reported by name:\n{stderr}"
    );
}

/// With no config file at all the `ui` row still resolves: the REPL by
/// default, and the flag when given.
#[test]
#[serial]
fn a_missing_config_file_still_resolves_the_ui_row() {
    let dir = fresh_config_dir("no-config-file");
    let _guard = TempDirGuard(dir.clone());
    assert!(!dir.join("config.yaml").exists());

    let out = run_piped(&dir, &[], &["--info"]);
    assert_eq!(
        info_ui_row(&out),
        "repl",
        "the default ui without a config file was not repl"
    );

    let out = run_piped(&dir, &[], &["--tui", "--info"]);
    assert_eq!(
        info_ui_row(&out),
        "tui",
        "--tui without a config file was not reported"
    );
}

/// Both shell flags are documented in `--help` under the Shell heading
/// with their one-line purpose, so a first-time user can find them.
#[test]
#[serial]
fn help_lists_both_shell_flags_under_the_shell_heading() {
    let dir = fresh_config_dir("help-shell-flags");
    let _guard = TempDirGuard(dir.clone());

    let out = run_piped(&dir, &[], &["--help"]);
    assert!(out.status.success(), "--help failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let plain = strip_ansi(&stdout);
    let shell_section: String = plain
        .lines()
        .skip_while(|line| !line.starts_with("Shell:"))
        .skip(1)
        .take_while(|line| line.starts_with(' ') || line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        shell_section.contains("--tui") && shell_section.contains("full-screen UI"),
        "--tui is not documented under Shell:\n{plain}"
    );
    assert!(
        shell_section.contains("--repl") && shell_section.contains("REPL"),
        "--repl is not documented under Shell:\n{plain}"
    );
}

/// Choosing the full-screen UI by flag or by the `ui:` key reaches the
/// same tty check: without a tty on stdin the run says `No TTY for TUI`,
/// exits 1, and neither the alternate screen nor the REPL banner is
/// written.
#[test]
#[serial]
fn the_tui_flag_and_key_without_a_tty_are_refused_alike() {
    let dir = fresh_config_dir("tui-flag-no-tty");
    let _guard = TempDirGuard(dir.clone());

    write_dry_run_config(&dir);
    let by_flag = run_piped(&dir, &[], &["--tui"]);
    write_config(&dir, "save_session: false\nui: tui\n");
    let by_key = run_piped(&dir, &[], &[]);

    for (what, out) in [("--tui", by_flag), ("ui: tui", by_key)] {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}\n{stderr}");
        assert_eq!(
            out.status.code(),
            Some(1),
            "{what} without a tty did not exit 1:\n{all}"
        );
        assert!(
            stderr.contains("No TTY for TUI"),
            "{what} without a tty gave no tty refusal on stderr:\n{all}"
        );
        assert!(
            find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
            "{what} without a tty entered the alternate screen:\n{all}"
        );
        assert!(
            !all.contains("Welcome to"),
            "{what} without a tty fell through to the REPL:\n{all}"
        );
    }
}

/// The prompt refusal is an error, not output: exit 1, the message on
/// stderr, and nothing at all on stdout, so a script wrapping the run
/// cannot mistake the refusal for a reply.
#[test]
#[serial]
fn a_shell_flag_prompt_refusal_is_stderr_only_with_exit_one() {
    let dir = fresh_config_dir("shell-flag-prompt-streams");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    for flag in ["--tui", "--repl"] {
        let out = run_piped(&dir, &[], &[flag, "say hello-sentinel"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{flag} with a prompt did not exit 1:\n{stderr}"
        );
        assert!(
            out.stdout.is_empty(),
            "{flag} with a prompt wrote to stdout: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            stderr.contains(&format!(
                "{flag} requires the interactive mode; drop the prompt"
            )),
            "{flag} refusal was not on stderr:\n{stderr}"
        );
    }
}

/// A `--headless` run is the one-shot path too: `COYOTE_UI=tui`, as a
/// child of a full-screen session inherits it, never blocks it and the
/// reply arrives on the normal screen.
#[test]
#[serial]
fn a_tui_env_never_blocks_a_headless_run() {
    let dir = fresh_config_dir("tui-env-headless");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    let out = run_piped(
        &dir,
        &[("COYOTE_UI", "tui")],
        &["--headless", "say hello-sentinel"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}\n{stderr}");
    assert!(
        out.status.success(),
        "--headless under COYOTE_UI=tui failed:\n{all}"
    );
    assert!(
        stdout.contains("hello-sentinel"),
        "--headless under COYOTE_UI=tui never reached the model:\n{all}"
    );
    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "--headless under COYOTE_UI=tui entered the alternate screen:\n{all}"
    );
    assert!(
        !all.contains("requires the interactive mode") && !all.contains("No TTY"),
        "--headless under COYOTE_UI=tui was refused:\n{all}"
    );
}

/// A shell flag decides the shell, it does not skip validation: a
/// typo in the `ui:` key is still refused by name when the flag is
/// overriding it.
#[test]
#[serial]
fn a_shell_flag_does_not_silence_a_bogus_key() {
    let dir = fresh_config_dir("flag-over-bogus");
    let _guard = TempDirGuard(dir.clone());

    write_config(&dir, "save_session: false\nui: bogus\n");
    for flag in ["--tui", "--repl"] {
        let out = run_piped(&dir, &[], &[flag, "--info"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{flag} accepted ui: bogus");
        assert!(
            stderr.contains("Invalid value 'bogus', expected one of: repl, tui"),
            "{flag} over ui: bogus did not name the bad value:\n{stderr}"
        );
    }
}

/// The generated completions for the common shells know both flags,
/// so a user who tabs after `coyote --` can find them.
#[test]
#[serial]
fn completions_for_the_common_shells_name_both_shell_flags() {
    let dir = fresh_config_dir("completions-shell-flags");
    let _guard = TempDirGuard(dir.clone());

    for (shell, tui, repl) in [
        ("bash", "--tui", "--repl"),
        ("zsh", "--tui", "--repl"),
        ("fish", "-l tui", "-l repl"),
    ] {
        let out = run_piped(&dir, &[], &["--completions", shell]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "--completions {shell} failed");
        assert!(
            stdout.contains(tui) && stdout.contains(repl),
            "{shell} completions do not list both shell flags:\n{stdout}"
        );
    }
}

/// `config.example.yaml` and the config template ship the two new rows,
/// and those rows, taken verbatim, load into a config whose `--info`
/// resolves the documented default, the REPL.
#[test]
#[serial]
fn the_documented_ui_rows_load_verbatim_and_resolve_to_the_repl() {
    let dir = fresh_config_dir("documented-ui-rows");
    let _guard = TempDirGuard(dir.clone());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    for file in ["config.example.yaml", "assets/config-template.yaml"] {
        let text = fs::read_to_string(root.join(file)).unwrap();
        let rows: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("ui:") || line.starts_with("tui_dump_on_exit:"))
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "{file} lacks the ui / tui_dump_on_exit rows:\n{text}"
        );
        assert!(
            rows.iter().any(|row| row.starts_with("ui: repl")),
            "{file} documents a default other than `ui: repl`: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| row.is_ascii()),
            "{file} has a non-ASCII ui row: {rows:?}"
        );

        write_config(&dir, &format!("save_session: false\n{}\n", rows.join("\n")));
        let out = run_piped(&dir, &[], &["--info"]);
        assert_eq!(
            info_ui_row(&out),
            "repl",
            "the rows from {file} did not resolve to repl"
        );
    }
}

/// `echo | coyote` gives the binary a pipe, not a tty, on stdin. With
/// nothing on the pipe the run is still an interactive start, and the
/// full-screen UI refuses it the same way it refuses `/dev/null`:
/// `No TTY for TUI` on stderr, exit 1, nothing on stdout.
#[test]
#[serial]
fn the_full_screen_ui_refuses_an_empty_pipe_on_stdin() {
    let dir = fresh_config_dir("tui-empty-stdin-pipe");
    let _guard = TempDirGuard(dir.clone());

    write_dry_run_config(&dir);
    let by_flag = run_with_stdin(&dir, &[], &["--tui"], Some(""));
    write_config(&dir, "save_session: false\nui: tui\n");
    let by_key = run_with_stdin(&dir, &[], &[], Some(""));

    for (what, out) in [("--tui", by_flag), ("ui: tui", by_key)] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{what} with an empty stdin pipe did not exit 1:\n{stderr}"
        );
        assert!(
            stderr.contains("No TTY for TUI"),
            "{what} with an empty stdin pipe gave no tty refusal:\n{stderr}"
        );
        assert!(
            out.stdout.is_empty(),
            "{what} with an empty stdin pipe wrote to stdout: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            !stderr.contains("Welcome to"),
            "{what} with an empty stdin pipe fell through to the REPL:\n{stderr}"
        );
    }
}

/// A prompt piped in (`echo prompt | coyote`) is a one-shot run, so a
/// shell preference from the file is ignored for it just as for a
/// positional prompt: the reply arrives on stdout, exit 0, and neither
/// the tty check nor the shell-flag refusal fires.
#[test]
#[serial]
fn a_tui_preference_never_blocks_a_prompt_on_stdin() {
    let dir = fresh_config_dir("tui-pref-stdin-prompt");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: tui\n");

    let out = run_with_stdin(&dir, &[], &[], Some("say hello-sentinel\n"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}\n{stderr}");
    let what = "a stdin prompt with ui: tui";
    assert!(out.status.success(), "{what} failed:\n{all}");
    assert!(
        stdout.contains("hello-sentinel"),
        "{what} never reached the model:\n{all}"
    );
    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "{what} entered the alternate screen:\n{all}"
    );
    assert!(
        !all.contains("No TTY") && !all.contains("requires the interactive mode"),
        "{what} was refused:\n{all}"
    );
}

/// YAML spellings of the key: a quoted scalar is the same value as a
/// bare one, while an absent value (`ui:` or `ui: null`) is not a
/// shell and is refused by name with the accepted values, like any
/// other typo, rather than silently falling back to the default.
#[test]
#[serial]
fn a_quoted_ui_value_loads_and_an_absent_one_is_refused_by_name() {
    let dir = fresh_config_dir("ui-yaml-spellings");
    let _guard = TempDirGuard(dir.clone());

    write_config(&dir, "save_session: false\nui: \"tui\"\n");
    let out = run_piped(&dir, &[], &["--info"]);
    assert_eq!(
        info_ui_row(&out),
        "tui",
        "a quoted `ui: \"tui\"` did not load"
    );

    for (row, shown) in [("ui:", ""), ("ui: null", "null")] {
        write_config(&dir, &format!("save_session: false\n{row}\n"));
        let out = run_piped(&dir, &[], &["--info"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "`{row}` was accepted:\n{stdout}");
        assert!(
            stderr.contains(&format!(
                "Invalid value '{shown}', expected one of: repl, tui"
            )),
            "`{row}` was not refused by name:\n{stderr}"
        );
    }
}

/// `--version` and `--completions` answer before any shell is chosen,
/// so a shell flag beside them is harmless: exit 0 and the usual
/// output, without a tty and without touching the config.
#[test]
#[serial]
fn the_shell_flags_do_not_disturb_version_or_completions() {
    let dir = fresh_config_dir("shell-flag-inspection");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\nui: bogus\n");

    for (flag, args, needle) in [
        ("--tui", &["--version"][..], "coyote "),
        ("--repl", &["--version"][..], "coyote "),
        ("--tui", &["--completions", "bash"][..], "--tui"),
        ("--repl", &["--completions", "bash"][..], "--repl"),
    ] {
        let mut full = vec![flag];
        full.extend_from_slice(args);
        let out = run_piped(&dir, &[], &full);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{full:?} failed:\n{stdout}\n{stderr}");
        assert!(
            stdout.contains(needle),
            "{full:?} did not print the expected output:\n{stdout}"
        );
        assert!(
            !stderr.contains("No TTY") && !stderr.contains("Invalid value"),
            "{full:?} reached the shell or the config:\n{stderr}"
        );
    }
}
