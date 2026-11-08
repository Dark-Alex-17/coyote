//! Where the bash prompt helper reads its reply from, observed from outside:
//! on a unix host `COYOTE_PROMPT_REPLY` is opened as given, a Windows pipe
//! name included, while a Git Bash or Cygwin shell (`OSTYPE` says so before
//! the helper is sourced) opens the same pipe through its `/proc` view of the
//! pipe namespace. Every other spelling stays untouched on both, and the
//! unix FIFO round trip is the one the helper always had. Each case runs the
//! shipped helper in a real bash with no terminal; the read target shows up
//! in bash's own error line when the path does not exist, so no Windows is
//! needed to see which path was opened.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs, process, thread};

const HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.sh"
);
const FIXTURE_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bin");
const WINDOWS_PIPE: &str = r"\\.\pipe\coyote-prompt-test-7";
const PROC_PIPE: &str = "/proc/sys/Device/NamedPipe/coyote-prompt-test-7";

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch_dir(tag: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-prompt-utils-pipe-path-{tag}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    TempDirGuard(dir)
}

/// The helper sourced in a fresh bash with no terminal, `script` run after
/// it; `ostype` is assigned before the `source` line the way an msys or
/// cygwin bash would report itself. The fixture `tput` keeps terminfo out.
fn bash(ostype: Option<&str>, reply: &str, script: &str) -> Output {
    let prelude = ostype
        .map(|os| format!("OSTYPE={os}; "))
        .unwrap_or_default();
    let path = env::join_paths(
        std::iter::once(PathBuf::from(FIXTURE_BIN))
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    Command::new("bash")
        .arg("-c")
        .arg(format!("{prelude}source \"$PROMPT_UTILS\"; {script}"))
        .env("PROMPT_UTILS", HELPER)
        .env("LC_ALL", "C")
        .env("COYOTE_PROMPT_REPLY", reply)
        .env("PATH", path)
        .env("TERM", "xterm-256color")
        .env_remove("COYOTE_UI")
        .env_remove("_COYOTE_PROMPT_OUT")
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run bash")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `confirm` with the reply path pointing nowhere: the request line still
/// goes out, the read fails on `path` and the helper fails closed.
fn assert_reads_from(ostype: Option<&str>, reply: &str, path: &str) {
    let out = bash(
        ostype,
        reply,
        "v=\"$(confirm 'Go? pipe-path-x7x')\"; echo \"rc=$? v=[$v]\"",
    );
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert!(
        stdout.contains(r#""action":"confirm","question":"Go? pipe-path-x7x""#),
        "the request line must still be sent: {stdout:?} / {stderr:?}"
    );
    assert!(
        stdout.contains("rc=1 v=[]"),
        "a dead reply path fails closed: {stdout:?} / {stderr:?}"
    );
    assert!(
        stderr.contains(&format!("{path}: No such file or directory")),
        "the reply must be read from {path}: {stderr:?}"
    );
    assert!(
        stderr.contains("prompt reply channel closed"),
        "the helper names the failure: {stderr:?}"
    );
}

#[test]
fn a_unix_host_opens_the_reply_path_as_given_even_when_it_looks_like_a_windows_pipe() {
    assert_reads_from(None, WINDOWS_PIPE, WINDOWS_PIPE);
    assert_reads_from(Some("darwin24.0"), WINDOWS_PIPE, WINDOWS_PIPE);
    assert_reads_from(Some("linux-gnu"), WINDOWS_PIPE, WINDOWS_PIPE);
}

#[test]
fn git_bash_and_cygwin_read_a_windows_pipe_through_its_proc_path() {
    assert_reads_from(Some("msys"), WINDOWS_PIPE, PROC_PIPE);
    assert_reads_from(Some("cygwin"), WINDOWS_PIPE, PROC_PIPE);
}

#[test]
fn git_bash_leaves_a_path_that_is_not_a_windows_pipe_alone() {
    let dir = scratch_dir("untranslated");
    let missing = dir.0.join("no-such-fifo");
    let missing = missing.to_str().unwrap();
    assert_reads_from(Some("msys"), missing, missing);
    // The forward-slash spelling is not what Coyote emits and is not rewritten.
    assert_reads_from(
        Some("msys"),
        "//./pipe/coyote-prompt-test-8",
        "//./pipe/coyote-prompt-test-8",
    );
}

#[test]
fn the_translation_does_not_rewrite_the_exported_variable() {
    let out = bash(
        Some("msys"),
        WINDOWS_PIPE,
        "printf '%s' \"$COYOTE_PROMPT_REPLY\"",
    );
    assert_eq!(text(&out.stdout), WINDOWS_PIPE);
}

#[test]
fn a_channel_handed_over_on_fd_nine_is_absent_under_git_bash_too() {
    let out = Command::new("bash")
        .arg("-c")
        .arg("OSTYPE=msys; source \"$PROMPT_UTILS\"; v=\"$(confirm 'Go? fd9-x7x')\"; echo \"rc=$? v=[$v]\"")
        .env("PROMPT_UTILS", HELPER)
        .env("LC_ALL", "C")
        .env("COYOTE_PROMPT_REPLY", "/dev/fd/9")
        .env("COYOTE_UI", "tui")
        .env("TERM", "xterm-256color")
        .env_remove("_COYOTE_PROMPT_OUT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run bash");
    let stdout = text(&out.stdout);
    let stderr = text(&out.stderr);
    assert!(stdout.contains("rc=1 v=[]"), "{stdout:?} / {stderr:?}");
    assert!(
        stderr.contains("prompts are not available in the TUI without the reply channel"),
        "{stderr:?}"
    );
}

/// The unix reply channel the way the host hands it over: a pipe inherited
/// as a file descriptor, named through `/dev/fd/N`, whose write end the
/// host keeps open. The request leaves on the tool's real stdout, a reply
/// carrying the request id is written into the pipe (a foreign id first,
/// which the helper must skip), and the helper prints the mapped answer.
/// `OSTYPE=msys` with a `/dev/fd` path takes the very same route.
#[test]
fn the_unix_reply_pipe_round_trip_is_unchanged_on_every_ostype() {
    use std::io::{BufRead, BufReader, Write};
    for ostype in [None, Some("msys")] {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        let prelude = ostype
            .map(|os| format!("OSTYPE={os}; "))
            .unwrap_or_default();
        // The read end arrives on stdin and is moved to fd 3 before the
        // helper is sourced; stdin itself then points at the null device.
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "exec 3<&0 0</dev/null; export COYOTE_PROMPT_REPLY=/dev/fd/3; \
                 {prelude}source \"$PROMPT_UTILS\"; \
                 a=\"$(confirm 'First? pipe-x7x')\"; echo \"first=$? [$a]\" >&2; \
                 b=\"$(list 'Pick? pipe-x7x' alpha beta gamma)\"; echo \"second=$? [$b]\" >&2; \
                 c=\"$(checkbox 'Which? pipe-x7x' a b c)\"; echo \"third=$? [$c]\" >&2; \
                 d=\"$(input 'Name? pipe-x7x')\"; echo \"fourth=$? [$d]\" >&2; \
                 e=\"$(confirm 'Cancel? pipe-x7x')\"; echo \"fifth=$? [$e]\" >&2"
            ))
            .env("PROMPT_UTILS", HELPER)
            .env("TERM", "xterm-256color")
            .env_remove("COYOTE_PROMPT_REPLY")
            .env_remove("COYOTE_UI")
            .env_remove("_COYOTE_PROMPT_OUT")
            .stdin(Stdio::from(reader))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bash");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let drain = thread::spawn(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            let mut stderr = stderr;
            stderr.read_to_end(&mut buf).unwrap();
            buf
        });
        let answers = [
            r#""answer":true"#,
            r#""answer":"gamma""#,
            r#""answer":["c","a"]"#,
            r#""answer":"Ada Lovelace""#,
            r#""answer":null"#,
        ];
        let mut requests = Vec::new();
        let mut lines = BufReader::new(stdout).lines();
        for answer in answers {
            let line = lines
                .next()
                .expect("a request line per helper call")
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = value["coyote_prompt"]["id"].as_str().unwrap().to_owned();
            requests.push(value);
            writer
                .write_all(b"{\"id\":\"p0-deadbeef\",\"answer\":false}\n")
                .unwrap();
            writer
                .write_all(format!("{{\"id\":\"{id}\",{answer}}}\n").as_bytes())
                .unwrap();
        }
        assert!(lines.next().is_none(), "no extra request lines");
        drop(writer);
        let status = child.wait().unwrap();
        let stderr = text(&drain.join().unwrap());
        assert!(status.success(), "ostype={ostype:?}: {stderr:?}");
        let reports: Vec<&str> = stderr.lines().collect();
        assert_eq!(
            reports,
            [
                "first=0 [1]",
                "second=0 [2]",
                "third=0 [0 2]",
                "fourth=0 [Ada Lovelace]",
                "fifth=1 []",
            ],
            "ostype={ostype:?}"
        );
        let actions: Vec<&str> = requests
            .iter()
            .map(|r| r["coyote_prompt"]["action"].as_str().unwrap())
            .collect();
        assert_eq!(
            actions,
            ["confirm", "select", "checkbox", "input", "confirm"],
            "{requests:?}"
        );
        assert_eq!(
            requests[1]["coyote_prompt"]["options"],
            serde_json::json!(["alpha", "beta", "gamma"])
        );
        assert_eq!(
            requests[0]["coyote_prompt"]["options"],
            serde_json::json!([])
        );
        for request in &requests {
            let id = request["coyote_prompt"]["id"].as_str().unwrap();
            assert!(
                id.starts_with('p') && id.contains('-'),
                "ids are p<pid>-<entropy>: {id}"
            );
        }
    }
}

