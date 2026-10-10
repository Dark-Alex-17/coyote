//! Usage coverage for `scripts/docker-entrypoint.sh`, the image's PID-2 under tini, run
//! the way the container runs it: a child `sh` under a throwaway HOME with a stand-in
//! `rnsd` and `setsid` on PATH, judged by exit code, output, the rendered
//! `~/.reticulum/config` and what the stand-in daemon recorded. The only edit made to
//! the script before running it is the template path (a baked absolute path the host
//! does not have); the test asserts the original line first.
//!
//! The static text pins live in `scripts_pins.rs`; `scripts/image-smoke.sh` proves the
//! process model, signals and the relay reach against a built image. The live tests at
//! the bottom cover what the smoke does not, against an already-built image: opt in
//! with `COYOTE_IMAGE_TESTS=1 COYOTE_IMAGE=<tag>` and `--include-ignored`.
//!
//! Unix only, like the script.
#![cfg(unix)]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

const TEMPLATE_LINE: &str = "template=/opt/coyote/reticulum.config.tmpl";

/// A throwaway HOME holding the shimmed tools, the relocated script and the rnsd log.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-entrypoint-{label}-{unique}"));
        fs::create_dir_all(root.join("bin")).unwrap();
        let home = Home { root };
        home.write_shims();
        home.relocate_script();
        home
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn config(&self) -> PathBuf {
        self.root.join(".reticulum").join("config")
    }

    fn rnsd_log(&self) -> PathBuf {
        self.root.join("rnsd.log")
    }

    fn template(&self) -> PathBuf {
        self.root.join("reticulum.config.tmpl")
    }

    fn script(&self) -> PathBuf {
        self.root.join("coyote-entrypoint")
    }

    fn write_executable(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join("bin").join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `setsid` runs its command in place; `rnsd` records its argv, the environment
    /// the contract hands it (HOME, PYTHONUNBUFFERED) and whether a canary from the
    /// script's own environment leaked through `env -i`, writes `ready` once its TERM
    /// trap is in place, then idles until TERM.
    fn write_shims(&self) {
        self.write_executable("setsid", "#!/bin/sh\nexec \"$@\"\n");
        let log = self.rnsd_log();
        self.write_executable(
            "rnsd",
            &format!(
                "#!/bin/sh\n\
                 log={log}\n\
                 printf 'pid=%s\\n' \"$$\" >> \"$log\"\n\
                 printf 'argv=%s\\n' \"$*\" >> \"$log\"\n\
                 printf 'PYTHONUNBUFFERED=%s\\n' \"${{PYTHONUNBUFFERED:-unset}}\" >> \"$log\"\n\
                 printf 'HOME=%s\\n' \"${{HOME:-unset}}\" >> \"$log\"\n\
                 printf 'canary=%s\\n' \"${{COYOTE_TEST_CANARY:-unset}}\" >> \"$log\"\n\
                 printf 'stdin=%s\\n' \"$(readlink /proc/$$/fd/0 2>/dev/null || echo unknown)\" >> \"$log\"\n\
                 echo 'rnsd-stub: [Notice] up' >&2\n\
                 trap 'printf \"signal=TERM\\n\" >> \"$log\"; exit 0' TERM\n\
                 printf 'ready\\n' >> \"$log\"\n\
                 while :; do sleep 0.1; done\n",
                log = log.display()
            ),
        );
    }

    /// A main-command body that waits (up to 5 s) for the stand-in rnsd's `ready` line
    /// and then runs `then`: the script TERMs rnsd as soon as the main command returns,
    /// so a command that exits at once races the stub's own startup.
    fn after_rnsd_ready(&self, then: &str) -> String {
        format!(
            "polls=0; until grep -q '^ready$' {log} 2>/dev/null || [ \"$polls\" -ge 100 ]; do \
             sleep 0.05; polls=$((polls + 1)); done; {then}",
            log = self.rnsd_log().display()
        )
    }

    /// A copy of the script whose only difference is the template path.
    fn relocate_script(&self) {
        let original = read(repo_root().join("scripts").join("docker-entrypoint.sh"));
        assert_eq!(
            original.matches(TEMPLATE_LINE).count(),
            1,
            "docker-entrypoint.sh must set `{TEMPLATE_LINE}` exactly once; the test relocates that one line"
        );
        fs::copy(
            repo_root().join("scripts").join("reticulum.config.tmpl"),
            self.template(),
        )
        .unwrap();
        let relocated = original.replace(
            TEMPLATE_LINE,
            &format!("template={}", self.template().display()),
        );
        fs::write(self.script(), relocated).unwrap();
        fs::set_permissions(self.script(), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, env_vars: &[(&str, &str)], args: &[&str]) -> Command {
        let mut path = self.root.join("bin").into_os_string();
        path.push(":/usr/bin:/bin");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(self.script())
            .args(args)
            .env_clear()
            .env("HOME", &self.root)
            .env("PATH", path);
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        cmd
    }

    fn rnsd_recorded(&self) -> String {
        fs::read_to_string(self.rnsd_log()).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(mut cmd: Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = cmd.output().expect("spawn sh");
    (
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

/// Non-comment, non-blank lines from `[interfaces]` on.
fn interface_lines(config: &str) -> Vec<String> {
    config
        .lines()
        .skip_while(|line| line.trim() != "[interfaces]")
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Hermetic: the script under a throwaway HOME with stand-in rnsd/setsid.
// ---------------------------------------------------------------------------

/// First start renders the template: the `[logging]` section rnsd needs for `-vv`, the
/// loopback listener with `ingress_control = No`, transport on, no opt-in stanza, no
/// marker line, no unexpanded placeholder, no `share_instance`; the file is owner-only
/// and the main command's status is the script's.
#[test]
fn a_first_start_renders_the_default_config_and_passes_the_exit_code_through() {
    let home = Home::new("first-start");
    let (code, stdout, stderr) = run(home.command(&[], &["sh", "-c", "echo main-ran; exit 7"]));
    assert_eq!(code, 7, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(
        stdout, "main-ran\n",
        "stdout must be the main command's alone"
    );
    assert!(
        !stderr.contains("WARNING"),
        "a clean first start must not warn:\n{stderr}"
    );

    let config = read(home.config());
    for needle in [
        "[reticulum]",
        "enable_transport = True",
        "[logging]",
        "loglevel = 4",
        "[[Coyote Sessions]]",
        "type = TCPServerInterface",
        "listen_ip = 127.0.0.1",
        "listen_port = 4242",
        "ingress_control = No",
    ] {
        assert!(
            config.contains(needle),
            "rendered config lacks `{needle}`:\n{config}"
        );
    }
    for absent in [
        "#@",
        "@RELAY_HOST@",
        "@RELAY_PORT@",
        "AutoInterface",
        "Team Relay",
        "share_instance",
    ] {
        assert!(
            !config.contains(absent),
            "rendered default config must not contain `{absent}`:\n{config}"
        );
    }
    assert_eq!(
        interface_lines(&config),
        [
            "[interfaces]",
            "[[Coyote Sessions]]",
            "type = TCPServerInterface",
            "enabled = Yes",
            "listen_ip = 127.0.0.1",
            "listen_port = 4242",
            "ingress_control = No",
        ],
        "the default config has the loopback listener only"
    );
    let mode = fs::metadata(home.config()).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "the rendered config is owner-only (mktemp), got {mode:o}"
    );
    assert!(
        fs::read_dir(home.config().parent().unwrap())
            .unwrap()
            .all(|e| e.unwrap().file_name() == "config"),
        "no temp file is left beside the config"
    );
}

/// The two opt-in variables add exactly their stanza, with the relay's host and port
/// substituted; `COYOTE_MESH_LAN` is `1` or nothing.
#[test]
fn the_lan_and_relay_variables_add_their_stanzas_and_only_those() {
    let home = Home::new("lan-relay");
    let (code, _, stderr) = run(home.command(
        &[
            ("COYOTE_MESH_LAN", "1"),
            ("COYOTE_MESH_RELAY", "pn.example-host.local:4243"),
        ],
        &["sh", "-c", "exit 0"],
    ));
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("WARNING"), "{stderr}");
    assert!(
        stderr.contains("coyote-entrypoint: COYOTE_MESH_LAN=1: with --network host this rnsd listens for LAN peers"),
        "COYOTE_MESH_LAN=1 prints the LAN note once the AutoInterface is written:\n{stderr}"
    );
    let config = read(home.config());
    assert_eq!(
        interface_lines(&config),
        [
            "[interfaces]",
            "[[Coyote Local]]",
            "type = AutoInterface",
            "enabled = Yes",
            "[[Coyote Sessions]]",
            "type = TCPServerInterface",
            "enabled = Yes",
            "listen_ip = 127.0.0.1",
            "listen_port = 4242",
            "ingress_control = No",
            "[[Team Relay]]",
            "type = TCPClientInterface",
            "enabled = Yes",
            "target_host = pn.example-host.local",
            "target_port = 4243",
        ],
    );
    assert!(
        !config.contains("#@"),
        "marker lines must never reach the output:\n{config}"
    );

    let relay_only = Home::new("relay-only");
    let (code, _, _) = run(relay_only.command(
        &[("COYOTE_MESH_RELAY", "10.0.0.7:4242")],
        &["sh", "-c", "exit 0"],
    ));
    assert_eq!(code, 0);
    let config = read(relay_only.config());
    assert!(config.contains("target_host = 10.0.0.7") && config.contains("target_port = 4242"));
    assert!(
        !config.contains("AutoInterface"),
        "no LAN stanza without COYOTE_MESH_LAN=1"
    );

    let lan_true = Home::new("lan-true");
    let (code, _, stderr) =
        run(lan_true.command(&[("COYOTE_MESH_LAN", "true")], &["sh", "-c", "exit 0"]));
    assert_eq!(code, 0);
    assert!(
        !read(lan_true.config()).contains("AutoInterface"),
        "only `COYOTE_MESH_LAN=1` enables the AutoInterface"
    );
    assert!(
        stderr.contains("WARNING: COYOTE_MESH_LAN=true is not 1; AutoInterface not added"),
        "a COYOTE_MESH_LAN that is neither 1 nor empty warns:\n{stderr}"
    );
}

/// A template checked out with CRLF line endings renders the same default config: the
/// awk program strips the CR before it matches the `#@if` / `#@end` markers, so the
/// optional stanzas stay out and no CR reaches the file.
#[test]
fn a_crlf_template_renders_the_default_config_without_the_optional_stanzas() {
    let home = Home::new("crlf-template");
    let template = read(home.template());
    assert!(
        !template.contains('\r'),
        "the checked-in template is LF; this test makes the CRLF copy itself"
    );
    fs::write(home.template(), template.replace('\n', "\r\n")).unwrap();
    let (code, _, stderr) = run(home.command(&[], &["sh", "-c", "exit 0"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("WARNING"), "{stderr}");
    let config = read(home.config());
    assert!(
        config.contains("[[Coyote Sessions]]") && config.contains("listen_ip = 127.0.0.1"),
        "the loopback listener is rendered from a CRLF template:\n{config}"
    );
    for absent in ["AutoInterface", "Team Relay", "#@", "\r"] {
        assert!(
            !config.contains(absent),
            "a CRLF template must not leak `{}` into the rendered config:\n{config}",
            absent.escape_debug()
        );
    }
}

/// Values that are not `host:port` (or carry characters the awk replacement cannot take
/// safely) warn, write no relay stanza, and never change the main command's status.
#[test]
fn a_malformed_relay_warns_writes_no_stanza_and_never_blocks_the_main_command() {
    for bad in [
        "bad",
        ":4242",
        "host:",
        "host:70000",
        "host:0",
        "host:12ab",
        "ho&st:4242",
        "a\\b:4242",
        "[::1]:4242",
    ] {
        let home = Home::new("bad-relay");
        let (code, stdout, stderr) = run(home.command(
            &[("COYOTE_MESH_RELAY", bad)],
            &["sh", "-c", &home.after_rnsd_ready("echo ran; exit 4")],
        ));
        assert_eq!(code, 4, "COYOTE_MESH_RELAY={bad:?}: {stderr}");
        assert_eq!(stdout, "ran\n");
        assert!(
            stderr.contains("WARNING") && stderr.contains("not host:port"),
            "COYOTE_MESH_RELAY={bad:?} must warn `not host:port`:\n{stderr}"
        );
        let config = read(home.config());
        assert!(
            !config.contains("Team Relay") && !config.contains("@RELAY"),
            "COYOTE_MESH_RELAY={bad:?} must write no relay stanza:\n{config}"
        );
        assert!(
            home.rnsd_recorded().contains("argv=-vv"),
            "rnsd still starts on the rest of the config for COYOTE_MESH_RELAY={bad:?}"
        );
    }
}

/// An existing `~/.reticulum/config` is never touched, whatever the variables say; the
/// variables that would have changed a fresh render are named in one note.
#[test]
fn an_existing_config_is_never_overwritten() {
    let home = Home::new("existing");
    fs::create_dir_all(home.config().parent().unwrap()).unwrap();
    let sentinel = "[reticulum]\n# operator edit\nenable_transport = No\n";
    fs::write(home.config(), sentinel).unwrap();
    let (code, _, stderr) = run(home.command(
        &[
            ("COYOTE_MESH_LAN", "1"),
            ("COYOTE_MESH_RELAY", "relay.example:4242"),
        ],
        &["sh", "-c", &home.after_rnsd_ready("exit 0")],
    ));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        read(home.config()),
        sentinel,
        "the operator's config was rewritten"
    );
    assert!(
        home.rnsd_recorded().contains("argv=-vv"),
        "rnsd starts on the operator's config"
    );
    assert!(
        stderr.contains(&format!(
            "coyote-entrypoint: {} exists; COYOTE_MESH_RELAY/COYOTE_MESH_LAN not applied (edit the file or remove it)",
            home.config().display()
        )),
        "the unapplied variables are named, as a note rather than a WARNING:\n{stderr}"
    );
    assert!(!stderr.contains("WARNING"), "{stderr}");
    assert!(
        fs::read_dir(home.config().parent().unwrap())
            .unwrap()
            .all(|e| e.unwrap().file_name() == "config"),
        "no temp file is rendered beside an existing config"
    );

    let quiet = Home::new("existing-quiet");
    fs::create_dir_all(quiet.config().parent().unwrap()).unwrap();
    fs::write(quiet.config(), sentinel).unwrap();
    let (code, _, stderr) =
        run(quiet.command(&[], &["sh", "-c", &quiet.after_rnsd_ready("exit 0")]));
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("not applied"),
        "with no opt-in variable set an existing config draws no note:\n{stderr}"
    );
}

/// rnsd is a sidecar: `COYOTE_MESH_RNSD=0` skips it with one stderr line; when its
/// config dir cannot be created or the template is missing the script warns and the
/// main command still runs with its own status.
#[test]
fn rnsd_is_skipped_on_request_and_its_failure_never_blocks_the_main_command() {
    let off = Home::new("rnsd-off");
    let (code, stdout, stderr) = run(off.command(
        &[("COYOTE_MESH_RNSD", "0")],
        &["sh", "-c", "echo ran; exit 3"],
    ));
    assert_eq!(code, 3);
    assert_eq!(stdout, "ran\n");
    assert!(
        stderr.contains("COYOTE_MESH_RNSD=0, rnsd not started"),
        "the opt-out is said on stderr:\n{stderr}"
    );
    assert!(
        !off.config().exists(),
        "no config is rendered when rnsd is off"
    );
    assert!(off.rnsd_recorded().is_empty(), "rnsd must not start");

    let other = Home::new("rnsd-other-value");
    let (code, _, stderr) = run(other.command(
        &[("COYOTE_MESH_RNSD", "false")],
        &["sh", "-c", &other.after_rnsd_ready("exit 0")],
    ));
    assert_eq!(code, 0);
    assert!(
        other.rnsd_recorded().contains("argv=-vv"),
        "only the literal `0` opts out; `false` starts rnsd"
    );
    assert!(
        stderr.contains("WARNING: COYOTE_MESH_RNSD=false is not 0; rnsd started"),
        "a COYOTE_MESH_RNSD that is neither 0, 1 nor empty warns and still starts rnsd:\n{stderr}"
    );

    let blocked = Home::new("rnsd-blocked");
    fs::write(
        blocked.path().join(".reticulum"),
        "a file where the dir goes",
    )
    .unwrap();
    let (code, stdout, stderr) = run(blocked.command(&[], &["sh", "-c", "echo ran; exit 5"]));
    assert_eq!(code, 5, "{stderr}");
    assert_eq!(stdout, "ran\n");
    assert!(
        stderr.contains("WARNING") && stderr.contains("rnsd not started"),
        "an uncreatable config dir warns and skips rnsd:\n{stderr}"
    );
    assert!(blocked.rnsd_recorded().is_empty());

    let no_template = Home::new("no-template");
    fs::remove_file(no_template.template()).unwrap();
    let (code, _, stderr) = run(no_template.command(&[], &["sh", "-c", "exit 6"]));
    assert_eq!(code, 6, "{stderr}");
    assert!(
        stderr.contains("missing or unreadable") && stderr.contains("rnsd not started"),
        "{stderr}"
    );
    assert!(!no_template.config().exists());
}

/// rnsd runs as `rnsd -vv` under `env -i` with HOME, PATH and `PYTHONUNBUFFERED=1` and
/// nothing else of the script's environment, `/dev/null` on fd 0, is TERMed once the
/// main command returns, and its own stderr lines reach the script's stderr raw; the
/// main command keeps the real stdin. (That INT never reaches rnsd is a property of
/// the real `setsid`, which the stand-in does not have; `scripts/image-smoke.sh`
/// proves it against the image.)
///
/// stderr goes to a file rather than a pipe: were the script to `exec` the main command
/// (or forget to stop rnsd), the orphaned daemon would hold a pipe open and this test
/// would hang instead of failing. The stub's pid is recorded so an orphan the script
/// never TERMed is reaped.
#[test]
fn rnsd_gets_its_scoped_environment_and_a_term_after_the_main_command_returns() {
    let home = Home::new("rnsd-env");
    let stderr_path = home.path().join("script.stderr");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let mut child = home
        .command(
            &[("COYOTE_TEST_CANARY", "leaked")],
            &["sh", "-c", &home.after_rnsd_ready("cat; exit 0")],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_file)
        .spawn()
        .expect("spawn sh");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from-the-container-stdin\n")
        .unwrap();
    let started = Instant::now();
    let out = child.wait_with_output().unwrap();
    let elapsed = started.elapsed();
    thread::sleep(Duration::from_millis(300));
    let recorded = home.rnsd_recorded();
    if !recorded.contains("signal=TERM\n") {
        let orphan = recorded
            .lines()
            .find_map(|line| line.strip_prefix("pid="))
            .filter(|pid| {
                let probe = Command::new("kill").args(["-0", pid]).output();
                probe.is_ok_and(|out| out.status.success())
            })
            .map(str::to_owned);
        if let Some(pid) = &orphan {
            let _ = Command::new("kill").args(["-9", pid]).output();
        }
    }
    let stderr = read(&stderr_path);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "from-the-container-stdin\n",
        "the main command runs in the foreground and reads the container's stdin"
    );
    assert!(
        stderr.contains("rnsd-stub: [Notice] up"),
        "rnsd's stderr reaches the script's stderr unprefixed:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the script waited {elapsed:?} for rnsd; TERM must end it well inside the 5 s KILL bound"
    );

    assert!(recorded.contains("argv=-vv\n"), "rnsd argv:\n{recorded}");
    assert!(
        recorded.contains("PYTHONUNBUFFERED=1\n"),
        "PYTHONUNBUFFERED must be set for rnsd:\n{recorded}"
    );
    assert!(
        recorded.contains(&format!("HOME={}\n", home.path().display())),
        "rnsd must get HOME, the root it finds ~/.reticulum under:\n{recorded}"
    );
    assert!(
        recorded.contains("canary=unset\n"),
        "the script's environment must not reach rnsd (env -i):\n{recorded}"
    );
    assert!(
        recorded.contains("stdin=/dev/null\n") || recorded.contains("stdin=unknown\n"),
        "rnsd's fd 0 is /dev/null:\n{recorded}"
    );
    assert!(
        recorded.contains("signal=TERM\n"),
        "rnsd must be TERMed after the main command:\n{recorded}"
    );
}

/// A daemon that ignores TERM is KILLed after the bounded wait and the script still
/// exits with the main command's status.
#[test]
fn a_daemon_that_ignores_term_is_killed_after_the_bounded_wait() {
    let home = Home::new("rnsd-stubborn");
    let log = home.rnsd_log();
    home.write_executable(
        "rnsd",
        &format!(
            "#!/bin/sh\ntrap '' TERM\nprintf 'started\\n' >> {log}\nwhile :; do sleep 0.1; done\n",
            log = log.display()
        ),
    );
    let started = Instant::now();
    let (code, _, stderr) = run(home.command(&[], &["sh", "-c", "sleep 0.3; exit 8"]));
    let elapsed = started.elapsed();
    assert_eq!(code, 8, "{stderr}");
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(9),
        "bounded wait (25 x 0.2 s) then KILL; took {elapsed:?}"
    );
}

/// Kills a whole process group on drop, so a wrapper that died early (the RED shape of
/// the signal test) cannot leave its main command looping after the test.
struct Group(u32);

impl Group {
    fn signal(&self, signal: &str) {
        let status = Command::new("sh")
            .args([
                "-c",
                r#"kill -s "$1" -- "-$2""#,
                "_",
                signal,
                &self.0.to_string(),
            ])
            .status()
            .expect("spawn sh kill");
        assert!(
            status.success(),
            "kill -s {signal} to process group {}",
            self.0
        );
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = Command::new("sh")
            .args([
                "-c",
                r#"kill -s KILL -- "-$1" 2>/dev/null"#,
                "_",
                &self.0.to_string(),
            ])
            .status();
    }
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{what} did not appear within 10 s"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The container's `tini -s -g` delivers every forwarded signal to the whole process
/// group, so the wrapper receives HUP, QUIT, USR1 and USR2 alongside the main command and
/// must outlive them: a main command that handles them keeps running and its own exit
/// status (here from USR2) is the script's; a main command with default dispositions dies
/// of HUP and the script reports that death verbatim (129). rnsd is TERMed by the script
/// after the main command either way.
///
/// The script runs in a fresh process group (what tini -g signals) and the stand-in rnsd
/// ignores these signals, standing in for the real daemon's own session.
#[test]
fn usage_probe_the_wrapper_outlives_every_signal_tini_forwards_to_the_group() {
    let home = Home::new("group-signals");
    let log = home.rnsd_log();
    home.write_executable(
        "rnsd",
        &format!(
            "#!/bin/sh\ntrap '' HUP QUIT USR1 USR2\ntrap 'printf \"signal=TERM\\\\n\" >> {log}; exit 0' TERM\n\
             printf 'started\\\\n' >> {log}\nwhile :; do sleep 0.1; done\n",
            log = log.display()
        ),
    );

    // First, a main command that handles the four signals and exits 9 on USR2.
    let ready = home.path().join("handled.ready");
    let stdout_path = home.path().join("handled.stdout");
    let stderr_path = home.path().join("handled.stderr");
    let mut child = home
        .command(
            &[],
            &[
                "sh",
                "-c",
                "trap 'echo got-HUP' HUP; trap 'echo got-QUIT' QUIT; trap 'echo got-USR1' USR1; \
                 trap 'exit 9' USR2; : > \"$0\"; while :; do sleep 0.1; done",
                ready.to_str().unwrap(),
            ],
        )
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout_path).unwrap())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawn sh");
    let group = Group(child.id());
    wait_for(&ready, "the handled main command's ready marker");
    wait_for(&log, "the stand-in rnsd's start line");
    for signal in ["HUP", "QUIT", "USR1"] {
        group.signal(signal);
        thread::sleep(Duration::from_millis(400));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the wrapper died of {signal} while its main command was still running:\n{}",
            read(&stderr_path)
        );
    }
    group.signal("USR2");
    let status = child.wait().unwrap();
    drop(group);
    let stdout = read(&stdout_path);
    assert_eq!(
        status.code(),
        Some(9),
        "the main command's exit status after USR2 is the script's:\n{}",
        read(&stderr_path)
    );
    for line in ["got-HUP", "got-QUIT", "got-USR1"] {
        assert_eq!(
            stdout.matches(line).count(),
            1,
            "the main command handled each signal exactly once:\n{stdout}"
        );
    }
    assert!(
        home.rnsd_recorded().contains("signal=TERM\n"),
        "rnsd is TERMed once the main command returns:\n{}",
        home.rnsd_recorded()
    );

    // Then a main command with default dispositions: HUP kills it, the script reports 129.
    let _ = fs::remove_file(&log);
    let ready = home.path().join("unhandled.ready");
    let stderr_path = home.path().join("unhandled.stderr");
    let mut child = home
        .command(
            &[],
            &[
                "sh",
                "-c",
                ": > \"$0\"; exec sleep 30",
                ready.to_str().unwrap(),
            ],
        )
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawn sh");
    let group = Group(child.id());
    wait_for(&ready, "the unhandled main command's ready marker");
    wait_for(&log, "the stand-in rnsd's start line");
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    group.signal("HUP");
    let status = child.wait().unwrap();
    let elapsed = started.elapsed();
    drop(group);
    assert_eq!(
        status.code(),
        Some(129),
        "an unhandled HUP on the main command is reported verbatim (128 + 1):\n{}",
        read(&stderr_path)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the script returned {elapsed:?} after the main command died"
    );
    assert!(
        home.rnsd_recorded().contains("signal=TERM\n"),
        "rnsd is TERMed after the main command's death too:\n{}",
        home.rnsd_recorded()
    );
}

/// Passthrough: `sh`, `bash` and an absolute path run as given; anything else is a
/// `coyote` argument, and rnsd starts ahead of that branch as it does ahead of
/// passthrough. The script's own helpers are not on PATH here, so the coyote branch is
/// observed through a stand-in `coyote`.
#[test]
fn the_first_argument_selects_passthrough_or_the_coyote_binary() {
    let home = Home::new("dispatch");
    home.write_executable("coyote", "#!/bin/sh\necho \"coyote-stub:$*\"\nexit 0\n");
    let (code, stdout, _) = run(home.command(&[("COYOTE_MESH_RNSD", "0")], &["--version"]));
    assert_eq!(code, 0);
    assert_eq!(stdout, "coyote-stub:--version\n");

    let (code, stdout, _) =
        run(home.command(&[("COYOTE_MESH_RNSD", "0")], &["/bin/echo", "by-path"]));
    assert_eq!(code, 0);
    assert_eq!(stdout, "by-path\n");

    let (code, stdout, _) = run(home.command(
        &[("COYOTE_MESH_RNSD", "0")],
        &["sh", "-c", "echo via-sh; exit 9"],
    ));
    assert_eq!(code, 9);
    assert_eq!(stdout, "via-sh\n");

    let (code, stdout, _) = run(home.command(&[("COYOTE_MESH_RNSD", "0")], &[]));
    assert_eq!(code, 0);
    assert_eq!(
        stdout, "coyote-stub:\n",
        "no arguments runs coyote with none"
    );

    let with_rnsd = Home::new("dispatch-rnsd");
    with_rnsd.write_executable(
        "coyote",
        &format!(
            "#!/bin/sh\n{}\n",
            with_rnsd.after_rnsd_ready("echo \"coyote-stub:$*\"; exit 0")
        ),
    );
    let (code, stdout, stderr) = run(with_rnsd.command(&[], &["--version"]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "coyote-stub:--version\n");
    assert!(
        with_rnsd.rnsd_recorded().contains("argv=-vv"),
        "rnsd starts before the coyote branch too:\n{}",
        with_rnsd.rnsd_recorded()
    );
}

// ---------------------------------------------------------------------------
// Live: an already-built coyote image. Opt in with COYOTE_IMAGE_TESTS=1 and
// COYOTE_IMAGE=<tag>; needs Docker. Covers what scripts/image-smoke.sh does not.
// ---------------------------------------------------------------------------

const LIVE_SWITCH: &str = "COYOTE_IMAGE_TESTS";
const IMAGE_VAR: &str = "COYOTE_IMAGE";

fn live_switch_is_on(value: Option<&OsStr>) -> bool {
    let Some(value) = value else { return false };
    let value = value.to_string_lossy();
    let value = value.trim();
    !(value.is_empty()
        || value == "0"
        || ["false", "no", "off"]
            .iter()
            .any(|off| value.eq_ignore_ascii_case(off)))
}

/// `Some(image)` to run, `None` after printing the conventional `skipping:` line.
fn live_image() -> Option<String> {
    let instructions = "`COYOTE_IMAGE_TESTS=1 COYOTE_IMAGE=<tag> cargo test --test docker_entrypoint_script -- --include-ignored` with a Docker daemon available";
    if !live_switch_is_on(env::var_os(LIVE_SWITCH).as_deref()) {
        eprintln!("skipping: set {LIVE_SWITCH}=1 and run {instructions}");
        return None;
    }
    match env::var(IMAGE_VAR) {
        Ok(image) if !image.trim().is_empty() => Some(image),
        _ => {
            eprintln!("skipping: {IMAGE_VAR} names no built image; run {instructions}");
            None
        }
    }
}

fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn docker {args:?}: {e}"))
}

fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {args:?} failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn label() -> String {
    format!("coyote-image-test={}", std::process::id())
}

/// A named container removed on drop (panics unwind, so a failed assertion tears down).
struct Container {
    name: String,
}

impl Container {
    fn detached(image: &str, extra: &[&str], command: &[&str]) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("coyote-image-test-{}-{unique}", std::process::id());
        let lbl = label();
        let mut args = vec!["run", "-d", "--name", &name, "--label", &lbl];
        args.extend_from_slice(extra);
        args.push(image);
        args.extend_from_slice(command);
        docker_ok(&args);
        Container { name }
    }

    fn exit_code(&self) -> String {
        docker_ok(&["inspect", "--format", "{{.State.ExitCode}}", &self.name])
            .trim()
            .to_owned()
    }

    fn logs(&self) -> String {
        let out = docker(&["logs", &self.name]);
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
    }
}

