//! Usage coverage for `scripts/mesh-relay.sh`, run the way an operator runs it: a
//! child `bash` under a throwaway HOME, judged by exit code, output and what lands on
//! disk. Nothing here installs rnsd or touches a service manager: either the dry run
//! is used, or a stand-in `rnsd` is seeded into `BIN_DIR` so the script takes its
//! reuse path and only the config step runs. The static text pins live in
//! `scripts_pins.rs`; the install ladder and the service managers are exercised by
//! the `scripts` CI job and by hand on a VM.
//!
//! Unix only, like the script. Without `bash` on PATH the tests print `skipping:`.
#![cfg(unix)]

use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn script() -> PathBuf {
    repo_root().join("scripts").join("mesh-relay.sh")
}

/// `None` when `bash` is not on PATH; the caller prints `skipping:` and returns.
fn bash() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .find(|candidate| candidate.is_file())
}

/// Resolve `name` on PATH, for building a PATH of hand-picked tools.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn running_as_root() -> bool {
    static ROOT: OnceLock<bool> = OnceLock::new();
    *ROOT.get_or_init(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "0")
            .unwrap_or(false)
    })
}

/// A throwaway HOME. `XDG_CONFIG_HOME` and `BIN_DIR` sit inside it so every path the
/// script may write is under one root that is removed when the fixture drops.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-mesh-relay-{label}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        Home { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    fn config_home(&self) -> PathBuf {
        self.root.join(".config")
    }

    fn reticulum_config(&self) -> PathBuf {
        self.root.join(".reticulum").join("config")
    }

    /// A stand-in `rnsd` whose `--version` succeeds, so the script reuses it instead
    /// of installing anything.
    fn seed_rnsd(&self) -> PathBuf {
        let bin = self.bin_dir();
        fs::create_dir_all(&bin).unwrap();
        let rnsd = bin.join("rnsd");
        fs::write(&rnsd, "#!/bin/sh\necho 'rnsd 1.5.2'\n").unwrap();
        fs::set_permissions(&rnsd, fs::Permissions::from_mode(0o755)).unwrap();
        rnsd
    }

    /// The script, with `--allow-root` appended when the test process is root so a
    /// root-run CI container exercises the same paths.
    fn relay(&self, bash: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(bash);
        cmd.arg(script())
            .args(args)
            .current_dir(repo_root())
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.config_home())
            .env("BIN_DIR", self.bin_dir())
            .env_remove("COYOTE_CONFIG_DIR");
        if running_as_root() {
            cmd.arg("--allow-root");
        }
        cmd
    }

    /// Everything under the HOME, relative, sorted, so a "wrote nothing" check can
    /// name what appeared.
    fn entries(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            if let Ok(read) = fs::read_dir(dir) {
                for entry in read.flatten() {
                    let path = entry.path();
                    out.push(path.strip_prefix(root).unwrap().display().to_string());
                    if path.is_dir() {
                        walk(&path, root, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out);
        out.sort();
        out
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A directory of hand-picked tools for a PATH that holds only what a scenario
/// needs: real coreutils by symlink, stand-ins written as `#!/bin/sh` scripts.
struct Tools {
    dir: PathBuf,
}

impl Tools {
    /// The coreutils every run of the script reaches for, so a scenario only adds
    /// the installers it wants the ladder to see.
    const BASE: [&'static str; 17] = [
        "sh", "uname", "id", "mktemp", "mkdir", "rm", "rmdir", "dirname", "basename", "chmod",
        "mv", "cp", "ln", "cat", "head", "grep", "sleep",
    ];

    fn new(home: &Home) -> Self {
        let dir = home.path().join("tools");
        fs::create_dir_all(&dir).unwrap();
        for name in Self::BASE {
            if let Some(real) = on_path(name) {
                symlink(real, dir.join(name)).unwrap();
            }
        }
        Tools { dir }
    }

    /// A stand-in `name` that appends its arguments to `log` and then runs `body`.
    fn fake(&self, name: &str, log: &Path, body: &str) -> &Self {
        let path = self.dir.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"{name} $*\" >> \"{log}\"\n{body}\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        self
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

/// What the stand-ins recorded, one invocation per line.
fn recorded(log: &Path) -> String {
    fs::read_to_string(log).unwrap_or_default()
}

/// Files directly under `dir`, sorted, so a "no temp file left behind" check can
/// name the stray.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn run(cmd: &mut Command) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = cmd.output().expect("spawn bash");
    (
        status.code().unwrap_or(-1),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

macro_rules! bash_or_skip {
    () => {
        match bash() {
            Some(bash) => bash,
            None => {
                eprintln!("skipping: no `bash` on PATH");
                return;
            }
        }
    };
}

#[test]
fn the_dry_run_prints_the_whole_plan_and_creates_nothing() {
    let bash = bash_or_skip!();
    let home = Home::new("dry-run");

    let (code, out, err) = run(&mut home.relay(&bash, &["--no-service", "--dry-run"]));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.is_empty(),
        "a clean dry run keeps stderr empty, got:\n{err}"
    );
    for needle in [
        "[reticulum]",
        "enable_transport = True",
        "[logging]",
        "loglevel = 4",
        "[[Coyote Local]]",
        "type = AutoInterface",
        "[[Coyote Sessions]]",
        "type = TCPServerInterface",
        "listen_ip = 127.0.0.1",
        "listen_port = 4242",
        "ingress_control = No",
        "Service: skipped (--no-service)",
        "Firewall:",
        "mesh:\n  interfaces:\n    - type: private\n      host: 127.0.0.1\n      port: 4242",
        ".mesh on",
    ] {
        assert!(out.contains(needle), "dry run lacks {needle:?}:\n{out}");
    }
    assert!(
        !out.contains("share_instance"),
        "the planned config must leave the shared instance at its default:\n{out}"
    );
    assert!(
        !out.contains("[[Team Relay]]"),
        "no relay was asked for:\n{out}"
    );
    assert_eq!(
        home.entries(),
        Vec::<String>::new(),
        "the dry run created files"
    );

    let (code, out, _) = run(&mut home.relay(
        &bash,
        &["--no-service", "--dry-run", "--relay", "relay.example:4242"],
    ));
    assert_eq!(code, 0);
    for needle in [
        "[[Team Relay]]",
        "type = TCPClientInterface",
        "target_host = relay.example",
        "target_port = 4242",
    ] {
        assert!(
            out.contains(needle),
            "relay dry run lacks {needle:?}:\n{out}"
        );
    }
    assert_eq!(
        home.entries(),
        Vec::<String>::new(),
        "the relay dry run created files"
    );
}

#[test]
fn usage_errors_exit_1_on_stderr_and_touch_nothing_while_help_exits_0() {
    let bash = bash_or_skip!();
    let home = Home::new("usage");

    let bad: [&[&str]; 10] = [
        &["--bogus"],
        &["stray"],
        &["--relay"],
        &["--relay", "nonsense"],
        &["--relay", "host:"],
        &["--relay", ":4242"],
        &["--relay", "host:99999"],
        &["--relay", "host:abc"],
        &["--version"],
        &["--bin-dir"],
    ];
    for args in bad {
        let mut full = vec!["--no-service", "--dry-run"];
        full.extend_from_slice(args);
        let (code, out, err) = run(&mut home.relay(&bash, &full));
        assert_eq!(
            code, 1,
            "{args:?} should be a usage error; stdout:\n{out}\nstderr:\n{err}"
        );
        assert!(
            err.contains("[coyote-mesh] Error:"),
            "{args:?} must explain itself on stderr, got:\n{err}"
        );
        assert!(
            out.is_empty(),
            "{args:?} must keep stdout clean, got:\n{out}"
        );
    }
    assert_eq!(
        home.entries(),
        Vec::<String>::new(),
        "usage errors created files"
    );

    let (code, out, err) = run(&mut home.relay(&bash, &["--help"]));
    assert_eq!(code, 0, "stderr:\n{err}");
    assert!(
        err.is_empty(),
        "--help writes to stdout only, got stderr:\n{err}"
    );
    for flag in [
        "--relay",
        "--version",
        "--bin-dir",
        "--no-service",
        "--dry-run",
        "--allow-root",
        "--help",
    ] {
        assert!(out.contains(flag), "--help does not list {flag}:\n{out}");
    }
}

#[test]
fn a_present_rnsd_is_reused_and_the_config_is_written_once_owner_only() {
    let bash = bash_or_skip!();
    let home = Home::new("reuse");
    let rnsd = home.seed_rnsd();

    let (code, out, err) = run(&mut home.relay(&bash, &["--no-service"]));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("rnsd already installed"),
        "a working rnsd in BIN_DIR is reused, not reinstalled:\n{out}"
    );
    assert!(
        !out.contains("Installing"),
        "nothing is installed when rnsd already works:\n{out}"
    );
    assert!(
        out.contains(&format!("Wrote {}", home.reticulum_config().display())),
        "{out}"
    );
    assert_eq!(
        fs::metadata(&rnsd).unwrap().len(),
        "#!/bin/sh\necho 'rnsd 1.5.2'\n".len() as u64,
        "the seeded rnsd was replaced"
    );

    let config = home.reticulum_config();
    let text = fs::read_to_string(&config).unwrap();
    assert_eq!(
        mode_of(&config),
        0o600,
        "the config holds the daemon's settings and is owner-only"
    );
    assert_eq!(
        mode_of(config.parent().unwrap()),
        0o700,
        "~/.reticulum is owner-only (umask 077)"
    );
    for needle in [
        "[reticulum]",
        "enable_transport = True",
        "[logging]",
        "loglevel = 4",
        "[[Coyote Local]]",
        "type = AutoInterface",
        "[[Coyote Sessions]]",
        "listen_ip = 127.0.0.1",
        "listen_port = 4242",
        "ingress_control = No",
    ] {
        assert!(text.contains(needle), "config lacks {needle:?}:\n{text}");
    }
    assert!(!text.contains("share_instance"), "{text}");
    assert!(!text.contains("[[Team Relay]]"), "{text}");
    let siblings: Vec<_> = fs::read_dir(config.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(
        siblings,
        vec![std::ffi::OsString::from("config")],
        "the atomic write left a temp file behind"
    );
    for rc in [".bashrc", ".profile", ".zshrc", ".bash_profile"] {
        assert!(
            !home.path().join(rc).exists(),
            "{rc} was created: rc files are never edited"
        );
    }

    let before = fs::read(&config).unwrap();
    let (code, out, err) = run(&mut home.relay(&bash, &["--no-service"]));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("already exists, not touched"), "{out}");
    assert_eq!(
        fs::read(&config).unwrap(),
        before,
        "the second run rewrote the config"
    );
}

#[test]
fn a_relay_lands_in_a_fresh_config_and_an_existing_config_is_only_printed_around() {
    let bash = bash_or_skip!();
    let home = Home::new("relay");
    home.seed_rnsd();

    let (code, out, err) =
        run(&mut home.relay(&bash, &["--no-service", "--relay", "relay.example:4242"]));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let config = home.reticulum_config();
    let text = fs::read_to_string(&config).unwrap();
    for needle in [
        "[[Team Relay]]",
        "type = TCPClientInterface",
        "target_host = relay.example",
        "target_port = 4242",
    ] {
        assert!(
            text.contains(needle),
            "fresh config lacks {needle:?}:\n{text}"
        );
    }

    // A later run with a different relay prints the stanza but leaves the file alone.
    let before = fs::read(&config).unwrap();
    let (code, out, _) =
        run(&mut home.relay(&bash, &["--no-service", "--relay", "other.example:5000"]));
    assert_eq!(code, 0);
    assert!(out.contains("already exists, not touched"), "{out}");
    assert!(
        out.contains("target_host = other.example") && out.contains("target_port = 5000"),
        "{out}"
    );
    assert_eq!(
        fs::read(&config).unwrap(),
        before,
        "an existing config was modified"
    );

    // A hand-written config the script knows nothing about gets the same treatment.
    let other = Home::new("relay-existing");
    other.seed_rnsd();
    fs::create_dir_all(other.reticulum_config().parent().unwrap()).unwrap();
    fs::write(other.reticulum_config(), "custom\n").unwrap();
    let (code, out, _) =
        run(&mut other.relay(&bash, &["--no-service", "--relay", "relay.example:4242"]));
    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(other.reticulum_config()).unwrap(),
        "custom\n"
    );
    assert!(
        out.contains("[[Team Relay]]") && out.contains("[[Coyote Sessions]]"),
        "{out}"
    );
}

#[test]
fn the_bin_dir_flag_beats_the_environment_and_relative_dirs_resolve_against_the_cwd() {
    let bash = bash_or_skip!();
    let home = Home::new("bin-dir");

    let (code, out, _) = run(&mut home.relay(&bash, &["--no-service", "--dry-run"]));
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!("BIN_DIR: {}", home.bin_dir().display())),
        "{out}"
    );

    let flagged = home.path().join("elsewhere");
    let flagged = flagged.to_str().unwrap();
    let (code, out, _) =
        run(&mut home.relay(&bash, &["--no-service", "--dry-run", "--bin-dir", flagged]));
    assert_eq!(code, 0);
    assert!(out.contains(&format!("BIN_DIR: {flagged}")), "{out}");

    let (code, out, _) = run(&mut home.relay(
        &bash,
        &["--no-service", "--dry-run", "--bin-dir", "./rel/bin"],
    ));
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!(
            "BIN_DIR: {}",
            repo_root().join("rel/bin").display()
        )),
        "{out}"
    );
    assert_eq!(home.entries(), Vec::<String>::new());
}

