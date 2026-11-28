//! Running a shell script the way an operator runs it: the interpreter found on
//! PATH, a throwaway HOME (Unix), a PATH of hand-picked tools (Unix), and the
//! exit code and output collected in one call.

use std::env;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::OnceLock;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

/// Resolve `name` on PATH, for building a PATH of hand-picked tools.
pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `None` when `bash` is not on PATH; the caller prints `skipping:` and returns.
pub fn bash() -> Option<PathBuf> {
    on_path("bash")
}

/// `pwsh` resolved on PATH by hand: on Windows `Command::new("pwsh")` looks in
/// System32 before PATH, so the absolute path is what gets spawned.
pub fn pwsh() -> Option<PathBuf> {
    on_path(if cfg!(windows) { "pwsh.exe" } else { "pwsh" })
}

/// What `id -u` printed, or empty when it could not be run.
pub fn current_uid() -> &'static str {
    static UID: OnceLock<String> = OnceLock::new();
    UID.get_or_init(|| {
        Command::new("id")
            .arg("-u")
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default()
    })
}

pub fn running_as_root() -> bool {
    current_uid() == "0"
}

pub fn run(cmd: &mut Command) -> (i32, String, String) {
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

/// The `bash` on PATH, or `skipping:` printed and the test returned.
#[macro_export]
macro_rules! bash_or_skip {
    () => {
        match $crate::support::script::bash() {
            Some(bash) => bash,
            None => {
                eprintln!("skipping: no `bash` on PATH");
                return;
            }
        }
    };
}

/// The `pwsh` on PATH, or `skipping:` printed and the test returned.
#[macro_export]
macro_rules! pwsh_or_skip {
    () => {
        match $crate::support::script::pwsh() {
            Some(pwsh) => pwsh,
            None => {
                eprintln!("skipping: no pwsh on PATH");
                return;
            }
        }
    };
}

/// A throwaway HOME. `XDG_CONFIG_HOME` and `BIN_DIR` sit inside it so every path a
/// script may write is under one root that is removed when the fixture drops.
#[cfg(unix)]
pub struct Home {
    root: PathBuf,
}

#[cfg(unix)]
impl Home {
    /// A fresh dir named `<prefix>-<label>-<nanos>` under the temp dir.
    pub fn new(prefix: &str, label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("{prefix}-{label}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        Home { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn config_home(&self) -> PathBuf {
        self.root.join(".config")
    }

    pub fn reticulum_config(&self) -> PathBuf {
        self.root.join(".reticulum").join("config")
    }

    /// A stand-in `rnsd` whose `--version` succeeds, so a script finds an rnsd
    /// already in place and reuses it instead of installing anything.
    pub fn seed_rnsd(&self) -> PathBuf {
        let bin = self.bin_dir();
        fs::create_dir_all(&bin).unwrap();
        let rnsd = bin.join("rnsd");
        fs::write(&rnsd, "#!/bin/sh\necho 'rnsd 1.5.2'\n").unwrap();
        fs::set_permissions(&rnsd, fs::Permissions::from_mode(0o755)).unwrap();
        rnsd
    }

    /// Everything under the HOME, relative, sorted, so a "wrote nothing" check can
    /// name what appeared.
    pub fn entries(&self) -> Vec<String> {
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

#[cfg(unix)]
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A directory of hand-picked tools for a PATH that holds only what a scenario
/// needs: real coreutils by symlink, stand-ins written as `#!/bin/sh` scripts.
#[cfg(unix)]
pub struct Tools {
    dir: PathBuf,
}

#[cfg(unix)]
impl Tools {
    /// `<home>/tools`, holding a symlink to each of `base` that is on PATH.
    pub fn new(home: &Home, base: &[&str]) -> Self {
        let dir = home.path().join("tools");
        fs::create_dir_all(&dir).unwrap();
        for name in base {
            if let Some(real) = on_path(name) {
                symlink(real, dir.join(name)).unwrap();
            }
        }
        Tools { dir }
    }

    /// A stand-in `name` that appends its arguments to `log` and then runs `body`.
    pub fn fake(&self, name: &str, log: &Path, body: &str) -> &Self {
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

    pub fn path(&self) -> &Path {
        &self.dir
    }
}