/// The image's metadata contract: exec-form `tini -s -g` entrypoint, CMD left to the
/// user, no HEALTHCHECK, no image-wide PYTHONUNBUFFERED, uid 1000.
#[test]
#[ignore = "needs docker, COYOTE_IMAGE_TESTS=1 and COYOTE_IMAGE=<tag>"]
fn the_image_metadata_keeps_the_entrypoint_contract() {
    let Some(image) = live_image() else { return };
    let inspect = |format: &str| {
        docker_ok(&["image", "inspect", "--format", format, &image])
            .trim()
            .to_owned()
    };
    assert_eq!(
        inspect("{{json .Config.Entrypoint}}"),
        r#"["/usr/bin/tini","-s","-g","--","/usr/local/bin/coyote-entrypoint"]"#
    );
    assert_eq!(
        inspect("{{json .Config.Cmd}}"),
        "null",
        "CMD is left to the user's arguments"
    );
    assert_eq!(
        inspect("{{.Config.Healthcheck}}"),
        "<nil>",
        "no HEALTHCHECK"
    );
    assert_eq!(inspect("{{.Config.User}}"), "1000");
    let env_json = inspect("{{json .Config.Env}}");
    assert!(
        !env_json.contains("PYTHONUNBUFFERED"),
        "PYTHONUNBUFFERED is scoped to the rnsd child, not an image ENV: {env_json}"
    );
}

