//! Usage coverage for `scripts/docker-entrypoint.sh`, the image's PID-2 under tini, run
//! the way the container runs it: a child `sh` under a throwaway HOME with a stand-in
//! `rnsd` and `setsid` on PATH, judged by exit code, output, the rendered
//! `~/.reticulum/config` and what the stand-in daemon recorded. The only edit made to
//! the script before running it is the template path (a baked absolute path the host
//! does not have); the test asserts the original line first. Every run is bounded by a
//! 30 s watchdog, so a stop phase that never returns is a red test, not a hung lane.
//!
//! The static text pins live in `scripts_pins.rs`; `scripts/image-smoke.sh` proves the
//! process model, signals and the relay reach against a built image.
//!
//! Unix only, like the script.
#![cfg(unix)]

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
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

    /// Polls the stand-in rnsd's log for its `ready` line for up to 10 s.
    fn wait_until_rnsd_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.rnsd_recorded().contains("ready\n") {
            assert!(
                Instant::now() < deadline,
                "the stand-in rnsd did not write `ready` within 10 s:\n{}",
                self.rnsd_recorded()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const WATCHDOG: Duration = Duration::from_secs(30);

fn kill_group(pgid: u32) {
    let _ = Command::new("sh")
        .args([
            "-c",
            r#"kill -s KILL -- "-$1" 2>/dev/null"#,
            "_",
            &pgid.to_string(),
        ])
        .status();
}

/// Reads a pipe to EOF on its own thread, so the watchdog can report what the script
/// printed before it was killed.
struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
    reader: thread::JoinHandle<()>,
}

impl Capture {
    fn start(mut pipe: impl Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&bytes);
        let (tx, done) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
            let _ = tx.send(());
        });
        Capture {
            bytes,
            done,
            reader,
        }
    }

    fn so_far(&self) -> String {
        String::from_utf8_lossy(&self.bytes.lock().unwrap()).into_owned()
    }

    /// The pipe's contents once every writer has closed it. A process the script left
    /// behind (an orphaned stand-in rnsd holds the script's stderr) would keep it open,
    /// so after 3 s the script's process group is KILLed and the read ends.
    fn finish(self, pgid: u32) -> String {
        let Capture {
            bytes,
            done,
            reader,
        } = self;
        if done.recv_timeout(Duration::from_secs(3)).is_err() {
            kill_group(pgid);
        }
        let _ = reader.join();
        let bytes = bytes.lock().unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Waits for a script spawned with `process_group(0)`, `WATCHDOG` at most: on expiry
/// the whole group (script, main command, stand-in rnsd) is KILLed and the test fails
/// with `output()`'s view of what it printed.
fn wait_bounded(child: &mut Child, output: impl Fn() -> String) -> ExitStatus {
    let deadline = Instant::now() + WATCHDOG;
    loop {
        if let Some(status) = child.try_wait().expect("wait sh") {
            return status;
        }
        if Instant::now() >= deadline {
            kill_group(child.id());
            let _ = child.wait();
            panic!("entrypoint did not exit within 30 s\n{}", output());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run(mut cmd: Command) -> (i32, String, String) {
    let mut child = cmd
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let stdout = Capture::start(child.stdout.take().unwrap());
    let stderr = Capture::start(child.stderr.take().unwrap());
    let status = wait_bounded(&mut child, || {
        format!("stdout:\n{}\nstderr:\n{}", stdout.so_far(), stderr.so_far())
    });
    let pgid = child.id();
    (
        status.code().unwrap_or(-1),
        stdout.finish(pgid),
        stderr.finish(pgid),
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
        &["sh", "-c", &home.after_rnsd_ready("exit 0")],
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

/// rnsd is a sidecar: `COYOTE_MESH_RNSD=0` skips it with one stderr line, `1` is the
/// default spelled out; when its config dir cannot be created or the template is
/// missing the script warns and the main command still runs with its own status.
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

    let on = Home::new("rnsd-on");
    let (code, _, stderr) = run(on.command(
        &[("COYOTE_MESH_RNSD", "1")],
        &["sh", "-c", &on.after_rnsd_ready("exit 0")],
    ));
    assert_eq!(code, 0, "{stderr}");
    assert!(
        on.rnsd_recorded().contains("argv=-vv"),
        "COYOTE_MESH_RNSD=1 starts rnsd as the default does:\n{}",
        on.rnsd_recorded()
    );
    assert!(
        !stderr.contains("WARNING"),
        "COYOTE_MESH_RNSD=1 is a recognised value and draws no warning:\n{stderr}"
    );

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
/// The main command is `cat`, so it returns when this test closes its stdin; that
/// happens only after the stub has written `ready`, and the clock for the TERM bound
/// starts there rather than at the spawn, so a slow stub start-up cannot fail it.
#[test]
fn rnsd_gets_its_scoped_environment_and_a_term_after_the_main_command_returns() {
    let home = Home::new("rnsd-env");
    let stderr_path = home.path().join("script.stderr");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let mut child = home
        .command(
            &[("COYOTE_TEST_CANARY", "leaked")],
            &["sh", "-c", "cat; exit 0"],
        )
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr_file)
        .spawn()
        .expect("spawn sh");
    let _group = Group(child.id());
    let stdout = Capture::start(child.stdout.take().unwrap());
    home.wait_until_rnsd_ready();
    let started = Instant::now();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from-the-container-stdin\n")
        .unwrap();
    let status = wait_bounded(&mut child, || {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            stdout.so_far(),
            read(&stderr_path)
        )
    });
    let elapsed = started.elapsed();
    let stdout = stdout.finish(child.id());
    let recorded = home.rnsd_recorded();
    let stderr = read(&stderr_path);
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert_eq!(
        stdout, "from-the-container-stdin\n",
        "the main command runs in the foreground and reads the container's stdin"
    );
    assert!(
        stderr.contains("rnsd-stub: [Notice] up"),
        "rnsd's stderr reaches the script's stderr unprefixed:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the script took {elapsed:?} from the stub's `ready` line to exit; TERM must end rnsd well inside the 5 s KILL bound"
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

/// The `&` child is `env`, then `setsid`, then rnsd, and a main command can return
/// before the last exec (`sh -c 'exit 7'`, `coyote --version`). The stop phase must
/// still stop the child it started: it is identified by its parent pid, which every
/// exec keeps, not by a name it has not taken yet. Here the stand-in `setsid` sleeps
/// before exec-ing, the main command exits at once, and the script must return with
/// the main command's status promptly, leaving nothing of the child's lineage behind.
/// The TERM usually lands on the pre-exec shell, which dies of it and never starts the
/// stub; a stub that did get as far as arming its trap records the TERM instead, or is
/// KILLed by the bounded wait when the TERM was swallowed on the way.
/// The lineage is checked through the script's process group, which the stand-in
/// `setsid` never leaves.
#[test]
fn a_main_command_that_returns_before_setsid_execs_rnsd_still_stops_the_child() {
    let home = Home::new("slow-setsid");
    home.write_executable("setsid", "#!/bin/sh\nsleep 0.3\nexec \"$@\"\n");
    let mut child = home
        .command(&[], &["sh", "-c", "exit 7"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let pgid = child.id();
    let group = Group(pgid);
    let stdout = Capture::start(child.stdout.take().unwrap());
    let stderr = Capture::start(child.stderr.take().unwrap());
    let started = Instant::now();
    let status = wait_bounded(&mut child, || {
        format!("stdout:\n{}\nstderr:\n{}", stdout.so_far(), stderr.so_far())
    });
    let elapsed = started.elapsed();
    let stdout = stdout.finish(pgid);
    let stderr = stderr.finish(pgid);
    assert_eq!(
        status.code(),
        Some(7),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "the script must not wait on a child it never signalled; took {elapsed:?}"
    );
    // The shim's own `sleep 0.3` may outlive the TERM that killed its shell; a stub
    // the script failed to stop loops for good.
    let deadline = Instant::now() + Duration::from_secs(2);
    while group.has_members() {
        assert!(
            Instant::now() < deadline,
            "something of the rnsd child's lineage outlived the script by 2 s:\n{}",
            home.rnsd_recorded()
        );
        thread::sleep(Duration::from_millis(50));
    }
    drop(group);
    let recorded = home.rnsd_recorded();
    // A TERM that lands in the `&` child's first instants, before the forked shell has
    // reset the trap handler it inherited, is swallowed there; the script re-sends TERM
    // once a second, so a stub that got as far as arming its trap was TERMed, never
    // KILLed.
    assert!(
        !recorded.contains("ready\n") || recorded.contains("signal=TERM\n"),
        "a stub that armed its trap must have been TERMed:\n{recorded}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("sending KILL"),
        "the stop phase escalated to KILL:\n{stderr}"
    );
}

/// A TERM swallowed by the child before it became rnsd (the forked shell still carries
/// this shell's trap handler for an instant) must not cost the 5 s KILL escalation: the
/// stop phase re-sends TERM once a second. The shim arms a one-shot trap at once and
/// sleeps past the main command's exit, so the first TERM is absorbed; it then exec's the
/// stub, which arms its own trap; without the re-send the stub idles until the KILL and
/// the entrypoint warns.
#[test]
fn a_term_swallowed_before_the_child_became_rnsd_is_sent_again() {
    let home = Home::new("absorbed-term");
    home.write_executable(
        "setsid",
        "#!/bin/sh\ntrap 'trap - TERM' TERM\nsleep 0.5\nexec \"$@\"\n",
    );
    let mut child = home
        .command(&[], &["sh", "-c", "sleep 0.2; exit 4"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let pgid = child.id();
    let group = Group(pgid);
    let stdout = Capture::start(child.stdout.take().unwrap());
    let stderr = Capture::start(child.stderr.take().unwrap());
    let started = Instant::now();
    let status = wait_bounded(&mut child, || {
        format!("stdout:\n{}\nstderr:\n{}", stdout.so_far(), stderr.so_far())
    });
    let elapsed = started.elapsed();
    let stdout = stdout.finish(pgid);
    let stderr = stderr.finish(pgid);
    drop(group);
    assert_eq!(
        status.code(),
        Some(4),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let recorded = home.rnsd_recorded();
    assert!(
        recorded.contains("signal=TERM\n"),
        "the stub never saw the re-sent TERM:\n{recorded}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("sending KILL"),
        "the stop phase escalated to KILL instead of re-sending TERM:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the re-sent TERM must end the child well inside the 5 s KILL bound; took {elapsed:?}"
    );
}

/// Kills a whole process group on drop, so a wrapper that died early (the RED shape of
/// the signal test) cannot leave its main command looping after the test.
struct Group(u32);

impl Group {
    fn has_members(&self) -> bool {
        Command::new("sh")
            .args([
                "-c",
                r#"kill -0 -- "-$1" 2>/dev/null"#,
                "_",
                &self.0.to_string(),
            ])
            .status()
            .expect("spawn sh kill")
            .success()
    }

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
        kill_group(self.0);
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
    let status = wait_bounded(&mut child, || read(&stderr_path));
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
    let status = wait_bounded(&mut child, || read(&stderr_path));
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

/// A CRLF checkout of the template must render exactly what the LF one renders, with
/// the opt-in stanzas too: the `#@if lan` / `#@if relay` / `#@end` markers are matched
/// after the CR is stripped, so both stanzas appear under `COYOTE_MESH_LAN=1` and
/// `COYOTE_MESH_RELAY=host:port`, the relay placeholders are expanded, and the two
/// rendered files are byte-identical.
#[test]
fn usage_probe_a_crlf_template_renders_byte_identically_to_the_lf_one_with_every_stanza() {
    let lf = Home::new("crlf-parity-lf");
    let crlf = Home::new("crlf-parity-crlf");
    let template = read(crlf.template());
    assert!(!template.contains('\r'), "the checked-in template is LF");
    fs::write(crlf.template(), template.replace('\n', "\r\n")).unwrap();
    let env_vars = [
        ("COYOTE_MESH_LAN", "1"),
        ("COYOTE_MESH_RELAY", "relay.example:4242"),
    ];
    for home in [&lf, &crlf] {
        let (code, _, stderr) = run(home.command(&env_vars, &["sh", "-c", "exit 0"]));
        assert_eq!(code, 0, "{stderr}");
        assert!(!stderr.contains("WARNING"), "{stderr}");
    }
    let from_lf = read(lf.config());
    let from_crlf = read(crlf.config());
    assert_eq!(
        from_crlf, from_lf,
        "a CRLF template must render the same bytes as the LF template"
    );
    let lines = interface_lines(&from_crlf);
    for expected in [
        "[[Coyote Sessions]]",
        "type = TCPServerInterface",
        "type = AutoInterface",
        "[[Team Relay]]",
        "type = TCPClientInterface",
        "target_host = relay.example",
        "target_port = 4242",
    ] {
        assert!(
            lines.iter().any(|l| l == expected),
            "`{expected}` must be rendered from the CRLF template:\n{from_crlf}"
        );
    }
    for absent in ["#@", "@RELAY_HOST@", "@RELAY_PORT@", "\r"] {
        assert!(
            !from_crlf.contains(absent),
            "`{}` must not reach the rendered config:\n{from_crlf}",
            absent.escape_debug()
        );
    }
}

/// The trap is armed before rnsd is spawned, so a TERM that lands on the wrapper during
/// the spawn itself (here the stand-in `setsid` sends it to its parent before exec-ing
/// rnsd) neither kills the wrapper nor leaks to the main command: the main command runs
/// to its own end, its status is the script's, and rnsd is still TERMed afterwards.
///
/// The parent's few builtins between the `&` and the old trap site always beat the
/// child's exec chain, so this pins the trap being armed by then rather than the order
/// of the two lines: it goes red only when the trap is missing altogether.
#[test]
fn usage_probe_a_term_landing_during_the_rnsd_spawn_leaves_the_wrapper_alive() {
    let home = Home::new("term-at-spawn");
    home.write_executable("setsid", "#!/bin/sh\nkill -s TERM \"$PPID\"\nexec \"$@\"\n");
    let stderr_path = home.path().join("spawn.stderr");
    let mut child = home
        .command(&[], &["sh", "-c", "sleep 0.5; echo main-finished; exit 7"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("spawn sh");
    let group = Group(child.id());
    let stdout = Capture::start(child.stdout.take().unwrap());
    let status = wait_bounded(&mut child, || {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            stdout.so_far(),
            read(&stderr_path)
        )
    });
    let stdout = stdout.finish(child.id());
    drop(group);
    let stderr = read(&stderr_path);
    assert_eq!(
        status.code(),
        Some(7),
        "a TERM during the rnsd spawn must not end the wrapper; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        stdout, "main-finished\n",
        "the main command ran to its own end"
    );
    let recorded = home.rnsd_recorded();
    assert!(
        recorded.contains("ready\n") && recorded.contains("signal=TERM\n"),
        "rnsd started under the TERMed wrapper and was TERMed after the main command:\n{recorded}"
    );
}

/// The stop phase decides "ours" by parent pid, not by `/proc/<pid>/comm`: the `&`
/// child is `env`, then `setsid`, and only then rnsd, so a comm check would disown a
/// live child whose exec chain the main command outran. Here the stand-in `setsid`
/// exec-s a looping `tool-child` in rnsd's place, so the child never reads `rnsd` at
/// all, and it still receives the TERM once the main command returns; the wrapper
/// exits with the main command's status inside the bounded wait. (A reused pid after
/// dash reaped an early-dead daemon has another parent and is left alone.)
#[cfg(target_os = "linux")]
#[test]
fn usage_probe_a_child_that_has_not_yet_become_rnsd_is_still_ours_and_is_stopped() {
    let home = Home::new("not-yet-rnsd");
    let log = home.rnsd_log();
    home.write_executable(
        "tool-child",
        &format!(
            "#!/bin/sh\nprintf 'comm=%s\\n' \"$(cat /proc/$$/comm)\" >> {log}\n\
             trap 'printf \"signal=TERM\\n\" >> {log}; exit 0' TERM\n\
             printf 'ready\\n' >> {log}\nwhile :; do sleep 0.1; done\n",
            log = log.display()
        ),
    );
    let tool_child = home.path().join("bin").join("tool-child");
    home.write_executable(
        "setsid",
        &format!("#!/bin/sh\nexec {}\n", tool_child.display()),
    );
    let started = Instant::now();
    let (code, _, stderr) = run(home.command(&[], &["sh", "-c", &home.after_rnsd_ready("exit 8")]));
    let elapsed = started.elapsed();
    let recorded = home.rnsd_recorded();
    assert_eq!(code, 8, "{stderr}");
    assert!(
        recorded.contains("comm=tool-child\n"),
        "the fixture must put a child not named rnsd at rnsd's pid:\n{recorded}"
    );
    assert!(
        recorded.contains("signal=TERM\n"),
        "a child that has not become rnsd is still ours and must be TERMed:\n{recorded}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "TERM ends the child well inside the 5 s KILL bound; took {elapsed:?}"
    );
}

/// The stop phase's "ours" test on its own, with the shell as the parent: `rnsd_alive`
/// says yes only for a live process whose parent is the calling shell, whatever that
/// process is called — a plain child as much as one whose comm carries `) ` and spaces
/// (`/proc/<pid>/stat` reads `pid (comm) S ppid …`, so the comm has to be stripped
/// through its last `) `) — and no for an empty pid, for a live pid with another parent
/// (the shape of a pid reused after dash reaped an early-dead daemon: here the test
/// process itself) and for a child already reaped.
#[cfg(target_os = "linux")]
#[test]
fn usage_probe_the_stop_phase_owns_a_live_child_of_any_name_and_nothing_else() {
    let home = Home::new("ours");
    let script = read(home.script());
    let function: Vec<&str> = script
        .lines()
        .skip_while(|line| *line != "rnsd_alive() {")
        .take_while(|line| *line != "}")
        .chain(std::iter::once("}"))
        .collect();
    assert!(
        function.len() > 2,
        "docker-entrypoint.sh must define `rnsd_alive() {{` … `}}` at column 0; the test runs that function alone"
    );
    let odd = home.write_executable(
        "r) S 1 x",
        "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 0.1; done\n",
    );
    let matrix = format!(
        "{function}\n\
         rnsd_pid=; rnsd_alive && echo empty=ours || echo empty=not\n\
         rnsd_pid=$PPID; rnsd_alive && echo foreign=ours || echo foreign=not\n\
         sleep 30 & rnsd_pid=$!; rnsd_alive && echo child=ours || echo child=not; kill $rnsd_pid\n\
         '{odd}' & rnsd_pid=$!; sleep 0.2; printf 'comm=%s\\n' \"$(cat /proc/$rnsd_pid/comm)\"; \
         rnsd_alive && echo odd=ours || echo odd=not; kill $rnsd_pid\n\
         sleep 0.01 & rnsd_pid=$!; wait $rnsd_pid; rnsd_alive && echo reaped=ours || echo reaped=not\n",
        function = function.join("\n"),
        odd = odd.display()
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(&matrix);
    let (code, stdout, stderr) = run(cmd);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "empty=not",
            "foreign=not",
            "child=ours",
            "comm=r) S 1 x",
            "odd=ours",
            "reaped=not",
        ],
        "rnsd_alive must own exactly the live children of the calling shell:\nstderr:\n{stderr}"
    );
}

/// The same, through the whole script: the stand-in `setsid` exec-s a stub whose comm is
/// `r) S 1 x` in rnsd's place, and the stop phase still recognises it as the child it
/// started, TERMs it once the main command returns and exits with the main command's
/// status inside the bounded wait. A stop phase that cut the stat line at the first `)`
/// would read `1` as the parent, disown the child and leave it looping.
#[cfg(target_os = "linux")]
#[test]
fn usage_probe_a_child_whose_name_carries_a_paren_and_spaces_is_still_stopped() {
    let home = Home::new("odd-comm");
    let log = home.rnsd_log();
    let odd = home.write_executable(
        "r) S 1 x",
        &format!(
            "#!/bin/sh\nprintf 'comm=%s\\n' \"$(cat /proc/$$/comm)\" >> '{log}'\n\
             trap 'printf \"signal=TERM\\n\" >> \"{log}\"; exit 0' TERM\n\
             printf 'ready\\n' >> '{log}'\nwhile :; do sleep 0.1; done\n",
            log = log.display()
        ),
    );
    home.write_executable("setsid", &format!("#!/bin/sh\nexec '{}'\n", odd.display()));
    let started = Instant::now();
    let (code, _, stderr) = run(home.command(&[], &["sh", "-c", &home.after_rnsd_ready("exit 8")]));
    let elapsed = started.elapsed();
    let recorded = home.rnsd_recorded();
    assert_eq!(code, 8, "{stderr}");
    assert!(
        recorded.contains("comm=r) S 1 x\n"),
        "the fixture must put a child with `) ` in its comm at rnsd's pid:\n{recorded}"
    );
    assert!(
        recorded.contains("signal=TERM\n"),
        "a child of ours is TERMed whatever its comm reads:\n{recorded}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "TERM ends the child well inside the 5 s KILL bound; took {elapsed:?}"
    );
}