/// The pipe-prefix match the way a tool would hit it
/// from other shell flavours and locales. A tool script may run under
/// `bash --posix`, the system `/bin/bash` or `sh` (bash in posix mode on
/// macOS); the helper's promise is the same everywhere: a value that starts
/// with the literal `\\.\pipe\` is opened through `/proc/sys/Device/NamedPipe`
/// under Git Bash or Cygwin and every other spelling is opened as given, with
/// the backslashes of the prefix taken literally rather than as glob escapes.
mod other_shells {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    const REAL_PIPE: &str = r"\\.\pipe\coyote-prompt-test-9";
    const REAL_PROC: &str = "/proc/sys/Device/NamedPipe/coyote-prompt-test-9";

    /// Spellings that only match the prefix when its backslashes are read as
    /// glob escapes (or that carry the prefix somewhere other than the
    /// start). None of them is a Windows pipe name and none may be rewritten.
    const NOT_A_PIPE: &[&str] = &[
        r"\.pipecoyote-prompt-test-9",
        r"\\.\pipe",
        r".pipe\coyote-prompt-test-9",
        r"x\\.\pipe\coyote-prompt-test-9",
        "//./pipe/coyote-prompt-test-9",
    ];

    /// The shell flavours present on this host that can source the helper
    /// (`[[` rules dash out: `sh` is skipped where it is dash).
    fn shell_flavours() -> Vec<(&'static str, Vec<&'static str>)> {
        let mut flavours: Vec<(&str, Vec<&str>)> =
            vec![("bash", vec![]), ("bash", vec!["--posix"])];
        for candidate in ["/bin/bash", "sh"] {
            let speaks_bash = Command::new(candidate)
                .arg("-c")
                .arg("[[ a == a ]] && [ -n \"$BASH_VERSION\" ]")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if speaks_bash {
                flavours.push((candidate, vec![]));
                if candidate == "/bin/bash" {
                    flavours.push((candidate, vec!["--posix"]));
                }
            }
        }
        flavours
    }