/// The baked template is root-owned and read-only for uid 1000 under a traversable
/// `/opt/coyote`; the rendered copy is owned by uid 1000; an operator's pre-existing
/// config on a mounted `~/.reticulum` is never overwritten even when the variables ask
/// for stanzas.
#[test]
#[ignore = "needs docker, COYOTE_IMAGE_TESTS=1 and COYOTE_IMAGE=<tag>"]
fn the_template_is_read_only_and_a_mounted_config_is_never_overwritten() {
    let Some(image) = live_image() else { return };
    let lbl = label();
    let probe = docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-e",
        "COYOTE_MESH_RNSD=0",
        &image,
        "sh",
        "-c",
        "stat -c '%u:%g %a %n' /opt/coyote /opt/coyote/reticulum.config.tmpl; \
         test -r /opt/coyote/reticulum.config.tmpl && echo readable; \
         test -w /opt/coyote/reticulum.config.tmpl || echo not-writable",
    ]);
    assert!(probe.contains("0:0 755 /opt/coyote\n"), "{probe}");
    assert!(
        probe.contains("0:0 644 /opt/coyote/reticulum.config.tmpl\n"),
        "{probe}"
    );
    assert!(
        probe.contains("readable\n") && probe.contains("not-writable\n"),
        "{probe}"
    );

    let owner = docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        &image,
        "sh",
        "-c",
        "stat -c '%u %a' ~/.reticulum/config",
    ]);
    assert_eq!(owner.trim(), "1000 600", "rendered config owner and mode");

    // A tmpfs stands in for the operator's mounted ~/.reticulum; the first entrypoint run
    // renders into it, the operator's edit replaces that, and a second entrypoint run (the
    // nested call) must leave the edit alone.
    let kept = docker_ok(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "--tmpfs",
        "/home/agent/.reticulum",
        "-e",
        "COYOTE_MESH_LAN=1",
        "-e",
        "COYOTE_MESH_RELAY=relay.example:4242",
        &image,
        "sh",
        "-c",
        "printf '[reticulum]\\nSENTINEL = yes\\n' > ~/.reticulum/config; \
         COYOTE_MESH_RNSD=0 /usr/local/bin/coyote-entrypoint sh -c true 2>/dev/null; \
         /usr/local/bin/coyote-entrypoint sh -c true 2>/dev/null; \
         cat ~/.reticulum/config",
    ]);
    assert_eq!(
        kept, "[reticulum]\nSENTINEL = yes\n",
        "an existing config was rewritten"
    );
}

