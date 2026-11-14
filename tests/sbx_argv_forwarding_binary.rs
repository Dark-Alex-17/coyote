//! Black-box contract for the `--sandbox` argv split.
//!
//! The `sbx` stub records each call one argument per line (so argv
//! boundaries survive), reports the sandbox `probe` as existing so no create
//! runs, and on `exec` prints a marker to stdout and stderr and then exits
//! with `STUB_EXEC_EXIT` or kills itself with `STUB_EXEC_SIGNAL`.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

const CALL_END: &str = "<<END>>";
const INNER_STDOUT: &str = "INNER-STDOUT";
const SANDBOX_REQUIRED: &str = "the following required arguments were not provided:\n  --sandbox\n";

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_root(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-sbx-argv-{label}-{}-{unique}",
        process::id()
    ));
    for sub in ["home", "config", "cache", "bin", "work/probe"] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir.canonicalize().unwrap()
}

fn write_dry_run_config(root: &Path) {
    let vault_pass = root.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        root.join("config/config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             save: false\n\
             memory: false\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn install_stub_sbx(root: &Path) -> PathBuf {
    let log = root.join("stub-sbx-calls");
    let stub = root.join("bin/sbx");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do printf '%s\\n' \"$a\" >> '{log}'; done\n\
             printf '%s\\n' '{CALL_END}' >> '{log}'\n\
             if [ \"$1\" = \"version\" ]; then echo 'sbx version v0.49.1 c5b1995'; exit 0; fi\n\
             if [ \"$1\" = \"secret\" ] && [ \"$2\" = \"ls\" ]; then exit 0; fi\n\
             if [ \"$1\" = \"ls\" ]; then printf 'NAME STATUS\\nprobe running\\nwork running\\n'; exit 0; fi\n\
             if [ \"$1\" = \"exec\" ] && [ \"$2\" = \"-i\" ]; then\n\
             \x20 echo '{INNER_STDOUT}'; echo 'INNER-STDERR' >&2\n\
             \x20 if [ -n \"$STUB_EXEC_SIGNAL\" ]; then kill -\"$STUB_EXEC_SIGNAL\" $$; fi\n\
             \x20 exit \"${{STUB_EXEC_EXIT:-0}}\"\n\
             fi\n\
             if [ \"$1\" = \"run\" ]; then echo 'RUN-STDOUT'; exit \"${{STUB_EXEC_EXIT:-0}}\"; fi\n\
             exit 0\n",
            log = log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    log
}

/// Removes every inherited `COYOTE_*` variable, so a developer's own
/// override (`COYOTE_CONFIG_FILE` beats the scratch config dir) never
/// reaches the child; the harness then sets the ones it needs.
fn scrub_coyote_env(cmd: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("COYOTE_")) {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn run_coyote_in(root: &Path, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let home = root.join("home");
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_coyote"));
    scrub_coyote_env(&mut command)
        .args(args)
        .current_dir(cwd)
        .env("PWD", cwd)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_SANDBOX_KIT")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .env_remove("STUB_EXEC_EXIT")
        .env_remove("STUB_EXEC_SIGNAL")
        .stdin(Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn run_coyote(root: &Path, args: &[&str]) -> Output {
    run_coyote_in(root, root, args, &[])
}

fn calls(log: &Path) -> Vec<Vec<String>> {
    let text = fs::read_to_string(log).unwrap_or_default();
    let mut out = Vec::new();
    let mut current = Vec::new();
    for line in text.lines() {
        if line == CALL_END {
            out.push(std::mem::take(&mut current));
        } else {
            current.push(line.to_string());
        }
    }
    out
}

fn exec_calls(log: &Path) -> Vec<Vec<String>> {
    calls(log)
        .into_iter()
        .filter(|c| {
            c.first().map(String::as_str) == Some("exec")
                && c.get(1).map(String::as_str) == Some("-i")
        })
        .collect()
}

fn expected_exec(cwd: &Path, name: &str, forwarded: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = ["exec", "-i", "-w"].iter().map(|s| s.to_string()).collect();
    v.push(cwd.display().to_string());
    v.push(name.to_string());
    v.push("coyote".to_string());
    v.extend(forwarded.iter().map(|s| s.to_string()));
    v
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn setup(label: &str) -> (PathBuf, TempDirGuard, PathBuf) {
    let root = fresh_root(label);
    let guard = TempDirGuard(root.clone());
    let log = install_stub_sbx(&root);
    write_dry_run_config(&root);
    (root, guard, log)
}

#[test]
fn usage_probe_forwards_non_host_tokens_in_order_and_the_tail_after_dashdash_untouched() {
    let (root, _g, log) = setup("order");
    let args = [
        "--headless",
        "--sandbox",
        "-m",
        "x:y",
        "--sandbox-name",
        "probe",
        "--info",
        "--fresh",
        "two words",
        "--",
        "--sandbox",
        "--sandbox-name",
        "other",
        "--fresh",
    ];
    let out = run_coyote(&root, &args);
    let execs = exec_calls(&log);
    assert_eq!(
        execs,
        vec![expected_exec(
            &root,
            "probe",
            &[
                "--headless",
                "-m",
                "x:y",
                "--info",
                "two words",
                "--",
                "--sandbox",
                "--sandbox-name",
                "other",
                "--fresh"
            ]
        )],
        "stderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn usage_probe_sandbox_name_equals_form_is_consumed() {
    let (root, _g, log) = setup("equals");
    let out = run_coyote(&root, &["--sandbox", "--sandbox-name=probe", "--info"]);
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&root, "probe", &["--info"])],
        "stderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn usage_probe_sandbox_name_defaults_to_cwd_basename() {
    let (root, _g, log) = setup("default-name");
    let cwd = root.join("work");
    let out = run_coyote_in(&root, &cwd, &["--sandbox", "--info"], &[]);
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&cwd, "work", &["--info"])],
        "stderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn usage_probe_host_actions_and_unknown_flags_are_forwarded_not_validated() {
    for flag in ["--help", "--install", "--completions", "--not-a-flag-yet"] {
        let (root, _g, log) = setup("forward");
        let out = run_coyote(&root, &["--sandbox", "--sandbox-name", "probe", flag]);
        assert_eq!(
            exec_calls(&log),
            vec![expected_exec(&root, "probe", &[flag])],
            "{flag}: stderr:\n{}",
            text(&out.stderr)
        );
        assert_eq!(text(&out.stdout), format!("{INNER_STDOUT}\n"), "{flag}");
    }
}

#[test]
fn usage_probe_only_the_inner_child_writes_stdout() {
    let (root, _g, _log) = setup("stdout");
    let out = run_coyote(
        &root,
        &["--sandbox", "--sandbox-name", "probe", "--headless", "hi"],
    );
    assert_eq!(text(&out.stdout), format!("{INNER_STDOUT}\n"));
    assert!(
        text(&out.stderr).contains("INNER-STDERR"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn usage_probe_inner_exit_code_propagates() {
    for code in ["0", "2", "7", "127"] {
        let (root, _g, _log) = setup("exit");
        let out = run_coyote_in(
            &root,
            &root,
            &["--sandbox", "--sandbox-name", "probe", "--headless", "hi"],
            &[("STUB_EXEC_EXIT", code)],
        );
        assert_eq!(
            out.status.code(),
            Some(code.parse().unwrap()),
            "stderr:\n{}",
            text(&out.stderr)
        );
    }
}

#[test]
fn usage_probe_inner_signal_maps_to_128_plus_n() {
    let (root, _g, _log) = setup("signal");
    let out = run_coyote_in(
        &root,
        &root,
        &["--sandbox", "--sandbox-name", "probe", "--headless", "hi"],
        &[("STUB_EXEC_SIGNAL", "TERM")],
    );
    assert_eq!(
        out.status.code(),
        Some(128 + 15),
        "stderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn usage_probe_sandbox_after_dashdash_is_not_sandbox_mode() {
    let (root, _g, log) = setup("after-dashdash");
    let out = run_coyote(&root, &["--info", "--", "--sandbox"]);
    assert!(calls(&log).is_empty(), "{:?}", calls(&log));
    let stderr = text(&out.stderr);
    assert!(
        !stderr.contains("--sandbox reached the host argument parser"),
        "{stderr}"
    );
}

#[test]
fn usage_probe_malformed_host_tokens_are_refused_before_sbx() {
    let no_value = "--sandbox takes no value; pass the name with --sandbox-name <NAME>";
    let cases: [(&[&str], &str); 7] = [
        (
            &["--sandbox", "--sandbox-name"],
            "--sandbox-name requires a value",
        ),
        (
            &["--sandbox", "--sandbox-name", "-m"],
            "--sandbox-name requires a value (got '-m', which looks like a flag)",
        ),
        (&["--sandbox", "--sandbox=box"], no_value),
        (&["--sandbox=box"], no_value),
        (&["--sandbox", "--fresh="], "--fresh takes no value"),
        (&["--sandbox", "--fresh=yes"], "--fresh takes no value"),
        (&["--sandbox-name", "box", "--info"], SANDBOX_REQUIRED),
    ];
    for (args, needle) in cases {
        let (root, _g, log) = setup("malformed");
        let out = run_coyote(&root, args);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            stderr.contains(needle),
            "{args:?}: stderr lacks {needle}:\n{stderr}"
        );
        assert!(calls(&log).is_empty(), "{args:?}: {:?}", calls(&log));
    }
}

#[test]
fn usage_probe_fresh_requires_sandbox() {
    let (root, _g, log) = setup("fresh");
    let out = run_coyote(&root, &["--fresh", "--info"]);
    assert!(!out.status.success());
    let stderr = text(&out.stderr);
    assert!(stderr.contains(SANDBOX_REQUIRED), "{stderr}");
    assert!(calls(&log).is_empty());
}

/// A resolved symlink target (macOS `/tmp` -> `/private/tmp`) does not exist
/// inside the sandbox, so the inner exec would fail to chdir.
#[test]
fn usage_probe_exec_workdir_is_the_logical_cwd_under_a_symlink() {
    let (root, _g, log) = setup("symlink");
    let link = root.join("probe-link");
    std::os::unix::fs::symlink(root.join("work/probe"), &link).unwrap();
    let out = run_coyote_in(
        &root,
        &link,
        &["--sandbox", "--sandbox-name", "probe", "--info"],
        &[],
    );
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&link, "probe", &["--info"])],
        "stderr:\n{}",
        text(&out.stderr)
    );
}

/// A flag-shaped value means the name was forgotten; taking it would swallow
/// the next flag as the sandbox name.
#[test]
fn usage_probe_empty_or_flag_shaped_sandbox_name_is_refused() {
    let cases: [&[&str]; 3] = [
        &["--sandbox", "--sandbox-name", "", "--info"],
        &["--sandbox", "--sandbox-name=", "--info"],
        &["--sandbox", "--sandbox-name", "--info"],
    ];
    for args in cases {
        let (root, _g, log) = setup("bad-name");
        let out = run_coyote(&root, args);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            stderr.contains("--sandbox-name requires a value"),
            "{args:?}:\n{stderr}"
        );
        assert!(calls(&log).is_empty(), "{args:?}: {:?}", calls(&log));
        assert!(text(&out.stdout).is_empty(), "{args:?}");
    }
}

#[test]
fn usage_probe_sandbox_name_before_sandbox_is_consumed() {
    let (root, _g, log) = setup("name-first");
    let out = run_coyote(&root, &["--sandbox-name", "probe", "--info", "--sandbox"]);
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&root, "probe", &["--info"])],
        "stderr:\n{}",
        text(&out.stderr)
    );
}

