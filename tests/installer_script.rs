//! Usage coverage for `scripts/install_coyote.sh`, run the way an operator runs it: a
//! child `bash` under a throwaway HOME, judged by exit code, output and what lands on
//! disk. Nothing reaches the network: a stand-in `curl` on a hand-picked PATH serves
//! the release metadata and a prepared tarball from disk and records every request,
//! and a `uname` answering `Linux` keeps the mesh relay on its systemd path (with no
//! `systemctl` on PATH it writes the unit and prints how to enable it) on every OS.
//! The fixture mirrors `mesh_relay_script.rs`.
//!
//! Unix only, like the script. Without `bash` on PATH the tests print `skipping:`.
#![cfg(unix)]

use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn installer() -> PathBuf {
    repo_root().join("scripts").join("install_coyote.sh")
}

/// `None` when `bash` is not on PATH; the caller prints `skipping:` and returns.
fn bash() -> Option<PathBuf> {
    on_path("bash")
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

/// The asset name the installer derives for a musl Linux host of this machine's arch.
fn asset_name() -> String {
    let machine = Command::new("uname").arg("-m").output().unwrap().stdout;
    let arch = match String::from_utf8_lossy(&machine).trim() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => panic!("unsupported arch {other}"),
    };
    format!("coyote-{arch}-unknown-linux-musl.tar.gz")
}

/// A throwaway HOME. `XDG_CONFIG_HOME` and `BIN_DIR` sit inside it so every path the
/// installer or the relay may write is under one root that is removed on drop.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-installer-{label}-{unique}"));
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

    /// A stand-in `rnsd` whose `--version` succeeds: to the installer a mesh set up
    /// earlier, to the relay an rnsd it reuses instead of installing anything.
    fn seed_rnsd(&self) {
        let bin = self.bin_dir();
        fs::create_dir_all(&bin).unwrap();
        let rnsd = bin.join("rnsd");
        fs::write(&rnsd, "#!/bin/sh\necho 'rnsd 1.5.2'\n").unwrap();
        fs::set_permissions(&rnsd, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `script` run by `bash` with stdin closed, as a piped or scripted install has it.
    fn install(&self, bash: &Path, script: &Path, args: &[&str], tools: &Tools) -> Command {
        let mut cmd = Command::new(bash);
        cmd.arg(script)
            .args(args)
            .current_dir(repo_root())
            .stdin(Stdio::null())
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.config_home())
            .env("BIN_DIR", self.bin_dir())
            .env("PATH", tools.path())
            .env_remove("COYOTE_CONFIG_DIR")
            .env_remove("COYOTE_VERSION");
        cmd
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A directory of hand-picked tools for a PATH that holds only what the installer
/// and the relay reach for: real coreutils by symlink, a `uname` whose `-s` answers
/// `Linux`, and a `curl` that serves a one-asset release from disk, hands out the
/// relay stub at `relay_stub` when one was written, and fails every other URL with
/// curl's own HTTP-error exit code 22. Every request is appended to `log`.
struct Tools {
    dir: PathBuf,
    log: PathBuf,
    relay_stub: PathBuf,
}

impl Tools {
    const BASE: [&'static str; 24] = [
        "sh", "bash", "id", "mktemp", "mkdir", "rm", "rmdir", "dirname", "basename", "chmod", "mv",
        "cp", "ln", "cat", "head", "grep", "sleep", "tar", "gzip", "install", "find", "tr", "sed",
        "sort",
    ];

    fn new(home: &Home) -> Self {
        let dir = home.path().join("tools");
        fs::create_dir_all(&dir).unwrap();
        for name in Self::BASE {
            if let Some(real) = on_path(name) {
                symlink(real, dir.join(name)).unwrap();
            }
        }
        let real_uname = on_path("uname").expect("uname on PATH");
        write_script(
            &dir.join("uname"),
            &format!(
                "if [ \"$1\" = -s ]; then echo Linux; else exec \"{}\" \"$@\"; fi\n",
                real_uname.display()
            ),
        );

        let release = home.path().join("release");
        let stage = release.join("stage");
        fs::create_dir_all(&stage).unwrap();
        write_script(&stage.join("coyote"), "echo 'coyote 0.0.0-test'\n");
        let asset = asset_name();
        let tarball = release.join(&asset);
        let packed = Command::new(on_path("tar").expect("tar on PATH"))
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&stage)
            .arg("coyote")
            .status()
            .unwrap();
        assert!(packed.success(), "tar failed to pack the stub release");
        let json = release.join("release.json");
        fs::write(
            &json,
            format!(
                r#"{{"assets":[{{"name":"{asset}","browser_download_url":"https://example.invalid/{asset}"}}]}}"#
            ),
        )
        .unwrap();

        let log = home.path().join("curl.log");
        let relay_stub = release.join("mesh-relay.sh");
        write_script(
            &dir.join("curl"),
            &format!(
                r#"printf '%s\n' "curl $*" >> "{log}"
url=""; out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -H) shift 2 ;;
    http*) url="$1"; shift ;;
    *) shift ;;
  esac
done
case "$url" in
  */releases/*) cat "{json}" ;;
  */{asset}) cp "{tarball}" "$out" ;;
  */mesh-relay.sh) [ -f "{stub}" ] && cat "{stub}" || exit 22 ;;
  *) exit 22 ;;
