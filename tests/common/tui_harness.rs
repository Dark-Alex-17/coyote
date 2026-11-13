//! The pty harness the full-screen UI suites share: temp config dirs,
//! the dry-run and scripted-model configs, the bash wrappers that run the
//! binary and report its exit code and the tty flags afterwards, and the
//! scanners that read the raw escape stream back. Unix-only, as the pty
//! it drives is.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use super::fake_llm::{FakeLlm, Reply};
use super::pty::{Pty, detached, strip_ansi};

/// Covers the keyboard-enhancement probe, which waits two seconds for an
/// answer a pseudo-terminal never gives.
pub const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Hotkeys on a modal count only once both the last keystroke and the
/// modal's appearance are at least HOTKEY_IDLE (1 s) old; 1200 ms settles
/// both.
pub const HOTKEY_GAP: Duration = Duration::from_millis(1200);

pub struct TempDirGuard(pub PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn fresh_config_dir(scenario: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-tui-binary-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// A dry-run config that never asks to save a session, so every exit path
/// is prompt-free. Paths land in single-quoted YAML scalars.
pub fn write_dry_run_config(dir: &Path) {
    write_config(dir, "save_session: false\n");
}

/// The same config with `extra` top-level YAML appended by the caller;
/// with `save_session` left unset a dirty session asks "Save session?"
/// on `.exit session`.
pub fn write_config(dir: &Path, extra: &str) {
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

/// Runs the binary in the background of a bash wrapper on a fresh pty with
/// the tty on stdin. The wrapper names the child's pid, waits for it, and
/// then reports its exit code and the tty flags so a test can check both
/// after any exit path.
pub fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    spawn_tui_under(dir, extra_env, WAIT_WRAPPER, "/dev/tty")
}

/// [`spawn_tui`] with command-line flags after the binary; `args` are
/// plain ASCII flags, so they go into the wrapper unquoted.
pub fn spawn_tui_with_args(dir: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> Pty {
    let args = format!(" {}", args.join(" "));
    spawn_binary_under(
        built_binary(),
        dir,
        extra_env,
        WAIT_WRAPPER,
        "/dev/tty",
        &args,
    )
}

/// Report line emitted by both wrappers once the binary has exited. The
/// alternate screen leaves no newline behind once escapes are stripped,
/// so the report starts a fresh line first.
pub const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";

/// The plain wrapper: one process group, `wait` for the child. Being a
/// `setsid` leader whose parent sits in another session makes that group
/// orphaned, and the kernel discards SIGTSTP for orphaned groups, so this
/// wrapper cannot observe a suspend.
pub const WAIT_WRAPPER: &str =
    "'{coyote}'{args} <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

/// [`WAIT_WRAPPER`] with the binary's stdout sent to `path` while stdin
/// keeps the `{stdin}` slot: the shape of `coyote --install x > log`
/// typed at a terminal. The wrapper's own report still reaches the pty.
pub fn wait_wrapper_with_stdout_to(path: &Path) -> String {
    WAIT_WRAPPER.replace("<{stdin}", &format!("<{{stdin}} >'{}'", path.display()))
}

/// A job-control wrapper that behaves like the user's shell: the binary
/// runs as the foreground job in its own process group; when it stops the
/// wrapper regains the terminal, says so, reports the tty flags it sees,
/// and after a moment resumes the job with `fg` as a user would. (`fg`
/// must keep its own stdout: redirecting it stops bash handing the tty
/// over.)
pub const JOB_CONTROL_WRAPPER: &str = "set -m; '{coyote}'{args} <{stdin}; rc=$?; \
     while [ -n \"$(jobs -s)\" ]; do echo; echo job-stopped; \
     echo \"stopped-tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo stopped-probe-done; \
     sleep 1; fg %1; rc=$?; done; {report}";

/// The build's own binary; every spawn goes through [`spawn_binary_under`]
/// (on a pty) or [`ttyless_command`] (with no terminal at all).
pub fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

/// [`spawn_binary_under`] for the build's own binary: `wrapper` is one of
/// the bash scripts above and `stdin` the path the binary's stdin is
/// redirected from.
pub fn spawn_tui_under(dir: &Path, extra_env: &[(&str, &str)], wrapper: &str, stdin: &str) -> Pty {
    spawn_binary_under(built_binary(), dir, extra_env, wrapper, stdin, "")
}

/// [`spawn_tui_under`] for a copy of the binary living elsewhere (the
/// self-update path classifies the install by where the executable is);
/// `args` fills the wrapper's `{args}` slot right after the binary.
pub fn spawn_binary_under(
    binary: &Path,
    dir: &Path,
    extra_env: &[(&str, &str)],
    wrapper: &str,
    stdin: &str,
    args: &str,
) -> Pty {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(
        wrapper
            .replace("{coyote}", binary.to_str().unwrap())
            .replace("{args}", args)
            .replace("{stdin}", stdin)
            .replace("{report}", TTY_REPORT),
    );
    base_env(&mut cmd, dir);
    assert_env_stays_under_dir(extra_env);
    cmd.env("COYOTE_UI", "tui").envs(extra_env.iter().copied());
    Pty::spawn(cmd).unwrap()
}

/// `extra_env` may add to the shared environment but never move the
/// binary out of the test's own dir: a test that overrode one of these
/// would run against the developer's real config, cache, sessions or
/// home.
fn assert_env_stays_under_dir(extra_env: &[(&str, &str)]) {
    assert!(
        !extra_env.iter().any(|(k, _)| matches!(
            *k,
            "COYOTE_CONFIG_DIR" | "COYOTE_CACHE_DIR" | "COYOTE_SESSIONS_DIR" | "HOME"
        )),
        "extra_env must not override COYOTE_CONFIG_DIR, COYOTE_CACHE_DIR, COYOTE_SESSIONS_DIR \
         or HOME: {extra_env:?}"
    );
}

/// The environment every spawn of the binary shares: working dir, config
/// and cache dirs and `HOME` all under `dir`, the fixture bin first on
/// `PATH`, and no inherited `COYOTE_*` or color variables.
pub fn base_env(cmd: &mut Command, dir: &Path) {
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
        .env("TERM", "xterm-256color")
        .env("PATH", path)
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR");
}

/// The binary at `binary` set up to run with no terminal at all: the
/// shared environment under `dir`, detached from the test runner's
/// controlling tty, the plain REPL selected and stdin at /dev/null. The
/// caller adds its arguments and chooses what to do with stdout and
/// stderr.
pub fn ttyless_command(binary: &Path, dir: &Path) -> Command {
    let mut cmd = Command::new(binary);
    base_env(&mut cmd, dir);
    detached(&mut cmd)
        .env("COYOTE_UI", "repl")
        .stdin(Stdio::null());
    cmd
}

/// Runs the binary with no terminal at all and its output discarded,
/// returning how it exited: for a run whose only job is its side effect
/// (seeding the built-in assets before a file walk).
pub fn run_ttyless_status(binary: &Path, dir: &Path, args: &[&str]) -> ExitStatus {
    ttyless_command(binary, dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
}

/// [`run_ttyless_binary`] returning the exit status itself, for a check
/// that needs the exact exit code (a clap usage error is 2) rather than
/// success or failure.
pub fn run_ttyless_output(
    binary: &Path,
    dir: &Path,
    args: &[&str],
    what: &str,
) -> (ExitStatus, String) {
    run_ttyless_until_exit(binary, dir, args, &[], what)
}

/// Runs the binary with no terminal at all (detached, stdin /dev/null,
/// pipes for stdout/stderr) and returns `(success, stdout+stderr)`,
/// failing the test if it has not exited by `PTY_DEADLINE`.
pub fn run_ttyless(dir: &Path, args: &[&str], what: &str) -> (bool, String) {
    run_ttyless_binary(built_binary(), dir, args, what)
}

/// [`run_ttyless`] for a copy of the binary living elsewhere (the
/// self-update path classifies the install by where the executable is).
pub fn run_ttyless_binary(binary: &Path, dir: &Path, args: &[&str], what: &str) -> (bool, String) {
    run_ttyless_with_env(binary, dir, args, &[], what)
}

/// [`run_ttyless_binary`] with `extra_env` added to the shared
/// environment (a log path, a level) for a test that reads back what a
/// silent run recorded.
pub fn run_ttyless_with_env(
    binary: &Path,
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
    what: &str,
) -> (bool, String) {
    let (status, out) = run_ttyless_until_exit(binary, dir, args, extra_env, what);
    (status.success(), out)
}

/// Both pipes are drained on their own threads while the child runs, so
/// a chatty child cannot block on a full pipe and be mistaken for a hang;
/// a child that is still running at the deadline is killed, reaped and
/// reported with whatever it had written by then. Either way anything
/// the child spawned is killed with it, so a detached grandchild cannot
/// hold the pipes open and stall the readers; the group is killed before
/// the child is reaped, while its pid still names that group.
fn run_ttyless_until_exit(
    binary: &Path,
    dir: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
    what: &str,
) -> (ExitStatus, String) {
    assert_env_stays_under_dir(extra_env);
    let mut cmd = ttyless_command(binary, dir);
    cmd.args(args)
        .envs(extra_env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        if exited_unreaped(child.id() as libc::pid_t) {
            break;
        }
        if std::time::Instant::now() > deadline {
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{what}: `coyote {}` with no terminal did not exit within {PTY_DEADLINE:?}; \
                 captured so far:\n{}",
                args.join(" "),
                both_streams_within(stdout, stderr, Duration::from_secs(2))
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    kill_process_group(child.id());
    let status = child.wait().unwrap();
    (status, both_streams_within(stdout, stderr, PTY_DEADLINE))
}

/// Whether our child `pid` has exited and is still waiting to be reaped.
/// A pid that is no child of ours any more counts as exited; any other
/// failure of `waitid` is a harness bug and fails the test outright.
fn exited_unreaped(pid: libc::pid_t) -> bool {
    // SAFETY: all-zero bytes are a valid `siginfo_t`, and `waitid` only
    // writes into it; with WNOHANG a pid that has not exited leaves
    // `si_pid` at zero.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        // SAFETY: `info` is a live, writable `siginfo_t`.
        if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, options) } == 0 {
            // SAFETY: `si_pid` is valid for a child-state report.
            return unsafe { info.si_pid() } != 0;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::ECHILD) => return true,
            _ => panic!("waitid({pid}) failed: {err}"),
        }
    }
}

/// SIGKILLs the process group whose id is `pid`, the one the child leads
/// after `detached` made it a session leader: the child itself if still
/// running and every grandchild still in the group. A group that is
/// already gone is not an error worth reporting.
fn kill_process_group(pid: u32) {
    // SAFETY: kill takes no pointers; a stale pid is reported as an
    // error, not UB.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

fn drain(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

/// Both captured streams: waits up to `patience` for the readers to see
/// end-of-file and reports a stream that is still open as such, so
/// neither the result nor the deadline panic can hang on a pipe an
/// orphan still holds.
fn both_streams_within(
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
    patience: Duration,
) -> String {
    let until = std::time::Instant::now() + patience;
    while !(stdout.is_finished() && stderr.is_finished()) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    let finished = |reader: std::thread::JoinHandle<Vec<u8>>| {
        if reader.is_finished() {
            String::from_utf8_lossy(&reader.join().unwrap()).into_owned()
        } else {
            format!("<still open {patience:?} after the kill>")
        }
    };
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        finished(stdout),
        finished(stderr)
    )
}

pub fn tty_flag_tokens(transcript: &str) -> Vec<&str> {
    let flags = transcript
        .lines()
        .find_map(|line| line.strip_prefix("tty-flags: "))
        .unwrap_or_else(|| panic!("no tty-flags line in the transcript:\n{transcript}"));
    flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect()
}

/// Raw mode clears echo, icanon, isig and opost; the UI must have put all
/// four back.
pub fn assert_tty_restored(transcript: &str) {
    let tokens = tty_flag_tokens(transcript);
    // Raw mode clears all four; a restored line discipline has them back.
    for flag in ["echo", "icanon", "isig", "opost"] {
        assert!(
            tokens.contains(&flag),
            "stty did not report {flag} after the UI left:\n{transcript}"
        );
        let off = format!("-{flag}");
        assert!(
            !tokens.contains(&off.as_str()),
            "the terminal was left with {flag} off:\n{transcript}"
        );
    }
}

pub fn exit_code(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("exit="))
        .unwrap_or_else(|| panic!("no exit= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

pub fn child_pid(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("cpid="))
        .unwrap_or_else(|| panic!("no cpid= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

/// Types `text` into a field modal and sends it. Enter on a field modal
/// only counts once the modal has been up for a second; the gate runs
/// from its appearance, not from typing.
pub fn answer_field_modal(pty: &mut Pty, text: &str) {
    pty.read_for(HOTKEY_GAP);
    pty.send(format!("{text}\r").as_bytes()).unwrap();
}

pub const ENTER_ALT: &[u8] = b"\x1b[?1049h";
pub const LEAVE_ALT: &[u8] = b"\x1b[?1049l";

pub fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

pub fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

pub fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// The 1-based (row, col) of the cursor-position sequence (`CSI r;c H`)
/// that placed each occurrence of `needle` from `from` on: the painter
/// positions the cursor before the first changed cell of a row, so this
/// is the screen row a piece of text was drawn on.
pub fn painted_at(haystack: &[u8], needle: &[u8], from: usize) -> Vec<(u16, u16)> {
    let mut positions = Vec::new();
    let mut at = from;
    while let Some(hit) = find(haystack, needle, at) {
        let mut end = hit;
        while let Some(start) = rfind(&haystack[..end], b"\x1b[") {
            let params: Vec<u8> = haystack[start + 2..hit]
                .iter()
                .copied()
                .take_while(|b| b.is_ascii_digit() || *b == b';')
                .collect();
            if haystack.get(start + 2 + params.len()) == Some(&b'H')
                && let Some((row, col)) = std::str::from_utf8(&params)
                    .ok()
                    .and_then(|p| p.split_once(';'))
                && let (Ok(row), Ok(col)) = (row.parse(), col.parse())
            {
                positions.push((row, col));
                break;
            }
            end = start;
        }
        at = hit + 1;
    }
    positions
}

/// Every SGR parameter list (`ESC [ ... m`) in the raw output, split into
/// its numeric parameters.
pub fn sgr_params(raw: &[u8]) -> Vec<Vec<u16>> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    let mut rest: &str = &text;
    while let Some(start) = rest.find("\x1b[") {
        rest = &rest[start + 2..];
        let end = rest
            .find(|c: char| !(('\x30'..='\x3f').contains(&c) || ('\x20'..='\x2f').contains(&c)));
        let Some(end) = end else { break };
        if rest.as_bytes()[end] == b'm' {
            let params = &rest[..end];
            if params.is_empty() {
                out.push(vec![0]);
            } else {
                out.push(
                    params
                        .split([';', ':'])
                        .map(|p| p.parse::<u16>().unwrap_or(u16::MAX))
                        .collect(),
                );
            }
        }
        let final_byte = rest[end..].chars().next().map_or(1, char::len_utf8);
        rest = &rest[end + final_byte..];
    }
    out
}

/// Whether some occurrence of `needle` at or after `from` is painted
/// under the DIM attribute: the SGR sequences within the 64 bytes before
/// it, read in order, leave DIM on (a `2` parameter standing on its own,
/// with no `22` or `0` after it). The arguments of an extended colour are
/// stepped over, so the `2` of a `38;2;r;g;b` truecolour is not mistaken
/// for the attribute.
pub fn painted_dim(raw: &[u8], from: usize, needle: &[u8]) -> bool {
    let mut at = from;
    while let Some(found) = find(raw, needle, at) {
        let start = found.saturating_sub(64);
        let dim = sgr_params(&raw[start..found])
            .iter()
            .fold(false, |dim, params| dim_after(params).unwrap_or(dim));
        if dim {
            return true;
        }
        at = found + needle.len();
    }
    false
}

/// What one SGR parameter list does to DIM: `Some(true)` when it switches
/// it on, `Some(false)` when it resets it (`22`, or `0` for everything),
/// `None` when it leaves it alone; the last word in the list wins.
pub fn dim_after(params: &[u16]) -> Option<bool> {
    let mut dim = None;
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            38 | 48 | 58 => {
                i += match params.get(i + 1) {
                    Some(5) => 3,
                    Some(2) => 5,
                    _ => 1,
                };
                continue;
            }
            2 => dim = Some(true),
            0 | 22 => dim = Some(false),
            _ => {}
        }
        i += 1;
    }
    dim
}

/// Whether an SGR parameter list switches conceal on: an `8` standing on
/// its own. The arguments of an extended colour are stepped over, so a
/// palette index or channel value of 8 (`38;5;8`, `48;2;8;8;8`) is not
/// mistaken for the attribute.
pub fn is_conceal_sgr(params: &[u16]) -> bool {
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            38 | 48 | 58 => {
                i += match params.get(i + 1) {
                    Some(5) => 3,
                    Some(2) => 5,
                    _ => 1,
                };
            }
            8 => return true,
            _ => i += 1,
        }
    }
    false
}

pub fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// Dirties a temp session the way the save-modal cases do: a dry-run turn
/// records nothing, so a session setting is what flips the dirty bit.
pub fn open_and_dirty_a_session(pty: &mut Pty) {
    pty.send(b".session\r").unwrap();
    pty.wait_for("temp", PTY_DEADLINE).unwrap();
    pty.send(b".set skills_enabled false\r").unwrap();
    pty.read_for(Duration::from_millis(800));
}

/// The dry-run config with its only client pointed at `fake` instead, so a
/// turn can carry a tool call. `save_session: false` keeps every exit
/// prompt-free, as in [`write_dry_run_config`].
pub fn write_fake_llm_config(dir: &Path, fake: &FakeLlm) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             save_session: false\n\
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
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
            api_base = fake.api_base(),
        ),
    )
    .unwrap();
}

