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
const WINDOWS_PIPE: &str = r"\\.\pipe\coyote-prompt-316-7";
const PROC_PIPE: &str = "/proc/sys/Device/NamedPipe/coyote-prompt-316-7";

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
        "//./pipe/coyote-prompt-316-8",
        "//./pipe/coyote-prompt-316-8",
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
