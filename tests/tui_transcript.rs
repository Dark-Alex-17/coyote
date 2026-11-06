//! Transcript cases for the full-screen UI: what the real binary shows in
//! the scrollback before and after the first turn. Each case boots the
//! binary cold (fresh config and cache dirs, a dry-run model). Unix-only
//! for the same reasons as the lifecycle suite; cases run one at a time
//! because each takes a controlling tty.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::pty::{Pty, strip_ansi};
use serial_test::serial;

/// Covers the keyboard-enhancement probe, which waits two seconds for an
/// answer a pseudo-terminal never gives.
const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Hotkeys on a modal count only once both the last keystroke and the
/// modal's appearance are at least HOTKEY_IDLE (1 s) old; 1200 ms settles
/// both.
const HOTKEY_GAP: Duration = Duration::from_millis(1200);

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
        "coyote-tui-transcript-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

/// A model-free config: a dry-run model, streaming on, nothing that could
/// prompt on exit.
fn write_dry_run_config(dir: &Path) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             stream: true\n\
             save_session: false\n\
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
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

fn fixture_path() -> std::ffi::OsString {
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap()
}

/// Pins `COYOTE_CONFIG_DIR`/`COYOTE_CACHE_DIR`, strips every inherited
/// `COYOTE_*`, selects the TUI through the provisional env branch.
fn base_env(cmd: &mut Command, dir: &Path) {
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("COYOTE_UI", "tui")
        .env("HOME", dir.join("home"))
        .env("TERM", "xterm-256color")
        .env("PATH", fixture_path())
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR");
}

const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";

const WAIT_WRAPPER: &str =
    "'{coyote}' </dev/tty & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

/// The binary on a fresh pty under a bash wrapper that reports the exit
/// code and the tty flags once it is gone.
fn spawn_tui(dir: &Path) -> Pty {
    let mut cmd = Command::new("bash");
    base_env(&mut cmd, dir);
    cmd.arg("-c").arg(
        WAIT_WRAPPER
            .replace("{coyote}", built_binary().to_str().unwrap())
            .replace("{report}", TTY_REPORT),
    );
    Pty::spawn(cmd).unwrap()
}

fn assert_tty_restored(transcript: &str) {
    let flags = transcript
        .lines()
        .find_map(|line| line.strip_prefix("tty-flags: "))
        .unwrap_or_else(|| panic!("no tty-flags line in the transcript:\n{transcript}"));
    let tokens: Vec<&str> = flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
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

fn exit_code(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("exit="))
        .unwrap_or_else(|| panic!("no exit= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Waits until the whitespace-squeezed screen text contains `marker` at
/// or after raw offset `from`.
fn wait_for_squeezed_after(pty: &mut Pty, marker: &str, from: usize, what: &str) -> String {
    let wanted = squeezed(marker);
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        let text = pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(
            &pty.raw()[from.min(pty.raw().len())..],
        ));
        if squeezed(&since).contains(&wanted) {
            return since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: {marker:?} never appeared on screen:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------
// The welcome block fills the empty transcript on boot and leaves for
// good once the first turn begins. Ctrl+L clears the screen and repaints
// everything, so the bytes after it are the whole transcript as it stands.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn the_welcome_block_leaves_after_the_first_reply() {
    let dir = fresh_config_dir("welcome");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);

    let booted = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let booted = squeezed(&strip_ansi(&booted));
    assert!(
        booted.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block did not show the interrupt hint on boot:\n{booted}"
    );
    assert!(
        booted.contains(&squeezed(concat!("coyote ", env!("CARGO_PKG_VERSION")))),
        "the welcome block did not show the wordmark and version on boot:\n{booted}"
    );

    let before = pty.raw().len();
    pty.send(b"**bye** welcome-marker\r").unwrap();
    wait_for_squeezed_after(&mut pty, "welcome-marker", before, "the first prompt");
    pty.read_for(Duration::from_millis(500));

    let repaint_at = pty.raw().len();
    pty.send(b"\x0c").unwrap();
    pty.read_for(Duration::from_secs(1));
    let repainted = squeezed(&strip_ansi(&String::from_utf8_lossy(
        &pty.raw()[repaint_at..],
    )));
    assert!(
        repainted.contains(&squeezed("welcome-marker")),
        "the repaint after the first reply does not show the reply:\n{repainted}"
    );
    assert!(
        !repainted.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block is still on screen after the first reply:\n{repainted}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}