/// Drops one bash tool into the config's `functions/tools` dir; the binary
/// builds its shim on startup. No `set -e`: the fixture `tput` on PATH
/// exits 1 for everything but `cols`/`lines`, and the prompt helper's
/// colour assignments would abort the tool under it.
pub fn write_tool(dir: &Path, file: &str, body: &str) {
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(file), body).unwrap();
}

/// Pumps the pty until `path` exists, so the child's output never backs up
/// while the test waits on a side effect.
pub fn wait_for_file(pty: &mut Pty, path: &Path, what: &str) {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    while !path.exists() {
        let text = pty.read_for(Duration::from_millis(100));
        assert!(
            std::time::Instant::now() < deadline,
            "{what} ({}) never appeared:\n{text}",
            path.display()
        );
    }
}

/// Waits until `marker` is on screen with its surrounding whitespace
/// squeezed out (a rendered markdown cell may wrap it), returning the
/// transcript.
pub fn wait_for_squeezed(pty: &mut Pty, marker: &str, what: &str) -> String {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let wanted: String = marker.chars().filter(|c| !c.is_whitespace()).collect();
    loop {
        let text = pty.read_for(Duration::from_millis(200));
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if squeezed.contains(&wanted) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never appeared:\n{text}"
        );
    }
}

/// A confirm tool that also records the answer it got at `out`, so a
/// test can see the value the modal produced rather than only the
/// model's follow-up.
pub const RECORDING_CONFIRM_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user before continuing and record the answer.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out v\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   v=\"$(confirm \"Really do it? q9d-q\")\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture confirm: ${v:-cancelled} q9d-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