/// The stub reports no existing sandbox so the first-launch setup children
/// run, and drains stdin on every setup call so a child that inherited the
/// pipe would steal the payload.
#[test]
fn usage_probe_piped_stdin_reaches_only_the_inner_exec() {
    let (root, _g, log) = setup("stdin");
    let stub = root.join("bin/sbx");
    let script = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a" >> '@ROOT@/stub-sbx-calls'; done
printf '%s\n' '<<END>>' >> '@ROOT@/stub-sbx-calls'
if [ "$1" = "version" ]; then echo 'sbx version v0.49.1 c5b1995'; exit 0; fi
if [ "$1" = "exec" ] && [ "$2" = "-i" ]; then cat > '@ROOT@/exec-stdin'; echo INNER-STDOUT; exit 0; fi
cat >> '@ROOT@/setup-stdin'
if [ "$1" = "ls" ]; then printf 'NAME STATUS\n'; fi
exit 0
"#
    .replace("@ROOT@", &root.display().to_string());
    fs::write(&stub, script).unwrap();

    let home = root.join("home");
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    let mut child = scrub_coyote_env(&mut Command::new(env!("CARGO_BIN_EXE_coyote")))
        .args(["--sandbox", "--sandbox-name", "probe", "--headless"])
        .current_dir(&root)
        .env("PWD", &root)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", &home)
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_SANDBOX_KIT")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"PIPED-PAYLOAD\n").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let setup_calls: Vec<Vec<String>> = calls(&log)
        .into_iter()
        .filter(|c| c[0] != "version" && c.get(1).map(String::as_str) != Some("-i"))
        .collect();
    assert!(
        setup_calls.iter().any(|c| c[0] == "create"),
        "{setup_calls:?}"
    );
    assert!(
        setup_calls
            .iter()
            .any(|c| c[0] == "exec" && c[2..4] == ["sh", "-c"]),
        "{setup_calls:?}"
    );
    assert!(setup_calls.iter().any(|c| c[0] == "cp"), "{setup_calls:?}");
    assert_eq!(
        fs::read_to_string(root.join("setup-stdin")).unwrap(),
        "",
        "a setup child read the piped stdin"
    );
    assert_eq!(
        fs::read_to_string(root.join("exec-stdin")).unwrap(),
        "PIPED-PAYLOAD\n"
    );
    assert_eq!(text(&out.stdout), format!("{INNER_STDOUT}\n"));
}

