use crate::config::{Session, paths};

use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Where instance locks live.
pub(crate) fn lock_dir() -> PathBuf {
    paths::cache_dir().join("mesh")
}

/// Marks one session's mesh instance as owned by this process. The lock file carries our pid
/// and is removed when the value drops; a lock left behind by a dead process is reclaimed.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    /// Claims `<cache_dir>/mesh/<instance_id>.lock`, refusing while another live process holds it.
    pub(crate) fn acquire(cache_dir: &Path, instance_id: &str) -> Result<Self> {
        if !Session::is_valid_mesh_instance_id(instance_id) {
            bail!(
                "Mesh instance id '{instance_id}' is malformed: expected 32 lowercase hex characters. The session file's `mesh_instance_id` is corrupt; remove that line from the session file to mint a fresh id."
            );
        }
        let dir = cache_dir.join("mesh");
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create directory '{}'", dir.display()))?;
        let path = dir.join(format!("{instance_id}.lock"));

        if let Some(lock) = Self::try_create(&path)? {
            return Ok(lock);
        }

        match read_holder_pid(&path)? {
            Some(pid) if process_is_alive(pid)? => bail!(
                "This session already has mesh enabled in another Coyote process (pid {pid}). Run `.mesh on --fresh` here to join the mesh with a new ephemeral destination, or turn mesh off in the other process first."
            ),
            Some(pid) => debug!(
                "Reclaiming mesh instance lock '{}' left by dead pid {pid}",
                path.display()
            ),
            // A hard kill between create and write leaves an empty file behind.
            None => debug!(
                "Reclaiming mesh instance lock '{}' with no readable pid",
                path.display()
            ),
        }
        remove_if_present(&path)?;

        match Self::try_create(&path)? {
            Some(lock) => Ok(lock),
            None => bail!(
                "Another Coyote process is enabling mesh on this session right now. Run `.mesh on --fresh` here to join the mesh with a new ephemeral destination, or retry in a moment."
            ),
        }
    }

    /// `Ok(None)` means the file already exists. The lock value is built before the pid is
    /// written so a failed write still removes the file on drop.
    fn try_create(path: &Path) -> Result<Option<Self>> {
        let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to create mesh instance lock '{}'", path.display())
                });
            }
        };
        let lock = Self {
            path: path.to_path_buf(),
        };
        file.write_all(std::process::id().to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("Failed to write mesh instance lock '{}'", path.display()))?;
        Ok(Some(lock))
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_holder_pid(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content.trim().parse().ok()),
        // The holder released between our create attempt and this read.
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err)
            .with_context(|| format!("Failed to read mesh instance lock '{}'", path.display())),
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "Failed to remove stale mesh instance lock '{}'",
                path.display()
            )
        }),
    }
}

/// Whether a process with `pid` exists. A process we lack permission to signal still exists.
// The only unsafe in this module: a signal-0 probe through libc.
#[allow(unsafe_code)]
pub(crate) fn process_is_alive(pid: u32) -> Result<bool> {
    // No process has pid 0; kill(0, 0) would probe our own process group and always succeed.
    if pid == 0 {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        // A pid outside pid_t range cannot name a process; passing it through would be
        // interpreted as a process group.
        let Ok(pid_t) = libc::pid_t::try_from(pid) else {
            return Ok(false);
        };
        // SAFETY: kill with signal 0 only performs the existence and permission checks; no
        // signal is delivered and no memory is touched.
        if unsafe { libc::kill(pid_t, 0) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            Some(libc::EPERM) => Ok(true),
            _ => Err(err).with_context(|| format!("Failed to check whether pid {pid} is alive")),
        }
    }
    #[cfg(not(unix))]
    {
        bail!(
            "Process liveness checks are not yet implemented on Windows, so a stale mesh instance lock cannot be reclaimed safely and mesh cannot be enabled on this platform yet."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;

    const ID_A: &str = "0123456789abcdef0123456789abcdef";
    const ID_B: &str = "fedcba9876543210fedcba9876543210";
    const ID_C: &str = "00000000000000000000000000000001";
    const ID_D: &str = "00000000000000000000000000000002";
    const ID_E: &str = "00000000000000000000000000000003";

    fn lock_path(cache_dir: &Path, id: &str) -> PathBuf {
        cache_dir.join("mesh").join(format!("{id}.lock"))
    }

    #[test]
    fn acquire_writes_pid_and_drop_removes_lock() {
        let cache = TempDir::new("lock-basic");
        let path = lock_path(&cache.path, ID_A);
        assert!(
            !cache.path.join("mesh").exists(),
            "the mesh cache dir must not exist before a lock is taken"
        );

        let lock = InstanceLock::acquire(&cache.path, ID_A).unwrap();

        assert_eq!(lock.path, path);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );

        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn acquire_rejects_malformed_instance_id() {
        let cache = TempDir::new("lock-malformed");

        for bad in [
            "../../escape",
            "0123456789ABCDEF0123456789ABCDEF",
            "abc",
            "",
        ] {
            let err = InstanceLock::acquire(&cache.path, bad)
                .unwrap_err()
                .to_string();

            assert!(err.contains("malformed"), "{err}");
            assert!(err.contains("mesh_instance_id"), "{err}");
            assert!(err.contains(bad), "{err}");
        }

        assert!(
            !cache.path.join("mesh").exists(),
            "a rejected id must not create the mesh cache dir"
        );
        assert!(
            fs::read_dir(&cache.path).unwrap().next().is_none(),
            "a rejected id must not create anything under the cache dir"
        );
        assert!(!cache.path.join("../escape.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn acquire_refuses_lock_held_by_live_pid() {
        let cache = TempDir::new("lock-live");
        let path = lock_path(&cache.path, ID_B);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, std::process::id().to_string()).unwrap();

        let err = InstanceLock::acquire(&cache.path, ID_B)
            .unwrap_err()
            .to_string();

        assert!(err.contains(".mesh on --fresh"), "{err}");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "{err}"
        );
        assert!(
            path.exists(),
            "a refused acquire must leave the holder's lock alone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn acquire_reclaims_lock_left_by_dead_pid() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        assert!(!process_is_alive(dead_pid).unwrap());

        let cache = TempDir::new("lock-dead");
        let path = lock_path(&cache.path, ID_C);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, dead_pid.to_string()).unwrap();

        let _lock = InstanceLock::acquire(&cache.path, ID_C).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn acquire_reclaims_lock_holding_pid_zero() {
        let cache = TempDir::new("lock-pid-zero");
        let path = lock_path(&cache.path, ID_E);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "0").unwrap();

        let _lock = InstanceLock::acquire(&cache.path, ID_E).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn acquire_reclaims_empty_lock() {
        let cache = TempDir::new("lock-empty");
        let path = lock_path(&cache.path, ID_D);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();

        let _lock = InstanceLock::acquire(&cache.path, ID_D).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_is_alive_reports_current_process() {
        assert!(process_is_alive(std::process::id()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn process_is_alive_treats_pid_zero_as_dead() {
        assert!(!process_is_alive(0).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn process_is_alive_treats_out_of_range_pid_as_dead() {
        assert!(!process_is_alive(u32::MAX - 1).unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn lock_dir_is_mesh_under_cache_dir() {
        let cache = TempDir::new("lock-dir");
        let _env =
            crate::testing::EnvVarGuard::set(crate::utils::get_env_name("cache_dir"), &cache.path);

        assert_eq!(lock_dir(), cache.path.join("mesh"));
        assert!(!lock_dir().exists());
    }
}