pub fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Files under `dir` (outside the cache) that were not there before and
/// carry `name` in their path.
pub fn new_files_named(dir: &Path, before: &[PathBuf], name: &str) -> Vec<PathBuf> {
    walk(dir)
        .into_iter()
        .filter(|path| !before.contains(path))
        .filter(|path| !path.to_string_lossy().contains("/cache/"))
        .filter(|path| path.to_string_lossy().contains(name))
        .collect()
}

/// The dry-run config with two embedding models on the dry-run client, so
/// creating a RAG has something to ask about (which model, which
/// documents) instead of failing on "No available embedding model".
pub fn write_dry_run_config_with_embeddings(dir: &Path) {
    write_config(
        dir,
        "\x20     - name: dry-embed-a\n\
         \x20       type: embedding\n\
         \x20       max_input_tokens: 2048\n\
         \x20       default_chunk_size: 1000\n\
         \x20       max_batch_size: 100\n\
         \x20     - name: dry-embed-b\n\
         \x20       type: embedding\n\
         \x20       max_input_tokens: 2048\n\
         \x20       default_chunk_size: 1000\n\
         \x20       max_batch_size: 100\n\
         save_session: false\n",
    );
}

/// A copy of the binary under `<home>/.cargo/bin`, where the self-update
/// path classifies it as a Cargo install and asks `Update anyway?` before
/// it goes anywhere near the network.
pub fn cargo_installed_binary(dir: &Path) -> PathBuf {
    let bin_dir = dir.join("home").join(".cargo").join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let copy = bin_dir.join("coyote");
    // A copy, never a hard link: the metadata checked afterwards must be
    // this file's own, not shared with the binary cargo may rebuild.
    fs::copy(built_binary(), &copy).unwrap();
    copy
}