/// The terminal comes from `script(1)`; the test is skipped when it is absent.
#[test]
fn usage_probe_terminal_launch_picks_run_or_exec_by_mode() {
    if Command::new("script").arg("-V").output().is_err() && !Path::new("/usr/bin/script").exists()
    {
        eprintln!("script(1) unavailable; skipping");
        return;
    }
    let cases: [(&[&str], Vec<&str>); 5] = [
        (&[], vec!["run", "--name", "probe"]),
        (&["--info"], vec!["run", "--name", "probe", "--", "--info"]),
        (
            &["--info", "--", "--headless"],
            vec!["run", "--name", "probe", "--", "--info", "--", "--headless"],
        ),
        (&["--headless", "hi"], vec!["exec", "-w"]),
        (&["--acp-server"], vec!["exec", "-w"]),
    ];
    for (extra, want_prefix) in cases {
        let (root, _g, log) = setup("tty");
        let mut argv: Vec<String> = vec![env!("CARGO_BIN_EXE_coyote").to_string()];
        argv.extend(
            ["--sandbox", "--sandbox-name", "probe"]
                .iter()
                .chain(extra.iter())
                .map(|s| s.to_string()),
        );
        let mut command = Command::new("script");
        if cfg!(target_os = "linux") {
            let quoted: Vec<String> = argv
                .iter()
                .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
                .collect();
            command.args(["-qec", &quoted.join(" "), "/dev/null"]);
        } else {
            command.args(["-q", "/dev/null"]).args(&argv);
        }
        let path = format!(
            "{}:{}",
            root.join("bin").display(),
            env::var("PATH").unwrap_or_default()
        );
        let out = scrub_coyote_env(&mut command)
            .current_dir(&root)
            .env("PWD", &root)
            .env("COYOTE_CONFIG_DIR", root.join("config"))
            .env("COYOTE_CACHE_DIR", root.join("cache"))
            .env("HOME", root.join("home"))
            .env("PATH", path)
            .env("NO_COLOR", "1")
            .env_remove("COYOTE_SANDBOX_KIT")
            .env_remove("IS_SANDBOX")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let launches: Vec<Vec<String>> = calls(&log)
            .into_iter()
            .filter(|c| {
                c[0] == "run"
                    || (c[0] == "exec" && matches!(c.get(1).map(String::as_str), Some("-i" | "-w")))
            })
            .collect();
        assert_eq!(
            launches.len(),
            1,
            "{extra:?}: {launches:?}\n{}",
            text(&out.stdout)
        );
        let got = &launches[0];
        if want_prefix[0] == "run" {
            assert_eq!(got, &want_prefix, "{extra:?}");
        } else {
            assert_eq!(&got[..2], &want_prefix[..], "{extra:?}");
            assert_eq!(got[2], root.display().to_string(), "{extra:?}");
            assert_eq!(&got[3..5], &["probe", "coyote"], "{extra:?}");
            assert_eq!(&got[5..], extra, "{extra:?}");
        }
    }
}