/// The main command runs in the script's foreground: it reads the container's stdin,
/// its stdout is the container's stdout alone, and a TERM it handles yields its own exit
/// code (not 143) within docker's grace period.
#[test]
#[ignore = "needs docker, COYOTE_IMAGE_TESTS=1 and COYOTE_IMAGE=<tag>"]
fn the_main_command_owns_stdin_stdout_and_its_exit_code_under_term() {
    let Some(image) = live_image() else { return };
    let lbl = label();
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--label",
            &lbl,
            &image,
            "sh",
            "-c",
            "sleep 1; tr a-z A-Z",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin-reaches-main\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "STDIN-REACHES-MAIN\n",
        "stdout must carry the main command's output only (rnsd goes to stderr)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[Debug]") || stderr.contains("[Notice]"),
        "rnsd's raw log lines reach the container's stderr:\n{stderr}"
    );
    assert!(
        !stderr
            .lines()
            .any(|l| l.starts_with("[rnsd]") || l.starts_with("rnsd:")),
        "rnsd lines are not prefixed:\n{stderr}"
    );

    let graceful = Container::detached(
        &image,
        &[],
        &[
            "bash",
            "-c",
            "trap 'echo main-got-TERM; exit 42' TERM; while :; do sleep 1; done",
        ],
    );
    thread::sleep(Duration::from_secs(2));
    let started = Instant::now();
    docker_ok(&["stop", &graceful.name]);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "docker stop took {elapsed:?}"
    );
    assert_eq!(
        graceful.exit_code(),
        "42",
        "the handled TERM's exit code passes through"
    );
    assert!(
        graceful.logs().contains("main-got-TERM"),
        "{}",
        graceful.logs()
    );
}