#[test]
fn without_uv_pipx_or_a_new_enough_python_it_exits_2_before_touching_the_home() {
    let bash = bash_or_skip!();
    let home = Home::new("no-python");

    // A PATH holding only what the script needs to reach its install decision, plus a
    // python whose version probe fails the way a 3.8 interpreter does.
    let tools = home.path().join("tools");
    fs::create_dir_all(&tools).unwrap();
    for name in ["uname", "id", "mktemp", "sh"] {
        let real = on_path(name).unwrap_or_else(|| panic!("{name} on PATH"));
        symlink(real, tools.join(name)).unwrap();
    }
    for name in ["python3", "python"] {
        let fake = tools.join(name);
        fs::write(
            &fake,
            "#!/bin/sh\ncase \"$*\" in *version_info*) exit 1;; esac\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let home_dir = home.path().to_path_buf();
    let (code, out, err) = run(home.relay(&bash, &["--no-service"]).env("PATH", &tools));
    assert_eq!(code, 2, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.contains("python3 >= 3.9"),
        "the remedy names the python floor:\n{err}"
    );
    assert!(
        !home_dir.join(".reticulum").exists(),
        "no config is written when rnsd cannot be installed"
    );
    assert!(!home.config_home().exists(), "no venv directory is started");
    assert_eq!(
        home.entries(),
        vec![
            "tools".to_string(),
            "tools/id".to_string(),
            "tools/mktemp".to_string(),
            "tools/python".to_string(),
            "tools/python3".to_string(),
            "tools/sh".to_string(),
            "tools/uname".to_string()
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_linux_dry_run_plans_a_user_unit_with_the_absolute_rnsd_and_writes_nothing() {
    let bash = bash_or_skip!();
    let home = Home::new("systemd");

    let (code, out, err) = run(&mut home.relay(&bash, &["--dry-run"]));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let unit = home.config_home().join("systemd/user/coyote-rnsd.service");
    for needle in [
        &format!("ExecStart={}/rnsd", home.bin_dir().display()),
        "Environment=PYTHONUNBUFFERED=1",
        "Restart=on-failure",
        "RestartSec=5",
        "WantedBy=default.target",
        "enable-linger",
        "journalctl --user -u coyote-rnsd",
        &unit.display().to_string(),
    ] {
        assert!(out.contains(needle), "plan lacks {needle:?}:\n{out}");
    }
    assert!(
        !out.contains("ExecStart=rnsd"),
        "the unit must not rely on systemd's fixed search path:\n{out}"
    );
    assert_eq!(
        home.entries(),
        Vec::<String>::new(),
        "the service dry run created files"
    );
}

#[test]
fn a_darwin_dry_run_plans_a_launch_agent_logging_to_the_library_and_writes_nothing() {
    let bash = bash_or_skip!();
    let home = Home::new("launchd");

    // `uname -s` answers Darwin; everything else runs the real uname.
    let real_uname = on_path("uname").expect("uname on PATH");
    let shim_dir = home.path().join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("uname");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nif [ \"$1\" = -s ]; then echo Darwin; else exec \"{}\" \"$@\"; fi\n",
            real_uname.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path = env::join_paths(
        std::iter::once(shim_dir.clone()).chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();

    let (code, out, err) = run(home.relay(&bash, &["--dry-run"]).env("PATH", path));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("OS: darwin"), "{out}");
    let log = home
        .path()
        .join("Library/Logs/coyote-rnsd.log")
        .display()
        .to_string();
    let plist = home
        .path()
        .join("Library/LaunchAgents/com.coyote.rnsd.plist")
        .display()
        .to_string();
    for needle in [
        "<key>Label</key>",
        "<string>com.coyote.rnsd</string>",
        &format!("<string>{}/rnsd</string>", home.bin_dir().display()),
        "<key>RunAtLoad</key>",
        "<key>KeepAlive</key>",
        "<key>EnvironmentVariables</key>",
        "<key>PYTHONUNBUFFERED</key>",
        "<key>StandardOutPath</key>",
        "<key>StandardErrorPath</key>",
        &plist,
        "launchctl bootstrap gui/",
    ] {
        assert!(out.contains(needle), "plan lacks {needle:?}:\n{out}");
    }
    assert_eq!(
        out.matches(&format!("<string>{log}</string>")).count(),
        2,
        "stdout and stderr both go to the one log:\n{out}"
    );
    let shim_entries = ["shim".to_string(), "shim/uname".to_string()];
    assert_eq!(
        home.entries(),
        shim_entries,
        "the launchd dry run created files"
    );
}

#[test]
fn root_is_a_usage_error_unless_allowed() {
    let bash = bash_or_skip!();
    if !running_as_root() {
        eprintln!("skipping: not running as root; the CI smoke lane covers the refusal under sudo");
        return;
    }
    let home = Home::new("root");
    let mut cmd = Command::new(&bash);
    cmd.arg(script())
        .args(["--no-service", "--dry-run"])
        .current_dir(repo_root())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.config_home())
        .env("BIN_DIR", home.bin_dir())
        .env_remove("COYOTE_CONFIG_DIR");
    let (code, out, err) = run(&mut cmd);
    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("--allow-root"), "{err}");
    assert!(out.is_empty(), "{out}");
    assert_eq!(home.entries(), Vec::<String>::new());
}

/// The venv rung, hermetically: a stand-in `python3` that passes the version probe,
/// creates a venv on `-m venv`, and drops an `rnsd` into it on `-m pip install`.
/// Pins where the venv lives, what the shim is, which spec pip was asked for, and
/// that a second run reuses the shim without reinstalling.
#[test]
fn without_uv_or_pipx_a_venv_lands_under_the_config_dir_behind_an_exec_shim() {
    let bash = bash_or_skip!();
    let home = Home::new("venv");
    let log = home.path().join("calls.log");
    let tools = Tools::new(&home);
    // The copy placed at <venv>/bin/python serves the pip step, so the body keys on
    // the subcommand, never on which copy is running.
    tools.fake(
        "python3",
        &log,
        r#"case "$1 $2" in
  "-c "*) exit 0 ;;
  "-m venv") d="$3"; mkdir -p "$d/bin"; cp "$0" "$d/bin/python"; chmod 755 "$d/bin/python"; exit 0 ;;
  "-m pip") printf '#!/bin/sh\necho rnsd 9.9.9\n' > "$(dirname "$0")/rnsd"; chmod 755 "$(dirname "$0")/rnsd"; exit 0 ;;
esac
exit 1"#,
    );

    let (code, out, err) = run(home
        .relay(&bash, &["--no-service", "--dry-run", "--version", "0.9.6"])
        .env("PATH", tools.path()));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let venv = home.config_home().join("coyote/mesh/rns-venv");
    assert!(
        out.contains(&format!(
            "Install: {}/python3 -m venv \"{}\" && pip install \"rns==0.9.6\"",
            tools.path().display(),
            venv.display()
        )),
        "the plan names the venv rung, the venv path and the overridden spec:\n{out}"
    );
    assert!(!venv.exists(), "a dry run started the venv");
    assert_eq!(
        recorded(&log).matches("python3 -m").count(),
        0,
        "{}",
        recorded(&log)
    );

    let (code, out, err) = run(home
        .relay(&bash, &["--no-service", "--version", "0.9.6"])
        .env("PATH", tools.path()));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(venv.join("bin/rnsd").is_file(), "no rnsd in the venv");
    let calls = recorded(&log);
    assert!(
        calls.contains(&format!("python3 -m venv {}", venv.display())),
        "{calls}"
    );
    assert!(
        calls.contains("python3 -m pip install --quiet rns==0.9.6"),
        "pip must be asked for exactly the pinned-or-overridden spec:\n{calls}"
    );
    let shim = home.bin_dir().join("rnsd");
    assert!(
        shim.is_file() && !shim.symlink_metadata().unwrap().file_type().is_symlink(),
        "the shim is a regular file, not a link into the venv"
    );
    assert_eq!(mode_of(&shim), 0o755);
    assert_eq!(
        fs::read_to_string(&shim).unwrap(),
        format!("#!/bin/sh\nexec \"{}/bin/rnsd\" \"$@\"\n", venv.display()),
        "the shim execs the venv's rnsd with the caller's arguments"
    );
    assert_eq!(
        names_in(&home.bin_dir()),
        vec!["rnsd".to_string()],
        "the atomic shim write left a temp file in BIN_DIR"
    );
    assert!(
        out.contains("rnsd ready:") && out.contains("(rnsd 9.9.9)"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "Note: {} is not in PATH",
            home.bin_dir().display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "export PATH=\"{}:$PATH\"",
            home.bin_dir().display()
        )),
        "{out}"
    );
    assert!(home.reticulum_config().is_file());

    let shim_before = fs::read(&shim).unwrap();
    let calls_before = recorded(&log);
    let (code, out, _) = run(home
        .relay(&bash, &["--no-service"])
        .env("PATH", tools.path()));
    assert_eq!(code, 0);
    assert!(out.contains("rnsd already installed"), "{out}");
    assert_eq!(
        fs::read(&shim).unwrap(),
        shim_before,
        "the second run rewrote the shim"
    );
    assert_eq!(
        recorded(&log),
        calls_before,
        "the second run must not call python again: the working shim is reused"
    );
}