#[test]
fn usage_probe_blank_or_repeated_sandbox_name_and_empty_sandbox_equals_are_refused() {
    let cases: [&[&str]; 4] = [
        &["--sandbox", "--sandbox-name", "   ", "--info"],
        &["--sandbox", "--sandbox-name", "a", "--sandbox-name", "b"],
        &["--sandbox", "--sandbox-name=a", "--sandbox-name=a"],
        &["--sandbox=", "--info"],
    ];
    for args in cases {
        let (root, _g, log) = setup("blank-repeat");
        let out = run_coyote(&root, args);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{args:?} must fail:\n{stderr}");
        assert!(stderr.contains("--sandbox"), "{args:?}:\n{stderr}");
        assert!(calls(&log).is_empty(), "{args:?}: {:?}", calls(&log));
        assert!(text(&out.stdout).is_empty(), "{args:?}");
    }
}

#[test]
fn usage_probe_fresh_setup_output_stays_off_stdout() {
    let (root, _g, log) = setup("fresh-stdout");
    let out = run_coyote(
        &root,
        &["--sandbox", "--fresh", "--sandbox-name", "probe", "--info"],
    );
    assert_eq!(
        text(&out.stdout),
        format!("{INNER_STDOUT}\n"),
        "stderr:\n{}",
        text(&out.stderr)
    );
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&root, "probe", &["--info"])]
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn usage_probe_terminal_stdin_with_piped_stdout_execs_without_stdin() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("script(1) unavailable; skipping");
        return;
    }
    let (root, _g, log) = setup("tty-in-pipe-out");
    let stub = root.join("bin/sbx");
    let script = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a" >> '@ROOT@/stub-sbx-calls'; done