/// Sidecar isolation against the real image: an unwritable HOME warns and skips rnsd
/// while the main command runs with its own status; the opt-out line is printed once.
#[test]
#[ignore = "needs docker, COYOTE_IMAGE_TESTS=1 and COYOTE_IMAGE=<tag>"]
fn rnsd_failure_in_the_image_never_blocks_the_main_command() {
    let Some(image) = live_image() else { return };
    let lbl = label();
    let out = docker(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-e",
        "HOME=/nonexistent",
        &image,
        "sh",
        "-c",
        "echo ran; exit 5",
    ]);
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ran\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WARNING: cannot create /nonexistent/.reticulum; rnsd not started"),
        "{stderr}"
    );

    let out = docker(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "--read-only",
        &image,
        "sh",
        "-c",
        "exit 3",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a read-only root without tmpfs still runs the main command"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("rnsd not started"));
}

/// Polls `docker exec` for the loopback listener for up to 30 s.
fn wait_for_4242(container: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let probe = docker(&[
            "exec",
            container,
            "bash",
            "-c",
            "exec 3<>/dev/tcp/127.0.0.1/4242",
        ]);
        if probe.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rnsd did not listen on 127.0.0.1:4242 within 30 s in {container}"
        );
        thread::sleep(Duration::from_millis(500));
    }
}