/// The pipx rung, hermetically, and the ladder's order above it: pipx is used only
/// when uv is absent, `pipx ensurepath` is printed and never run, and a failing uv
/// stops the run at exit 2 before any config is written.
#[test]
fn pipx_is_the_second_rung_its_ensurepath_is_only_printed_and_a_failing_uv_exits_2() {
    let bash = bash_or_skip!();
    let home = Home::new("pipx");
    let log = home.path().join("calls.log");
    let tools = Tools::new(&home);
    let pipx_bin = home.path().join(".local/bin");
    tools.fake(
        "pipx",
        &log,
        &format!(
            r#"case "$1" in
  environment) echo "{bin}"; exit 0 ;;
  install) mkdir -p "{bin}"; printf '#!/bin/sh\necho rnsd 1.5.2\n' > "{bin}/rnsd"; chmod 755 "{bin}/rnsd"; exit 0 ;;
esac
exit 1"#,
            bin = pipx_bin.display()
        ),
    );

    let (code, out, _) = run(home
        .relay(&bash, &["--no-service", "--dry-run"])
        .env("PATH", tools.path()));
    assert_eq!(code, 0);
    assert!(
        out.contains("Install: pipx install \"rns==1.5.2\""),
        "{out}"
    );

    let (code, out, err) = run(home
        .relay(&bash, &["--no-service"])
        .env("PATH", tools.path()));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let calls = recorded(&log);
    assert!(calls.contains("pipx install rns==1.5.2"), "{calls}");
    assert!(
        !calls.contains("ensurepath"),
        "pipx ensurepath edits rc files and must only be suggested:\n{calls}"
    );
    assert!(
        out.contains("'pipx ensurepath'"),
        "the hint is printed:\n{out}"
    );
    let shim = home.bin_dir().join("rnsd");
    assert_eq!(
        fs::read_link(&shim).unwrap(),
        pipx_bin.join("rnsd"),
        "BIN_DIR/rnsd links to the rnsd pipx installed"
    );
    assert!(out.contains("rnsd ready:"), "{out}");
    for rc in [".bashrc", ".profile", ".zshrc", ".bash_profile"] {
        assert!(!home.path().join(rc).exists(), "{rc} was created");
    }

    // uv first: with both present the plan names uv, and when uv then fails the run
    // stops at exit 2 with nothing written, so a half-install never gets a config.
    let other = Home::new("uv-fails");
    let other_log = other.path().join("calls.log");
    let other_tools = Tools::new(&other);
    other_tools
        .fake("pipx", &other_log, "exit 1")
        .fake("uv", &other_log, "exit 1");
    let (code, out, _) = run(other
        .relay(&bash, &["--no-service", "--dry-run"])
        .env("PATH", other_tools.path()));
    assert_eq!(code, 0);
    assert!(
        out.contains("Install: uv tool install --python 3.12 \"rns==1.5.2\""),
        "uv outranks pipx:\n{out}"
    );
    let (code, out, err) = run(other
        .relay(&bash, &["--no-service"])
        .env("PATH", other_tools.path()));
    assert_eq!(code, 2, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("uv tool install failed"), "{err}");
    assert!(
        recorded(&other_log).contains("uv tool install --python 3.12 rns==1.5.2"),
        "{}",
        recorded(&other_log)
    );
    assert!(
        !other.reticulum_config().exists(),
        "a failed install must not be followed by the config write"
    );
    assert!(!other.bin_dir().exists(), "BIN_DIR was created for nothing");
    assert!(!out.contains("Wrote"), "{out}");
}