/// Waits for the modal `command` raises after `before`: every marker and
/// the modal's `Esc = cancel` legend are on screen and the terminal was
/// never released. Markers and screen are compared with all whitespace
/// removed, so a marker may be written as natural text even though the
/// modal wraps it.
pub fn wait_for_the_modal(
    pty: &mut Pty,
    before: usize,
    command: &str,
    modal_markers: &[&str],
) -> String {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let markers: Vec<String> = modal_markers
        .iter()
        .map(|marker| squeezed(marker))
        .collect();
    let modal = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        let flat = squeezed(&since);
        if markers.iter().all(|marker| flat.contains(marker.as_str()))
            && flat.contains("Esc=cancel")
        {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`{command}` did not raise the modal showing {modal_markers:?} within {PTY_DEADLINE:?}:\n{since}"
        );
    };
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "the question of `{command}` released the terminal:\n{modal}"
    );
    modal
}

/// Esc on the modal, then a repaint forced by a resize so the frame can
/// be checked for the modal's legend being gone. The size alternates
/// between two values from call to call: a resize to the size the pty
/// already has is a no-op, and then no repaint follows.
pub fn esc_cancels_the_modal(pty: &mut Pty, command: &str) {
    static TOGGLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pty.read_for(HOTKEY_GAP);
    let pressed_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(800));
    let rows = if TOGGLE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed) {
        60
    } else {
        59
    };
    let resized_at = pty.raw().len();
    pty.resize(rows, 110).unwrap();
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    assert!(
        pty.raw().len() > resized_at,
        "no repaint followed the resize after Esc on the modal of `{command}`:\n{since}"
    );
    assert!(
        !squeezed(&since).contains("Esc=cancel"),
        "the modal of `{command}` is still up after Esc:\n{since}"
    );
}

