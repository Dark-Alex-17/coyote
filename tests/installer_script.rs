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

use expectrl::process::unix::WaitStatus;
use expectrl::session::{OsProcess, OsStream};
use expectrl::{Expect, Session};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

fn euid() -> u32 {
    static UID: OnceLock<u32> = OnceLock::new();
    *UID.get_or_init(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
            .unwrap_or(u32::MAX)
    })
}

fn owned_by_me(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.uid() == euid())
        .unwrap_or(false)
}

fn machine_arch() -> &'static str {
    let machine = Command::new("uname").arg("-m").output().unwrap().stdout;
    match String::from_utf8_lossy(&machine).trim() {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => panic!("unsupported arch {other}"),
    }
}

/// The asset name the bash installer derives for a musl Linux host of this machine's
/// arch (its `uname -s` is shimmed to Linux below).
fn asset_name() -> String {
    format!("coyote-{}-unknown-linux-musl.tar.gz", machine_arch())
}

/// The PowerShell installer reads the OS from .NET, not `uname`, so on a Mac it asks
/// for the Darwin asset; the stub release lists both names for the same tarball.
fn darwin_asset_name() -> String {
    format!("coyote-{}-apple-darwin.tar.gz", machine_arch())
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
        let mut cmd = self.install_at_tty(bash, script, args, tools);
        cmd.stdin(Stdio::null());
        cmd
    }

    /// `script` run by `bash` with its stdio untouched, for `Session::spawn` to wire to
    /// a pseudo-terminal: `PtyProcess` dup2s the slave onto 0/1/2 before `exec`, so any
    /// `Stdio` set on the command here would win over the tty.
    fn install_at_tty(&self, bash: &Path, script: &Path, args: &[&str], tools: &Tools) -> Command {
        let mut cmd = Command::new(bash);
        cmd.arg(script)
            .args(args)
            .current_dir(repo_root())
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.config_home())
            .env("BIN_DIR", self.bin_dir())
            .env("PATH", tools.path())
            .env_remove("COYOTE_CONFIG_DIR")
            .env_remove("COYOTE_VERSION");
        cmd
    }

    /// The installer fed to `bash -s` on stdin, as `curl ... | bash -s -- <args>` has it:
    /// `BASH_SOURCE` is empty, so there is no checkout to find a sibling in.
    fn install_piped(&self, bash: &Path, args: &[&str], tools: &Tools) -> (i32, String, String) {
        let mut cmd = self.install(bash, Path::new("-s"), &[], tools);
        cmd.arg("--")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn bash");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&fs::read(installer()).unwrap())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
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
    json: PathBuf,
    tarball: PathBuf,
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
                r#"{{"assets":[{{"name":"{asset}","browser_download_url":"https://example.invalid/{asset}"}},{{"name":"{darwin}","browser_download_url":"https://example.invalid/{darwin}"}}]}}"#,
                darwin = darwin_asset_name(),
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
  */{asset}|*/{darwin}) cp "{tarball}" "$out" ;;
  */mesh-relay.sh) [ -f "{stub}" ] && cat "{stub}" || exit 22 ;;
  *) exit 22 ;;