/// A real Linux run on a host without a user session bus (ssh into a headless box,
/// a container): the unit is written where `systemctl --user` will find it, the
/// enable command and linger hint are printed, nothing waits for a port, exit 0,
/// and a second run finds the unit already present and changes nothing.
#[cfg(target_os = "linux")]
#[test]
fn without_a_user_bus_the_unit_is_written_the_enable_command_printed_and_nothing_waited_for() {
    let bash = bash_or_skip!();
    let home = Home::new("no-bus");
    home.seed_rnsd();
    let log = home.path().join("calls.log");
    let tools = Tools::new(&home);
    tools.fake("systemctl", &log, "exit 1");

    let started = std::time::Instant::now();
    let (code, out, err) = run(home.relay(&bash, &[]).env("PATH", tools.path()));
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "no service was started, so nothing may wait for the port (took {elapsed:?})"
    );

    let unit = home.config_home().join("systemd/user/coyote-rnsd.service");
    let expected = format!(
        "[Unit]\nDescription=Reticulum daemon for Coyote mesh\nAfter=network.target\n\n[Service]\nExecStart={}/rnsd\nEnvironment=PYTHONUNBUFFERED=1\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        home.bin_dir().display()
    );
    assert_eq!(fs::read_to_string(&unit).unwrap(), expected);
    assert_eq!(mode_of(&unit), 0o644);
    assert_eq!(
        names_in(unit.parent().unwrap()),
        vec!["coyote-rnsd.service".to_string()],
        "the atomic unit write left a temp file behind"
    );
    for needle in [
        &format!("Wrote {}", unit.display()),
        "No user session bus",
        "systemctl --user daemon-reload && systemctl --user enable --now coyote-rnsd",
        "enable-linger",
        "journalctl --user -u coyote-rnsd",
        "Firewall:",
    ] {
        assert!(out.contains(needle), "run lacks {needle:?}:\n{out}");
    }
    assert!(!out.contains("Waiting for rnsd"), "{out}");
    let calls = recorded(&log);
    assert!(
        !calls.contains("daemon-reload") && !calls.contains("enable"),
        "with no bus, systemctl is only probed, never asked to act:\n{calls}"
    );

    let before = fs::read(&unit).unwrap();
    let mtime = fs::metadata(&unit).unwrap().modified().unwrap();
    let (code, out, _) = run(home.relay(&bash, &[]).env("PATH", tools.path()));
    assert_eq!(code, 0);
    assert!(
        out.contains(&format!(
            "{} already present with this content",
            unit.display()
        )),
        "{out}"
    );
    assert_eq!(fs::read(&unit).unwrap(), before);
    assert_eq!(
        fs::metadata(&unit).unwrap().modified().unwrap(),
        mtime,
        "the second run rewrote an identical unit"
    );
    assert!(out.contains("already exists, not touched"), "{out}");

    // The dry run against a present, identical unit says so and writes nothing new.
    let entries = home.entries();
    let (code, out, _) = run(home.relay(&bash, &["--dry-run"]).env("PATH", tools.path()));
    assert_eq!(code, 0);
    assert!(out.contains("already present with this content"), "{out}");
    assert!(out.contains("No user session bus here"), "{out}");
    assert_eq!(home.entries(), entries);
}