/// Runs one fake-LLM turn with no session open, then `.session <name>`:
/// the question whether to carry the exchange into the new session is
/// asked as the modal, not refused. Returns the pty with the modal up.
/// The fake comes back with the pty so it outlives every later turn.
pub fn open_session_after_a_turn(dir: &Path, name: &str) -> (Pty, FakeLlm) {
    let fake = FakeLlm::start(vec![Reply::Text("fixture first reply xk3".to_string())]);
    write_fake_llm_config(dir, &fake);
    let mut pty = spawn_tui(dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"first question xk3\r").unwrap();
    wait_for_squeezed(&mut pty, "fixture first reply xk3", "the first reply");
    pty.read_for(Duration::from_millis(500));
    let asked_at = pty.raw().len();
    pty.send(format!(".session {name}\r").as_bytes()).unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[asked_at..]));
        if squeezed(&since).contains("incorporatesthelastquestionandanswer") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "`.session` after a turn did not ask about the last exchange within {PTY_DEADLINE:?}:\n{since}"
        );
    }
    pty.read_for(HOTKEY_GAP);
    (pty, fake)
}

/// Answers the open modal with `key` and checks the session comes up: the
/// footer names it, `.info session` shows it empty or holding the
/// exchange, and the UI leaves clean. (The window is grown first: the
/// repaint that forces paints every footer cell, where an incremental one
/// skips the cells the old footer shared, and the whole table then fits.)
pub fn answer_and_expect_session(mut pty: Pty, key: &[u8], name: &str, empty: bool) {
    pty.send(key).unwrap();
    pty.read_for(Duration::from_millis(800));
    let grown_at = pty.raw().len();
    pty.resize(120, 120).unwrap();
    pty.read_for(Duration::from_millis(800));
    let grown = strip_ansi(&String::from_utf8_lossy(&pty.raw()[grown_at..]));
    assert!(
        squeezed(&grown).contains(name),
        "the footer does not name `{name}` after answering the modal with {key:?}:\n{grown}"
    );
    let info_at = pty.raw().len();
    pty.send(b".info session\r").unwrap();
    pty.wait_for_after("total_tokens", info_at, PTY_DEADLINE)
        .expect("`.info session` never rendered after the modal was answered");
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
    assert_eq!(
        squeezed(&since).contains("total_tokens0"),
        empty,
        "the session after answering with {key:?} should be empty: {empty}\n{since}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The question `Agent::init` asks when the agent has documents attached,
/// as the plain REPL has always worded it.
pub const INIT_RAG_QUESTION: &str = "The agent has documents attached, init RAG?";

/// An agent with one markdown document beside its config, so starting it
/// asks whether to initialize its RAG.
pub fn write_agent_with_documents(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("notes.md"),
        "# Notes\n\nThe probe document for the agent's knowledge base.\n",
    )
    .unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: an agent with documents\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are an assistant with notes. Reply briefly.\n\
             documents:\n\
             \x20 - notes.md\n"
        ),
    )
    .unwrap();
}