printf '%s\n' '<<END>>' >> '@ROOT@/stub-sbx-calls'
if [ "$1" = "version" ]; then echo 'sbx version v0.49.1 c5b1995'; exit 0; fi
if [ "$1" = "ls" ]; then printf 'NAME STATUS\nprobe running\n'; exit 0; fi
if [ "$1" = "exec" ] && [ "$2" = "-w" ]; then
  if [ -t 0 ]; then echo INNER-STDIN-TTY; else c=$(cat); echo "INNER-STDIN-BYTES:${#c}"; fi
  if [ -t 1 ]; then echo INNER-STDOUT-TTY; else echo INNER-STDOUT-PIPE; fi
  exit 0
fi
exit 0
"#
    .replace("@ROOT@", &root.display().to_string());
    fs::write(&stub, script).unwrap();

    let inner = format!(
        "'{}' --sandbox --sandbox-name probe --info | cat",
        env!("CARGO_BIN_EXE_coyote")
    );
    let mut command = Command::new("script");
    if cfg!(target_os = "linux") {
        command.args(["-qec", &format!("sh -c \"{inner}\""), "/dev/null"]);
    } else {
        command.args(["-q", "/dev/null", "sh", "-c", &inner]);
    }
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    let out = scrub_coyote_env(&mut command)
        .current_dir(&root)
        .env("PWD", &root)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", root.join("home"))
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_SANDBOX_KIT")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let transcript = text(&out.stdout);
    let launches: Vec<Vec<String>> = calls(&log)
        .into_iter()
        .filter(|c| c[0] == "run" || (c[0] == "exec" && c.len() > 3 && c[3] == "probe"))
        .collect();
    let mut want: Vec<String> = vec!["exec".into(), "-w".into(), root.display().to_string()];
    want.extend(["probe", "coyote", "--info"].map(String::from));
    assert_eq!(launches, vec![want], "{transcript}");
    assert!(
        transcript.contains("INNER-STDIN-BYTES:0"),
        "inner stdin was not null:\n{transcript}"
    );
    assert!(transcript.contains("INNER-STDOUT-PIPE"), "{transcript}");
}