/// The launchd plan is a property list a Mac will load: parsed with plistlib, it
/// carries the agent contract key for key, with the daemon's stdout and stderr on
/// the one log file.
#[test]
fn the_darwin_plist_parses_and_carries_the_launch_agent_contract() {
    let bash = bash_or_skip!();
    let Some(python3) = on_path("python3") else {
        eprintln!("skipping: no `python3` on PATH to parse the plist");
        return;
    };
    let home = Home::new("plist");
    let real_uname = on_path("uname").expect("uname on PATH");
    let shim_dir = home.path().join("shim");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim = shim_dir.join("uname");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nif [ \"$1\" = -s ]; then echo Darwin; else exec \"{}\" \"$@\"; fi\n",
            real_uname.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let path = env::join_paths(
        std::iter::once(shim_dir).chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();

    let (code, out, err) = run(home.relay(&bash, &["--dry-run"]).env("PATH", path));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let start = out.find("<?xml ").expect("plist start");
    let end = out.find("</plist>").expect("plist end") + "</plist>".len();
    let plist = &out[start..end];

    let mut parse = Command::new(python3);
    parse.arg("-c").arg(
        r#"import plistlib, sys, json
d = plistlib.loads(sys.stdin.buffer.read())
print(json.dumps(d, sort_keys=True))"#,
    );
    parse.stdin(std::process::Stdio::piped());
    parse.stdout(std::process::Stdio::piped());
    parse.stderr(std::process::Stdio::piped());
    let mut child = parse.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(plist.as_bytes())
        .unwrap();
    let parsed = child.wait_with_output().unwrap();
    assert!(
        parsed.status.success(),
        "plistlib rejected the plan:\n{}\n{plist}",
        String::from_utf8_lossy(&parsed.stderr)
    );
    let json = String::from_utf8_lossy(&parsed.stdout);
    let log = home.path().join("Library/Logs/coyote-rnsd.log");
    let expected = format!(
        r#"{{"EnvironmentVariables": {{"PYTHONUNBUFFERED": "1"}}, "KeepAlive": true, "Label": "com.coyote.rnsd", "ProgramArguments": ["{rnsd}"], "RunAtLoad": true, "StandardErrorPath": "{log}", "StandardOutPath": "{log}"}}"#,
        rnsd = home.bin_dir().join("rnsd").display(),
        log = log.display()
    );
    assert_eq!(json.trim(), expected, "plist:\n{plist}");
    assert!(
        out.contains(&format!(
            "Would run: launchctl bootstrap gui/{} {}",
            String::from_utf8_lossy(&Command::new("id").arg("-u").output().unwrap().stdout).trim(),
            home.path()
                .join("Library/LaunchAgents/com.coyote.rnsd.plist")
                .display()
        )),
        "{out}"
    );
    assert!(out.contains(&format!("Logs: {}", log.display())), "{out}");
}