/// The one shape every no-tty refusal has: `Refusing to <action>
/// non-interactively. Re-run in a terminal<remedy>`. Returns the action
/// and the remedy, or `None` when no line of `out` has the shape.
pub fn refusal_shape(out: &str) -> Option<(String, String)> {
    const HEAD: &str = "Refusing to ";
    const MIDDLE: &str = " non-interactively. Re-run in a terminal";
    out.lines().find_map(|line| {
        let line = line.trim_start_matches("Error: ");
        let rest = line.strip_prefix(HEAD)?;
        let at = rest.find(MIDDLE)?;
        Some((
            rest[..at].to_string(),
            rest[at + MIDDLE.len()..].to_string(),
        ))
    })
}

/// The remedy a `coyote --install` refusal ends with.
pub const CLI_INSTALL_REMEDY: &str = ", with --install-force (installs), or with --yes (updates).";

/// The remedy a `.install` refusal in the plain REPL ends with.
pub const REPL_CONFLICT_REMEDY: &str = ", with --force (installs).";

/// The head of the refusal an `.install` with no one to ask gives at the
/// conflicting MCP server `write_conflicting_local_mcp_server` seeds.
pub const MCP_CONFLICT_REFUSAL: &str =
    "Refusing to merge the already-installed MCP server 'bundle-k7s-server' non-interactively";