esac
"#,
                log = log.display(),
                json = json.display(),
                tarball = tarball.display(),
                stub = relay_stub.display(),
            ),
        );
        Tools {
            dir,
            log,
            relay_stub,
        }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// What the stand-in `curl` was asked for, one invocation per line.
    fn requests(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// From now on the relay URL serves a script whose body is `body`.
    fn serve_relay(&self, body: &str) {
        write_script(&self.relay_stub, body);
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
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

fn pointer_lines(out: &str) -> Vec<&str> {
    out.lines()
        .filter(|line| line.contains("mesh-relay.sh"))
        .collect()
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
fn without_the_flag_or_a_tty_coyote_is_installed_and_the_mesh_is_only_pointed_at() {
    let bash = bash_or_skip!();
    let home = Home::new("pointer");
    let tools = Tools::new(&home);

    let (code, out, err) = run(&mut home.install(&bash, &installer(), &[], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.is_empty(), "a clean install keeps stderr empty:\n{err}");
    let coyote = home.bin_dir().join("coyote");
    assert!(coyote.is_file(), "no coyote in BIN_DIR:\n{out}");
    assert_eq!(
        fs::metadata(&coyote).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(out.contains("Done. Try: coyote --help"), "{out}");

    let pointers = pointer_lines(&out);
    assert_eq!(
        pointers.len(),
        1,
        "exactly one line points at the mesh setup:\n{out}"
    );
    assert!(
        pointers[0].contains("--with-mesh"),
        "the pointer names the flag:\n{}",
        pointers[0]
    );
    assert_eq!(
        pointers[0].contains("normal user"),
        running_as_root(),
        "root is told to run the relay unelevated, nobody else is:\n{}",
        pointers[0]
    );
    assert!(!out.contains("[y/N]"), "no prompt without a tty:\n{out}");
    assert!(
        !home.path().join(".reticulum").exists() && !home.config_home().join("systemd").exists(),
        "the relay ran without being asked"
    );
    let requests = tools.requests();
    assert!(
        !requests.contains("mesh-relay.sh"),
        "the relay script was fetched without being asked:\n{requests}"
    );
}

#[test]
fn with_the_flag_the_checkouts_own_relay_runs_and_nothing_is_fetched_for_it() {
    let bash = bash_or_skip!();
    if running_as_root() {
        eprintln!("skipping: the relay refuses root and the installer passes it no --allow-root");
        return;
    }
    let home = Home::new("sibling");
    let tools = Tools::new(&home);
    home.seed_rnsd();

    let (code, out, err) = run(&mut home.install(&bash, &installer(), &["--with-mesh"], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(home.bin_dir().join("coyote").is_file());
    assert!(
        home.reticulum_config().is_file(),
        "the sibling relay did not write the config:\n{out}"
    );
    assert!(out.contains("rnsd already installed"), "{out}");
    assert!(out.contains("Done. Try: coyote --help"), "{out}");
    let requests = tools.requests();
    assert!(
        !requests.contains("mesh-relay.sh"),
        "a checked-out installer uses its sibling, it does not fetch:\n{requests}"
    );
    assert!(
        !out.contains("Fetching https://raw.githubusercontent.com"),
        "{out}"
    );
}

#[test]
fn with_the_flag_and_no_sibling_a_failed_relay_is_an_error_on_stderr_and_exit_3() {
    let bash = bash_or_skip!();
    let home = Home::new("alone");
    let tools = Tools::new(&home);
    let alone = home.path().join("alone");
    fs::create_dir_all(&alone).unwrap();
    let copy = alone.join("install_coyote.sh");
    fs::copy(installer(), &copy).unwrap();

    let (code, out, err) = run(&mut home.install(&bash, &copy, &["--with-mesh"], &tools));
    assert_eq!(code, 3, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.contains("failed to download the mesh setup script")
            && err.contains("coyote itself is installed"),
        "the failure is reported on stderr:\n{err}"
    );
    assert!(
        home.bin_dir().join("coyote").is_file(),
        "coyote is installed even when the mesh step fails"
    );
    assert!(out.contains("Done. Try: coyote --help"), "{out}");
    assert_eq!(
        tools.requests().matches("mesh-relay.sh").count(),
        1,
        "{}",
        tools.requests()
    );
    assert!(!home.reticulum_config().exists());

    // A relay that was fetched but exits non-zero is the same error with its code.
    tools.serve_relay("exit 7\n");
    let (code, out, err) = run(&mut home.install(&bash, &copy, &["--with-mesh"], &tools));
    assert_eq!(code, 3, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.contains("mesh setup exited with code 7"),
        "the relay's exit code is named:\n{err}"
    );
    assert!(out.contains("Done. Try: coyote --help"), "{out}");
}

#[test]
fn an_rnsd_already_in_bin_dir_means_an_upgrade_so_neither_prompt_nor_pointer_repeats() {
    let bash = bash_or_skip!();
    let home = Home::new("upgrade");
    let tools = Tools::new(&home);
    home.seed_rnsd();

    let (code, out, err) = run(&mut home.install(&bash, &installer(), &[], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(home.bin_dir().join("coyote").is_file());
    assert!(
        pointer_lines(&out).is_empty() && !out.contains("Reticulum daemon"),
        "an upgrade is not pointed at the mesh setup again:\n{out}"
    );
    assert!(!out.contains("[y/N]"), "{out}");
    assert!(
        !home.reticulum_config().exists(),
        "the relay ran without being asked"
    );
    assert!(!tools.requests().contains("mesh-relay.sh"));
}

#[test]
fn help_lists_with_mesh_and_the_exit_3_contract() {
    let bash = bash_or_skip!();
    let home = Home::new("help");
    let tools = Tools::new(&home);

    let (code, out, err) = run(&mut home.install(&bash, &installer(), &["--help"], &tools));
    assert_eq!(code, 0, "stderr:\n{err}");
    assert!(err.is_empty(), "--help writes to stdout only:\n{err}");
    for needle in [
        "--with-mesh",
        "--version",
        "--bin-dir",
        "Exits 3 when --with-mesh",
    ] {
        assert!(out.contains(needle), "--help lacks {needle:?}:\n{out}");
    }
    assert_eq!(tools.requests(), "", "--help made a request");
}