fn wait_for_exit(container: &Container) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let running = docker_ok(&["inspect", "--format", "{{.State.Running}}", &container.name]);
        if running.trim() == "false" {
            return container.exit_code();
        }
        assert!(
            Instant::now() < deadline,
            "{} still running 10 s after the signal:\n{}",
            container.name,
            container.logs()
        );
        thread::sleep(Duration::from_millis(250));
    }
}

/// Under `tini -s -g` every signal docker forwards reaches the main command directly
/// and the wrapper outlives it: a main command handling HUP, QUIT and USR1 keeps the
/// container up (and rnsd, in its own session, keeps listening), its own exit status
/// on USR2 is the container's; a main command with default dispositions dies of HUP or
/// USR1 and the container exits 128 + n verbatim.
#[test]
#[ignore = "needs docker, COYOTE_IMAGE_TESTS=1 and COYOTE_IMAGE=<tag>"]
fn usage_probe_forwarded_signals_reach_the_main_command_and_the_wrapper_outlives_them() {
    let Some(image) = live_image() else { return };
    {
        let handled = Container::detached(
            &image,
            &[],
            &[
                "bash",
                "-c",
                "trap 'echo got-HUP' HUP; trap 'echo got-QUIT' QUIT; trap 'echo got-USR1' USR1; \
                 trap 'exit 9' USR2; while :; do sleep 1; done",
            ],
        );
        wait_for_4242(&handled.name);
        for signal in ["HUP", "QUIT", "USR1"] {
            docker_ok(&["kill", "-s", signal, &handled.name]);
            thread::sleep(Duration::from_millis(1500));
            let running = docker_ok(&["inspect", "--format", "{{.State.Running}}", &handled.name]);
            assert_eq!(
                running.trim(),
                "true",
                "the container died of {signal} while its main command was handling it:\n{}",
                handled.logs()
            );
        }
        let logs = handled.logs();
        for line in ["got-HUP", "got-QUIT", "got-USR1"] {
            assert_eq!(
                logs.matches(line).count(),
                1,
                "the main command handled each forwarded signal exactly once:\n{logs}"
            );
        }
        wait_for_4242(&handled.name);
        docker_ok(&["kill", "-s", "USR2", &handled.name]);
        assert_eq!(
            wait_for_exit(&handled),
            "9",
            "the main command's own status after USR2 is the container's:\n{}",
            handled.logs()
        );
    }
    for (signal, code) in [("HUP", "129"), ("USR1", "138")] {
        let unhandled =
            Container::detached(&image, &["--label", signal], &["sh", "-c", "sleep 999"]);
        wait_for_4242(&unhandled.name);
        docker_ok(&["kill", "-s", signal, &unhandled.name]);
        assert_eq!(
            wait_for_exit(&unhandled),
            code,
            "an unhandled {signal} on the main command exits 128 + n verbatim:\n{}",
            unhandled.logs()
        );
    }
}