/// Where every no-terminal refusal sends the user.
pub const TERMINAL_HINT: &str = "Re-run in a terminal";

/// Whether an SGR sequence sets a foreground, background or underline
/// colour (the 16 named colours, their bright twins, or a 38/48/58
/// extended-colour introducer).
pub fn is_colour_sgr(params: &[u16]) -> bool {
    let mut i = 0;
    while i < params.len() {
        let p = params[i];
        if (30..=37).contains(&p)
            || (40..=47).contains(&p)
            || (90..=97).contains(&p)
            || (100..=107).contains(&p)
            || p == 38
            || p == 48
            || p == 58
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Writes an `$EDITOR` that records into `launched` the path it was asked
/// to open and, on a second line, whether that file `existed` or was
/// `missing` when the editor started; a missing file is saved the way an
/// editor's save would (a new role or macro exists only once its editor
/// wrote it). Returns the editor's path.
pub fn recording_editor(dir: &Path, launched: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let editor = dir.join("recording-editor.sh");
    fs::write(
        &editor,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" > '{launched}'\n\
             if [ -e \"$1\" ]; then echo existed; else echo missing; printf 'saved\\n' > \"$1\"; fi >> '{launched}'\n\
             exit 0\n",
            launched = launched.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    editor.to_str().unwrap().to_string()
}

/// Runs one git command in `repo`, with an identity so `commit` works on a
/// machine without one, and with the developer's global and system config
/// shut out so signing, hook paths or a default branch name cannot leak
/// into the fixture repo.
pub fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.email=tests@example.invalid",
            "-c",
            "user.name=tests",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "git {args:?} failed in {}",
        repo.display()
    );
}

/// The bundle's `mcp.json`: one stdio server whose env references every
/// name in `secrets` as `{{NAME}}`.
pub fn bundle_mcp_json(secrets: &[&str]) -> String {
    let env: Vec<String> = secrets
        .iter()
        .map(|name| format!("\"{name}_VAR\": \"{{{{{name}}}}}\""))
        .collect();
    format!(
        "{{\"mcpServers\": {{\"bundle-k7s-server\": {{\"type\": \"stdio\", \"command\": \"echo\", \
         \"args\": [\"k7s\"], \"env\": {{{}}}}}}}}}\n",
        env.join(", ")
    )
}

/// A local git repository laid out as a bundle (`coyote-bundle.yaml`, a
/// role, an `mcp.json` referencing `secrets`), committed so `.install`
/// can clone it, at `dir/bundle-src`. Returns the repository path.
pub fn write_bundle_repo(dir: &Path, name: &str, secrets: &[&str]) -> PathBuf {
    let repo = dir.join("bundle-src");
    write_bundle_repo_at(&repo, name, secrets);
    repo
}

/// `write_bundle_repo` at a path of the caller's choosing, for a test
/// that installs two bundles; the store tells bundles apart by source.
pub fn write_bundle_repo_at(repo: &Path, name: &str, secrets: &[&str]) {
    fs::create_dir_all(repo.join("roles")).unwrap();
    fs::write(repo.join("coyote-bundle.yaml"), format!("name: {name}\n")).unwrap();
    fs::write(
        repo.join("roles").join(format!("{name}-role.md")),
        "---\nmodel: fake:fake-model\n---\nYou are the bundle role.\n",
    )
    .unwrap();
    fs::write(
        repo.join("roles").join(format!("{name}-obsolete.md")),
        "---\nmodel: fake:fake-model\n---\nYou are the role the update drops.\n",
    )
    .unwrap();
    fs::write(repo.join("mcp.json"), bundle_mcp_json(secrets)).unwrap();
    git(repo, &["init", "-q"]);
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "bundle"]);
}