    fn fixture_path() -> std::ffi::OsString {
        env::join_paths(
            std::iter::once(PathBuf::from(FIXTURE_BIN))
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap()
    }

    /// Runs `script` after the helper is sourced (with `.`, which every
    /// flavour has) in the given shell with `OSTYPE` set first.
    fn shell(program: &str, args: &[&str], ostype: &str, reply: &str, script: &str) -> Output {
        Command::new(program)
            .args(args)
            .arg("-c")
            .arg(format!("OSTYPE={ostype}; . \"$PROMPT_UTILS\"; {script}"))
            .env("PROMPT_UTILS", HELPER)
            .env("LC_ALL", "C")
            .env("COYOTE_PROMPT_REPLY", reply)
            .env("PATH", fixture_path())
            .env("TERM", "xterm-256color")
            .env_remove("COYOTE_UI")
            .env_remove("_COYOTE_PROMPT_OUT")
            .env_remove("AUTO_CONFIRM")
            .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run shell")
    }

    /// The path the helper tried to open, read off the shell's own
    /// redirection error (`<file>: line N: <path>: No such file or directory`).
    fn opened_path(out: &Output) -> String {
        let stderr = text(&out.stderr);
        let line = stderr
            .lines()
            .find(|l| l.ends_with(": No such file or directory"))
            .unwrap_or_else(|| panic!("no redirection error in {stderr:?}"));
        let body = line.trim_end_matches(": No such file or directory");
        let (_, path) = body
            .split_once(": line ")
            .map(|(_, rest)| rest.split_once(": ").unwrap())
            .unwrap_or_else(|| panic!("unexpected error shape: {line:?}"));
        path.to_owned()
    }

    #[test]
    fn the_prefix_is_matched_literally_in_every_shell_flavour() {
        for (program, args) in shell_flavours() {
            for ostype in ["msys", "cygwin", "darwin24.0", "linux-gnu", ""] {
                let translating = ostype.starts_with("msys") || ostype.starts_with("cygwin");
                let script = "v=\"$(confirm 'Go? sh-x7x')\"; echo \"rc=$? v=[$v]\"";
                let out = shell(program, &args, ostype, REAL_PIPE, script);
                let expected = if translating { REAL_PROC } else { REAL_PIPE };
                assert_eq!(
                    opened_path(&out),
                    expected,
                    "{program} {args:?} OSTYPE={ostype:?}: {:?}",
                    text(&out.stderr)
                );
                assert!(
                    text(&out.stdout).contains("rc=1 v=[]"),
                    "{program} {args:?}: {:?}",
                    text(&out.stdout)
                );
                for spelling in NOT_A_PIPE {
                    let out = shell(program, &args, ostype, spelling, script);
                    assert_eq!(
                        opened_path(&out),
                        *spelling,
                        "{program} {args:?} OSTYPE={ostype:?} must not rewrite {spelling:?}"
                    );
                    assert!(
                        text(&out.stdout).contains("rc=1 v=[]"),
                        "{program} {args:?}: {:?}",
                        text(&out.stdout)
                    );
                    assert_eq!(
                        text(&out.stderr)
                            .matches("prompt reply channel closed")
                            .count(),
                        1,
                        "{program} {args:?}: {:?}",
                        text(&out.stderr)
                    );
                }
            }
        }
    }

    /// Tool scripts commonly run with `set -u` and `pipefail`; sourcing the
    /// helper under them succeeds in every flavour and still translates.
    /// (`set -e` is left out only because the fixture `tput` exits 1 on
    /// colour queries, which the helper's colour setup would trip on.)
    #[test]
    fn sourcing_under_strict_shell_options_still_translates() {
        for (program, args) in shell_flavours() {
            for reply in ["", REAL_PIPE, "/no/such/fifo-test-9"] {
                let out = Command::new(program)
                    .args(&args)
                    .arg("-c")
                    .arg("set -u -o pipefail; OSTYPE=msys; . \"$PROMPT_UTILS\"; echo sourced-ok")
                    .env("PROMPT_UTILS", HELPER)
                    .env("LC_ALL", "C")
                    .env("COYOTE_PROMPT_REPLY", reply)
                    .env("PATH", fixture_path())
                    .env("TERM", "xterm-256color")
                    .env_remove("COYOTE_UI")
                    .env_remove("_COYOTE_PROMPT_OUT")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .expect("run shell");
                assert!(
                    out.status.success() && text(&out.stdout).trim() == "sourced-ok",
                    "{program} {args:?} reply={reply:?}: {:?} / {:?}",
                    text(&out.stdout),
                    text(&out.stderr)
                );
                assert!(
                    out.stderr.is_empty(),
                    "{program} {args:?} reply={reply:?}: {:?}",
                    text(&out.stderr)
                );
            }
            let out = shell(
                program,
                &args,
                "msys",
                REAL_PIPE,
                ". \"$PROMPT_UTILS\"; v=\"$(confirm 'Twice? sh-x7x')\"; echo \"rc=$? v=[$v]\"",
            );
            assert_eq!(
                opened_path(&out),
                REAL_PROC,
                "{program} {args:?}: sourcing twice keeps the translation"
            );
        }
    }

    /// Sourcing the helper changes the tool's environment by exactly the
    /// one variable it documents (`_COYOTE_PROMPT_OUT`); `COYOTE_PROMPT_REPLY`
    /// keeps the Windows spelling and nothing the translation uses leaks.
    #[test]
    fn sourcing_exports_only_the_documented_variable() {
        let dir = scratch_dir("env-delta");
        fs::create_dir_all(&dir.0).unwrap();
        let out = Command::new("bash")
            .arg("-c")
            .arg(
                "env | LC_ALL=C sort > \"$1\"; OSTYPE=msys; . \"$PROMPT_UTILS\"; \
                 env | LC_ALL=C sort | comm -13 \"$1\" -",
            )
            .arg("helper")
            .arg(dir.0.join("before"))
            .env("PROMPT_UTILS", HELPER)
            .env("LC_ALL", "C")
            .env("COYOTE_PROMPT_REPLY", REAL_PIPE)
            .env("PATH", fixture_path())
            .env("TERM", "xterm-256color")
            .env_remove("COYOTE_UI")
            .env_remove("_COYOTE_PROMPT_OUT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run bash");
        let added: Vec<String> = text(&out.stdout)
            .lines()
            .filter(|l| {
                !l.starts_with("OSTYPE=") && !l.starts_with("_=") && !l.starts_with("SHLVL=")
            })
            .map(str::to_owned)
            .collect();
        assert_eq!(added, ["_COYOTE_PROMPT_OUT=9"], "{:?}", text(&out.stderr));
    }

    /// Non-ASCII question text and answers cross the request and reply
    /// streams byte for byte whatever locale the tool runs under.
    #[test]
    fn non_ascii_prompts_round_trip_under_every_locale() {
        let question = "Go? caf\u{e9} \u{2713} \u{1f680}".to_owned();
        let answer = "Ada caf\u{e9} \u{2713} \u{1f680}".to_owned();
        for (ostype, locale) in [
            ("darwin24.0", Some("C")),
            ("linux-gnu", Some("POSIX")),
            ("msys", Some("en_US.UTF-8")),
            ("cygwin", None),
            ("msys", Some("C")),
        ] {
            let (reader, mut writer) = std::io::pipe().expect("pipe");
            let mut cmd = Command::new("bash");
            cmd.arg("-c")
                .arg(format!(
                    "exec 3<&0 0</dev/null; export COYOTE_PROMPT_REPLY=/dev/fd/3; \
                     OSTYPE={ostype}; . \"$PROMPT_UTILS\"; \
                     a=\"$(confirm \"$1\")\"; echo \"first=$? [$a]\" >&2; \
                     b=\"$(input \"$1\")\"; echo \"second=$? [$b]\" >&2; \
                     c=\"$(list \"$1\" alpha \"$2\" gamma)\"; echo \"third=$? [$c]\" >&2"
                ))
                .arg("helper")
                .arg(&question)
                .arg(&answer)
                .env("PROMPT_UTILS", HELPER)
                .env("PATH", fixture_path())
                .env("TERM", "xterm-256color")
                .env_remove("COYOTE_PROMPT_REPLY")
                .env_remove("COYOTE_UI")
                .env_remove("_COYOTE_PROMPT_OUT")
                .env_remove("LANG")
                .env_remove("LC_CTYPE")
                .stdin(Stdio::from(reader))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match locale {
                Some(lc) => {
                    cmd.env("LC_ALL", lc);
                }
                None => {
                    cmd.env_remove("LC_ALL");
                }
            }
            let mut child = cmd.spawn().expect("spawn bash");
            let stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let drain = thread::spawn(move || {
                let mut buf = Vec::new();
                stderr.read_to_end(&mut buf).unwrap();
                buf
            });
            let answers = [
                r#""answer":true"#.to_owned(),
                format!(r#""answer":{}"#, serde_json::Value::String(answer.clone())),
                format!(r#""answer":{}"#, serde_json::Value::String(answer.clone())),
            ];
            let mut requests = Vec::new();
            let mut lines = BufReader::new(stdout).split(b'\n');
            for reply in &answers {
                let raw = lines.next().expect("a request line").unwrap();
                let line = String::from_utf8(raw).expect("request line is UTF-8");
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                let id = value["coyote_prompt"]["id"].as_str().unwrap().to_owned();
                requests.push(value);
                writer
                    .write_all(format!("{{\"id\":\"{id}\",{reply}}}\n").as_bytes())
                    .unwrap();
            }
            drop(writer);
            let status = child.wait().unwrap();
            let stderr = drain.join().unwrap();
            assert!(status.success(), "{ostype} {locale:?}: {:?}", text(&stderr));
            let expected = format!("first=0 [1]\nsecond=0 [{answer}]\nthird=0 [1]\n");
            assert_eq!(
                stderr,
                expected.as_bytes(),
                "{ostype} {locale:?}: {:?}",
                text(&stderr)
            );
            for request in &requests {
                assert_eq!(
                    request["coyote_prompt"]["question"].as_str().unwrap(),
                    question,
                    "{ostype} {locale:?}"
                );
            }
            assert_eq!(
                requests[2]["coyote_prompt"]["options"],
                serde_json::json!(["alpha", answer, "gamma"]),
                "{ostype} {locale:?}"
            );
        }
    }
}