esac
"#,
                log = log.display(),
                json = json.display(),
                darwin = darwin_asset_name(),
                tarball = tarball.display(),
                stub = relay_stub.display(),
            ),
        );
        Tools {
            dir,
            log,
            json,
            tarball,
            relay_stub,
        }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    /// Swap the stand-in `curl` for a stand-in `wget`. `gnu` picks a `--help` that lists
    /// `--https-only` and `--secure-protocol` and accepts them; otherwise the help lists
    /// neither and, like busybox, any unknown `--` option is a fatal exit 1.
    fn wget_instead_of_curl(&self, gnu: bool) {
        fs::remove_file(self.dir.join("curl")).unwrap();
        let (help, tls) = if gnu {
            (
                "  --https-only                only follow secure HTTPS links\\n  --secure-protocol=PR        choose secure protocol, one of auto, SSLv2,",
                "--https-only|--secure-protocol=*) shift ;;",
            )
        } else {
            (
                "Usage: wget [-cqS] [-O FILE] [-o LOGFILE] [--header STR] URL...",
                "",
            )
        };
        write_script(
            &self.dir.join("wget"),
            &format!(
                r#"printf '%s\n' "wget $*" >> "{log}"
for a in "$@"; do case "$a" in --help) printf '{help}\n'; exit 0 ;; esac; done
url=""; out="-"
while [ $# -gt 0 ]; do
  case "$1" in
    -O) out="$2"; shift 2 ;;
    -q|-qO-|--header=*) shift ;;
    {tls}
    --*) echo "wget: unrecognized option: $1" >&2; exit 1 ;;
    http*) url="$1"; shift ;;
    *) shift ;;
  esac
done
serve() {{ if [ "$out" = - ]; then cat "$1"; else cp "$1" "$out"; fi; }}
case "$url" in
  */releases/*) serve "{json}" ;;
  */{asset}|*/{darwin}) serve "{tarball}" ;;
  */mesh-relay.sh) [ -f "{stub}" ] && serve "{stub}" || exit 8 ;;
  *) exit 8 ;;