#[test]
fn usage_probe_exec_workdir_ignores_a_stale_pwd() {
    let (root, _g, log) = setup("stale-pwd");
    let cwd = root.join("work/probe");
    let out = run_coyote_in(
        &root,
        &cwd,
        &["--sandbox", "--sandbox-name", "probe", "--info"],
        &[("PWD", root.join("work").to_str().unwrap())],
    );
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&cwd, "probe", &["--info"])],
        "stderr:\n{}",
        text(&out.stderr)
    );
}

#[test]
fn usage_probe_host_survives_sigint_and_sigquit_while_the_child_runs() {
    let (root, _g, _log) = setup("sigint");
    let stub = root.join("bin/sbx");
    let script = r#"#!/bin/sh
if [ "$1" = "version" ]; then echo 'sbx version v0.49.1 c5b1995'; exit 0; fi
if [ "$1" = "ls" ]; then printf 'NAME STATUS\nprobe running\n'; exit 0; fi
if [ "$1" = "exec" ] && [ "$2" = "-i" ]; then
  : > '@ROOT@/child-ready'
  i=0; while [ ! -e '@ROOT@/child-go' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done
  echo INNER-STDOUT; exit 5
fi
exit 0
"#
    .replace("@ROOT@", &root.display().to_string());
    fs::write(&stub, script).unwrap();
    let path = format!(
        "{}:{}",
        root.join("bin").display(),
        env::var("PATH").unwrap_or_default()
    );
    let child = scrub_coyote_env(&mut Command::new(env!("CARGO_BIN_EXE_coyote")))
        .args(["--sandbox", "--sandbox-name", "probe", "--headless", "hi"])
        .current_dir(&root)
        .env("PWD", &root)
        .env("COYOTE_CONFIG_DIR", root.join("config"))
        .env("COYOTE_CACHE_DIR", root.join("cache"))
        .env("HOME", root.join("home"))
        .env("PATH", path)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_SANDBOX_KIT")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready = root.join("child-ready");
    for _ in 0..200 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ready.exists(), "inner exec never started");
    let pid = child.id().to_string();
    for sig in ["INT", "QUIT"] {
        let st = Command::new("kill")
            .args([&format!("-{sig}"), &pid])
            .status()
            .unwrap();
        assert!(st.success());
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    fs::write(root.join("child-go"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(5),
        "host did not survive SIGINT/SIGQUIT: {:?}\n{}",
        out.status,
        text(&out.stderr)
    );
    assert_eq!(text(&out.stdout), format!("{INNER_STDOUT}\n"));
}

#[test]
fn usage_probe_debug_log_omits_forwarded_arg_contents() {
    let (root, _g, log) = setup("log");
    let log_file = root.join("coyote.log");
    let secret = "SUPERSECRET-PROMPT-TEXT-1234";
    let out = run_coyote_in(
        &root,
        &root,
        &["--sandbox", "--sandbox-name", "probe", "--headless", secret],
        &[
            ("COYOTE_LOG_LEVEL", "trace"),
            ("COYOTE_LOG_PATH", log_file.to_str().unwrap()),
        ],
    );
    assert_eq!(
        exec_calls(&log),
        vec![expected_exec(&root, "probe", &["--headless", secret])],
        "stderr:\n{}",
        text(&out.stderr)
    );
    let logged = fs::read_to_string(&log_file).unwrap_or_default();
    assert!(
        logged.contains("probe"),
        "debug log recorded no launch line at all:\n{logged}"
    );
    assert!(!logged.contains(secret), "forwarded arg leaked:\n{logged}");
    assert!(!text(&out.stderr).contains(secret), "{}", text(&out.stderr));
}
