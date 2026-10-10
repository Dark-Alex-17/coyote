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
    /// script's own environment leaked through `env -i`, then idles until TERM.
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
                 trap 'printf \"signal=INT\\n\" >> \"$log\"; exit 0' INT\n\
                 while :; do sleep 0.1; done\n",
                log = log.display()
            ),
        );
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
            &["sh", "-c", "sleep 0.5; echo ran; exit 4"],
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
        &["sh", "-c", "sleep 0.5; exit 0"],
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
    let (code, _, stderr) = run(quiet.command(&[], &["sh", "-c", "sleep 0.5; exit 0"]));
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
        &["sh", "-c", "sleep 0.5; exit 0"],
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
/// nothing else of the script's environment, `/dev/null` on fd 0, is TERMed (not INTed)
/// once the main command returns, and its own stderr lines reach the script's stderr
/// raw; the main command keeps the real stdin.
///
/// stderr goes to a file rather than a pipe: were the script to `exec` the main command
/// (or forget to stop rnsd), the orphaned daemon would hold a pipe open and this test
/// would hang instead of failing. The stub's pid is recorded so an orphan is reaped.
#[test]
fn rnsd_gets_its_scoped_environment_and_a_term_after_the_main_command_returns() {
    let home = Home::new("rnsd-env");
    let stderr_path = home.path().join("script.stderr");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let mut child = home
        .command(
            &[("COYOTE_TEST_CANARY", "leaked")],
            &["sh", "-c", "sleep 0.5; cat; exit 0"],
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
    // Give a daemon the script failed to stop a moment, then reap it so nothing leaks.
    thread::sleep(Duration::from_millis(300));
    let recorded = home.rnsd_recorded();
    let orphan = recorded
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .filter(|pid| Path::new(&format!("/proc/{pid}")).exists() || cfg!(not(target_os = "linux")))
        .map(str::to_owned);
    if let Some(pid) = &orphan {
        let _ = Command::new("kill").args(["-9", pid]).output();
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
    assert!(
        !recorded.contains("signal=INT"),
        "INT must never reach rnsd:\n{recorded}"
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

/// Passthrough: `sh`, `bash` and an absolute path run as given; anything else is a
/// `coyote` argument. The script's own helpers are not on PATH here, so the coyote branch
/// is observed through a stand-in `coyote`.
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
        let name = format!(
            "coyote-image-test-{}-{}",
            std::process::id(),
            extra.len() + command.len()
        );
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