esac
"#,
                log = self.log.display(),
                json = self.json.display(),
                asset = asset_name(),
                darwin = darwin_asset_name(),
                tarball = self.tarball.display(),
                stub = self.relay_stub.display(),
            ),
        );
    }

    /// Swap the real `id` for one answering `0`, so the installers take their root branch.
    fn fake_root(&self) {
        fs::remove_file(self.dir.join("id")).unwrap();
        write_script(&self.dir.join("id"), "echo 0\n");
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
fn root_is_pointed_at_the_relay_as_its_normal_user_and_never_prompted() {
    let bash = bash_or_skip!();
    let home = Home::new("fake-root");
    let tools = Tools::new(&home);
    tools.fake_root();

    let (code, out, err) = run(&mut home.install(&bash, &installer(), &[], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let pointers = pointer_lines(&out);
    assert_eq!(pointers.len(), 1, "exactly one pointer line:\n{out}");
    assert!(
        pointers[0].contains("run it as your normal user"),
        "root is told to run the relay unelevated:\n{}",
        pointers[0]
    );
    assert!(!out.contains("[y/N]"), "root is never prompted:\n{out}");
    assert!(
        !tools.requests().contains("mesh-relay.sh"),
        "{}",
        tools.requests()
    );

    // An rnsd already in BIN_DIR is an upgrade for root too: neither pointer nor prompt.
    let home = Home::new("fake-root-upgrade");
    let tools = Tools::new(&home);
    tools.fake_root();
    home.seed_rnsd();
    let (code, out, err) = run(&mut home.install(&bash, &installer(), &[], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        pointer_lines(&out).is_empty() && !out.contains("[y/N]"),
        "{out}"
    );

    // --with-mesh with the fetched relay failing: exit 3, and the retry hint names the normal user.
    let home = Home::new("fake-root-with-mesh");
    let tools = Tools::new(&home);
    tools.fake_root();
    tools.serve_relay("exit 1\n");
    let alone = home.path().join("alone");
    fs::create_dir_all(&alone).unwrap();
    let copy = alone.join("install_coyote.sh");
    fs::copy(installer(), &copy).unwrap();
    let (code, out, err) = run(&mut home.install(&bash, &copy, &["--with-mesh"], &tools));
    assert_eq!(code, 3, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("mesh setup exited with code 1"), "{err}");
    assert!(
        err.trim_end().ends_with("as your normal user"),
        "the retry hint is addressed to the normal user:\n{err}"
    );
}

#[test]
fn with_the_flag_the_checkouts_own_relay_runs_and_nothing_is_fetched_for_it() {
    let bash = bash_or_skip!();
    if running_as_root() {
        eprintln!("skipping: the relay refuses root and the installer passes it no --allow-root");
        return;
    }
    if !owned_by_me(&installer()) {
        eprintln!(
            "skipping: scripts/install_coyote.sh is not owned by this user, so its sibling is never trusted"
        );
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

/// Without curl the installer downloads with wget, passing `--https-only` and
/// `--secure-protocol=TLSv1_2` only when `wget --help` advertises them: busybox wget
/// (Alpine) and old GNU wget reject unknown options, and the install must still land.
#[test]
fn a_wget_only_host_installs_with_whichever_tls_flags_its_wget_understands() {
    let bash = bash_or_skip!();

    let home = Home::new("gnu-wget");
    let tools = Tools::new(&home);
    tools.wget_instead_of_curl(true);
    let (code, out, err) = run(&mut home.install(&bash, &installer(), &[], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(home.bin_dir().join("coyote").is_file(), "{out}");
    let requests = tools.requests();
    let downloads: Vec<&str> = requests
        .lines()
        .filter(|line| line.starts_with("wget ") && !line.contains("--help"))
        .collect();
    assert_eq!(downloads.len(), 2, "{requests}");
    for line in &downloads {
        assert!(
            line.contains("--https-only --secure-protocol=TLSv1_2"),
            "a GNU wget gets both TLS flags: {line}"
        );
    }
    assert!(!out.contains("This wget lacks"), "{out}");

    let home = Home::new("busybox-wget");
    let tools = Tools::new(&home);
    tools.wget_instead_of_curl(false);
    let (code, out, err) = run(&mut home.install(&bash, &installer(), &[], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(home.bin_dir().join("coyote").is_file(), "{out}");
    let requests = tools.requests();
    assert!(
        requests.lines().any(|line| line.starts_with("wget --help")),
        "the flags are feature-detected through --help:\n{requests}"
    );
    assert!(
        !requests.contains("--https-only") && !requests.contains("--secure-protocol"),
        "a wget that lacks the flags must not be given them:\n{requests}"
    );
    assert!(
        out.contains("This wget lacks --https-only")
            && out.contains("This wget lacks --secure-protocol"),
        "each missing flag is named once:\n{out}"
    );
}

/// A sibling `mesh-relay.sh` the invoking user does not own is never run: the installer
/// fetches the relay instead, exactly as a piped `bash -s` with no checkout does.
#[test]
fn an_unowned_sibling_relay_is_bypassed_and_a_piped_installer_always_fetches() {
    let bash = bash_or_skip!();
    let foreign = ["/etc/hostname", "/etc/passwd"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file() && !owned_by_me(path));
    let Some(foreign) = foreign else {
        eprintln!("skipping: no file owned by another user to stand in as the sibling");
        return;
    };

    let home = Home::new("unowned-sibling");
    let tools = Tools::new(&home);
    tools.serve_relay("echo relay-stub-ran\nexit 0\n");
    let checkout = home.path().join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    let copy = checkout.join("install_coyote.sh");
    fs::copy(installer(), &copy).unwrap();
    symlink(foreign, checkout.join("mesh-relay.sh")).unwrap();

    let (code, out, err) = run(&mut home.install(&bash, &copy, &["--with-mesh"], &tools));
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        out.contains("Fetching https://raw.githubusercontent.com")
            && out.contains("relay-stub-ran"),
        "the unowned sibling must be passed over for the fetched relay:\n{out}"
    );
    assert_eq!(
        tools.requests().matches("mesh-relay.sh").count(),
        1,
        "{}",
        tools.requests()
    );

    let home = Home::new("piped");
    let tools = Tools::new(&home);
    tools.serve_relay("echo relay-stub-ran\nexit 0\n");
    let (code, out, err) = home.install_piped(&bash, &["--with-mesh"], &tools);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(home.bin_dir().join("coyote").is_file(), "{out}");
    assert!(
        out.contains("Fetching https://raw.githubusercontent.com")
            && out.contains("relay-stub-ran"),
        "a piped installer has no checkout and must fetch:\n{out}"
    );
    assert_eq!(
        tools.requests().matches("mesh-relay.sh").count(),
        1,
        "{}",
        tools.requests()
    );
    assert!(
        !home.reticulum_config().exists(),
        "the checkout's real relay ran instead of the fetched stub"
    );
}

/// The PowerShell installer, driven on this host by `pwsh`: global functions defined
/// before the installer runs shadow `Invoke-RestMethod` and `Invoke-WebRequest`, so the
/// release and the relay come from disk and every request is recorded. The harness
/// tools sit ahead of the host's PATH, so a fake `id` is the one the installer asks.
struct PwshDriver {
    script: PathBuf,
    log: PathBuf,
    path: OsString,
}

impl PwshDriver {
    fn new(home: &Home, tools: &Tools) -> Self {
        let dir = home.path().join("pwsh");
        fs::create_dir_all(&dir).unwrap();
        let installer_copy = dir.join("install_coyote.ps1");
        fs::copy(
            repo_root().join("scripts").join("install_coyote.ps1"),
            &installer_copy,
        )
        .unwrap();
        let log = home.path().join("pwsh-requests.log");
        let script = dir.join("driver.ps1");
        fs::write(
            &script,
            format!(
                r#"function global:Invoke-RestMethod {{
  param([string]$Uri, $Headers, [string]$Method, [switch]$UseBasicParsing)
  Add-Content -LiteralPath '{log}' -Value "irm $Uri"
  return (Get-Content -Raw -LiteralPath '{json}' | ConvertFrom-Json)
}}
function global:Invoke-WebRequest {{
  param([string]$Uri, $Headers, [string]$OutFile, [switch]$UseBasicParsing)
  Add-Content -LiteralPath '{log}' -Value "iwr $Uri"
  if ($Uri.EndsWith('/{asset}') -or $Uri.EndsWith('/{darwin}')) {{ Copy-Item -LiteralPath '{tarball}' -Destination $OutFile; return }}
  if ($Uri.EndsWith('/mesh-relay.sh') -and (Test-Path -LiteralPath '{stub}')) {{ Copy-Item -LiteralPath '{stub}' -Destination $OutFile; return }}
  throw "404 Not Found: $Uri"
}}
& '{installer}' @args
exit $LASTEXITCODE
"#,
                log = log.display(),
                json = tools.json.display(),
                asset = asset_name(),
                darwin = darwin_asset_name(),
                tarball = tools.tarball.display(),
                stub = tools.relay_stub.display(),
                installer = installer_copy.display(),
            ),
        )
        .unwrap();
        let mut dirs = vec![tools.path().to_path_buf()];
        if let Some(host) = env::var_os("PATH") {
            dirs.extend(env::split_paths(&host));
        }
        let path = env::join_paths(dirs).unwrap();
        PwshDriver { script, log, path }
    }

    fn run(&self, pwsh: &Path, home: &Home, args: &[&str]) -> (i32, String, String) {
        let mut cmd = Command::new(pwsh);
        cmd.arg("-NoProfile")
            .arg("-File")
            .arg(&self.script)
            .args(args)
            .current_dir(repo_root())
            .stdin(Stdio::null())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.config_home())
            .env("BIN_DIR", home.bin_dir())
            .env("PATH", &self.path)
            .env("CI", "1")
            .env_remove("COYOTE_CONFIG_DIR")
            .env_remove("COYOTE_VERSION");
        run(&mut cmd)
    }

    fn requests(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn the_powershell_installer_points_fetches_and_exits_3_the_same_way_as_the_bash_one() {
    let Some(pwsh) = on_path("pwsh") else {
        eprintln!("skipping: no `pwsh` on PATH");
        return;
    };
    if running_as_root() {
        eprintln!("skipping: the relay refuses root and the installer passes it no --allow-root");
        return;
    }

    // No flag, CI set: coyote lands, one pointer, no prompt, nothing fetched for the mesh.
    let home = Home::new("pwsh-pointer");
    let tools = Tools::new(&home);
    let driver = PwshDriver::new(&home, &tools);
    let (code, out, err) = driver.run(&pwsh, &home, &[]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        home.bin_dir().join("coyote").is_file(),
        "no coyote in BIN_DIR:\n{out}\n{err}"
    );
    let pointers = pointer_lines(&out);
    assert_eq!(pointers.len(), 1, "exactly one pointer line:\n{out}");
    assert!(pointers[0].contains("-WithMesh"), "{}", pointers[0]);
    assert!(
        !out.contains("Set up the local Reticulum daemon for Coyote mesh now?"),
        "no prompt under CI:\n{out}"
    );
    assert!(
        !driver.requests().contains("mesh-relay.sh"),
        "{}",
        driver.requests()
    );

    // No flag as root (a fake `id` answering 0): the pointer says to run the relay as
    // the normal user, and there is no prompt.
    let home = Home::new("pwsh-fake-root");
    let tools = Tools::new(&home);
    tools.fake_root();
    let driver = PwshDriver::new(&home, &tools);
    let (code, out, err) = driver.run(&pwsh, &home, &[]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    let pointers = pointer_lines(&out);
    assert_eq!(pointers.len(), 1, "exactly one pointer line:\n{out}");
    assert!(
        pointers[0].contains("run it as your normal user"),
        "{}",
        pointers[0]
    );
    assert!(
        !out.contains("Set up the local Reticulum daemon for Coyote mesh now?"),
        "root is never prompted:\n{out}"
    );

    // -WithMesh with the relay fetch failing: an error on stderr, exit 3, coyote installed.
    let home = Home::new("pwsh-fetch-fails");
    let tools = Tools::new(&home);
    let driver = PwshDriver::new(&home, &tools);
    let (code, out, err) = driver.run(&pwsh, &home, &["-WithMesh"]);
    assert_eq!(code, 3, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        err.contains("failed to download the mesh setup script")
            && err.contains("coyote itself is installed"),
        "{err}"
    );
    assert!(home.bin_dir().join("coyote").is_file());
    assert_eq!(
        driver.requests().matches("mesh-relay.sh").count(),
        1,
        "{}",
        driver.requests()
    );

    // -WithMesh with a relay that runs clean: exit 0.
    let home = Home::new("pwsh-relay-ok");
    let tools = Tools::new(&home);
    tools.serve_relay("echo relay-stub-ran\nexit 0\n");
    let driver = PwshDriver::new(&home, &tools);
    let (code, out, err) = driver.run(&pwsh, &home, &["-WithMesh"]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("relay-stub-ran"), "{out}");
    assert!(err.is_empty(), "{err}");

    // -WithMesh as root with the fetched relay failing: exit 3, and the retry hint names the normal user.
    let home = Home::new("pwsh-fake-root-with-mesh");
    let tools = Tools::new(&home);
    tools.fake_root();
    tools.serve_relay("exit 1\n");
    let driver = PwshDriver::new(&home, &tools);
    let (code, out, err) = driver.run(&pwsh, &home, &["-WithMesh"]);
    assert_eq!(code, 3, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("mesh setup exited with code 1"), "{err}");
    assert!(
        err.trim_end().ends_with("as your normal user"),
        "the retry hint is addressed to the normal user:\n{err}"
    );

    // An rnsd already in BinDir: an upgrade, so no pointer.
    let home = Home::new("pwsh-upgrade");
    let tools = Tools::new(&home);
    home.seed_rnsd();
    let driver = PwshDriver::new(&home, &tools);
    let (code, out, err) = driver.run(&pwsh, &home, &[]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(
        pointer_lines(&out).is_empty() && !out.contains("Reticulum daemon"),
        "{out}"
    );
    assert!(!driver.requests().contains("mesh-relay.sh"));
}

/// At a tty the installer asks once, and Enter takes the default `N`: only the
/// pointer follows, and the relay is neither run nor fetched.
#[test]
fn at_a_tty_enter_declines_the_prompt_once_and_only_the_pointer_follows() {
    let bash = bash_or_skip!();
    if running_as_root() {
        eprintln!("skipping: root is never prompted");
        return;
    }
    let home = Home::new("tty-decline");
    let tools = Tools::new(&home);
    let copy = checkout_with_sibling(&home, "exit 0\n");

    let (code, out) = install_answering(home.install_at_tty(&bash, &copy, &[], &tools), "\r");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.matches("[y/N]").count(), 1, "asked once:\n{out}");
    let pointers = pointer_lines(&out);
    assert_eq!(pointers.len(), 1, "exactly one pointer line:\n{out}");
    assert!(pointers[0].contains("--with-mesh"), "{}", pointers[0]);
    assert!(
        !home.path().join("relay.log").exists(),
        "the relay ran after a declined prompt:\n{out}"
    );
    let requests = tools.requests();
    assert!(
        !requests.contains("mesh-relay.sh"),
        "the relay was fetched after a declined prompt:\n{requests}"
    );
    let coyote = home.bin_dir().join("coyote");
    assert!(coyote.is_file(), "no coyote in BIN_DIR:\n{out}");
    assert_eq!(
        fs::metadata(&coyote).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!home.path().join(".reticulum").exists(), "{out}");
}

/// Answering `y` runs the checkout's own relay once, with the installer's `BIN_DIR`,
/// fetching nothing; a relay that succeeds leaves no pointer behind.
#[test]
fn at_a_tty_y_runs_the_sibling_relay_exactly_once_and_nothing_is_fetched() {
    let bash = bash_or_skip!();
    if running_as_root() {
        eprintln!("skipping: root is never prompted");
        return;
    }
    let home = Home::new("tty-accept");
    let tools = Tools::new(&home);
    let copy = checkout_with_sibling(&home, "exit 0\n");

    let (code, out) = install_answering(home.install_at_tty(&bash, &copy, &[], &tools), "y\r");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.matches("[y/N]").count(), 1, "asked once:\n{out}");
    assert_eq!(
        fs::read_to_string(home.path().join("relay.log")).unwrap_or_default(),
        format!("BIN_DIR={}\n", home.bin_dir().display()),
        "the sibling relay runs exactly once with the installer's BIN_DIR:\n{out}"
    );
    let requests = tools.requests();
    assert!(
        !requests.contains("mesh-relay.sh"),
        "a checked-out installer uses its sibling, it does not fetch:\n{requests}"
    );
    assert!(
        pointer_lines(&out).is_empty(),
        "a relay that succeeded is not pointed at again:\n{out}"
    );
}

/// On the prompt path the mesh is optional, so a relay that fails is a note with its
/// exit code plus the pointer, and the installer still exits 0 (exit 3 is reserved
/// for an explicit `--with-mesh`).
#[test]
fn at_a_tty_y_with_a_failing_relay_is_a_note_and_the_pointer_not_exit_3() {
    let bash = bash_or_skip!();
    if running_as_root() {
        eprintln!("skipping: root is never prompted");
        return;
    }
    let home = Home::new("tty-relay-fails");
    let tools = Tools::new(&home);
    let copy = checkout_with_sibling(&home, "exit 2\n");

    let (code, out) = install_answering(home.install_at_tty(&bash, &copy, &[], &tools), "y\r");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out.matches("[y/N]").count(), 1, "asked once:\n{out}");
    assert!(
        out.contains("mesh setup exited with code 2; coyote itself is installed."),
        "the relay's exit code is named:\n{out}"
    );
    assert_eq!(
        pointer_lines(&out).len(),
        1,
        "exactly one pointer line:\n{out}"
    );
    assert_eq!(
        fs::read_to_string(home.path().join("relay.log"))
            .unwrap_or_default()
            .lines()
            .count(),
        1,
        "{out}"
    );
}

/// Copies the installer into `<HOME>/checkout/` beside a stand-in `mesh-relay.sh` that
/// appends `BIN_DIR=<value>` to `<HOME>/relay.log` and then runs `tail`. Both files are
/// owned by the test uid, so `run_mesh_relay` trusts the sibling over a fetch.
fn checkout_with_sibling(home: &Home, tail: &str) -> PathBuf {
    let checkout = home.path().join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    let copy = checkout.join("install_coyote.sh");
    fs::copy(installer(), &copy).unwrap();
    write_script(
        &checkout.join("mesh-relay.sh"),
        &format!(
            "printf '%s\\n' \"BIN_DIR=$BIN_DIR\" >> \"{}\"\n{tail}",
            home.path().join("relay.log").display()
        ),
    );
    copy
}

type Pty = Session<OsProcess, OsStream>;

const PTY_TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `cmd` under a pseudo-terminal, types `answer` at the mesh prompt and returns
/// the exit code with the whole transcript, its `\r\n` line ends normalised.
fn install_answering(cmd: Command, answer: &str) -> (i32, String) {
    let mut session = spawn_pty(cmd);
    let mut transcript = Vec::new();
    expect_or_dump(
        &mut session,
        &mut transcript,
        "Set up the local Reticulum daemon for Coyote mesh now? [y/N] ",
    );
    session.send(answer).expect("answer the prompt");
    expect_or_dump(&mut session, &mut transcript, "Done. Try: coyote --help");
    drain_to_eof(&mut session, &mut transcript);
    let code = wait_exit(&mut session);
    (
        code,
        String::from_utf8_lossy(&transcript).replace("\r\n", "\n"),
    )
}

fn spawn_pty(command: Command) -> Pty {
    let mut session = Session::spawn(command).expect("spawn the installer under a pty");
    session
        .get_process_mut()
        .set_window_size(120, 40)
        .expect("set pty window size");
    session.set_expect_timeout(Some(PTY_TIMEOUT));
    session
}

/// Waits for `needle`, appending everything up to and including it to `transcript`.
/// On timeout or EOF the panic carries the transcript plus whatever else has arrived.
fn expect_or_dump(session: &mut Pty, transcript: &mut Vec<u8>, needle: &str) {
    match session.expect(needle) {
        Ok(captures) => {
            transcript.extend_from_slice(captures.before());
            transcript.extend_from_slice(captures.get(0).unwrap_or_default());
        }
        Err(err) => {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = session.try_read(&mut chunk) {
                if n == 0 {
                    break;
                }
                transcript.extend_from_slice(&chunk[..n]);
            }
            panic!(
                "waiting for {needle:?}: {err}\ntranscript:\n{}",
                String::from_utf8_lossy(transcript)
            );
        }
    }
}

/// Reads the rest of the output into `transcript` until the pty hangs up, which Linux
/// reports as `EIO` rather than a zero-length read.
fn drain_to_eof(session: &mut Pty, transcript: &mut Vec<u8>) {
    let deadline = Instant::now() + PTY_TIMEOUT;
    let mut chunk = [0u8; 4096];
    loop {
        match session.try_read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => transcript.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the installer kept the pty open:\n{}",
                    String::from_utf8_lossy(transcript)
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
}

fn wait_exit(session: &mut Pty) -> i32 {
    match session
        .get_process_mut()
        .wait()
        .expect("wait for the installer")
    {
        WaitStatus::Exited(_, code) => code,
        other => panic!("the installer did not exit normally: {other:?}"),
    }
}