/// The real rnsd never inherits the container's environment: its `/proc/<pid>/environ`
/// holds HOME, PATH and `PYTHONUNBUFFERED=1` and nothing else, so a provider key handed
/// to the container reaches the main command only.
#[test]
#[ignore = "needs docker, COYOTE_IMAGE_TESTS=1 and COYOTE_IMAGE=<tag>"]
fn usage_probe_the_real_rnsd_sees_home_path_and_unbuffered_only() {
    let Some(image) = live_image() else { return };
    let lbl = label();
    let secret = "probe-not-a-real-key";
    let out = docker(&[
        "run",
        "--rm",
        "--label",
        &lbl,
        "-e",
        &format!("ANTHROPIC_API_KEY={secret}"),
        &image,
        "bash",
        "-c",
        "for _ in $(seq 1 60); do (exec 3<>/dev/tcp/127.0.0.1/4242) 2>/dev/null && break; sleep 0.5; done; \
         p=$(pgrep -x rnsd | head -1); [ -n \"$p\" ] || { echo no-rnsd; exit 1; }; \
         echo environ-begin; tr '\\0' '\\n' < /proc/$p/environ | sort; echo environ-end; \
         echo \"main-sees=${ANTHROPIC_API_KEY:-unset}\"",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let environ: Vec<&str> = stdout
        .lines()
        .skip_while(|l| *l != "environ-begin")
        .skip(1)
        .take_while(|l| *l != "environ-end")
        .collect();
    let keys: Vec<&str> = environ
        .iter()
        .map(|l| l.split_once('=').map(|(k, _)| k).unwrap_or(l))
        .collect();
    assert_eq!(
        keys,
        ["HOME", "PATH", "PYTHONUNBUFFERED"],
        "rnsd's environment is exactly HOME, PATH, PYTHONUNBUFFERED:\n{stdout}"
    );
    assert!(
        environ.contains(&"HOME=/home/agent") && environ.contains(&"PYTHONUNBUFFERED=1"),
        "{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .take_while(|l| *l != "environ-end")
            .any(|l| l.contains(secret)),
        "the provider key reached rnsd:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("main-sees={secret}\n")),
        "the main command keeps the container's environment:\n{stdout}"
    );
}