/// A local `mcp.json` holding the server `write_bundle_repo` ships under a
/// different definition, so the merge hits a conflict it would otherwise
/// resolve through a menu. Returns what was written.
pub fn write_conflicting_local_mcp_server(dir: &Path) -> String {
    let local = "{\"mcpServers\": {\"bundle-k7s-server\": {\"type\": \"stdio\", \"command\": \"printf\", \
                 \"args\": [\"local-definition-k7s\"], \"env\": {}}}}\n";
    fs::write(dir.join("mcp.json"), local).unwrap();
    local.to_string()
}

/// The UI after `.info` still takes keys, then Ctrl+D leaves it clean.
pub fn assert_ui_alive_then_quit(pty: &mut Pty, after: &str) -> String {
    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("`.info` after {after} never rendered: {e:?}"));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    transcript
}

/// Forces a full repaint and returns the stripped frame painted since the
/// resize; the painter only repaints changed cells, so a frame read
/// straight after a key may be a partial diff.
pub fn repainted_frame(pty: &mut Pty, rows: u16, cols: u16) -> String {
    pty.read_for(Duration::from_millis(500));
    let from = pty.raw().len();
    pty.resize(rows, cols).unwrap();
    pty.read_for(Duration::from_millis(800));
    strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]))
}

/// The vault refuses a password file anyone else can read; the fixture
/// writes it with the default mode, so a case that stores a secret
/// tightens it first.
pub fn secure_vault_password_file(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir.join("vault-pass"), fs::Permissions::from_mode(0o600)).unwrap();
}

/// The notice the UI used to print instead of asking, gone now that the
/// question is a modal.
pub const RETIRED_RAG_NOTICE: &str = "starting without RAG";

/// Starts the UI on an agent-with-documents config and sends `.agent
/// <name>`; hands back the pty and the byte offset the command was sent
/// at once the Confirm modal asking `init RAG?` is on screen.
pub fn open_the_init_rag_modal(dir: &Path, name: &str) -> (Pty, usize) {
    write_dry_run_config_with_embeddings(dir);
    write_agent_with_documents(dir, name);
    let mut pty = spawn_tui(dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 120).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    let command = format!(".agent {name}");
    pty.send(format!("{command}\r").as_bytes()).unwrap();
    let modal = wait_for_the_modal(
        &mut pty,
        before,
        &command,
        &[&squeezed(INIT_RAG_QUESTION), "y=yes"],
    );
    assert!(
        !squeezed(&modal).contains("(Y/n)") && !squeezed(&modal).contains("(y/N)"),
        "`init RAG?` was asked on the raw tty, not in the modal:\n{modal}"
    );
    assert!(
        !modal.contains(RETIRED_RAG_NOTICE),
        "the retired `{RETIRED_RAG_NOTICE}` notice was printed under the UI:\n{modal}"
    );
    (pty, before)
}
