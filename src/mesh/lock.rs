use crate::config::Session;
use crate::mesh::identity::create_private_dir;
use crate::mesh::redact_hashes;
use crate::mesh::{mesh_cache_dir, refuse_symlink};

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

/// Marks one session's mesh instance as owned by this process through a kernel advisory lock
/// on `<cache_dir>/mesh/<instance_id>.lock`. Dropping the guard unlocks explicitly, and a
/// crashed holder is released by the kernel when its handle closes, so no cleanup is needed.
/// The file itself is never removed: unlinking would race a concurrent opener onto an
/// orphaned inode, and a leftover unlocked file is harmless because acquiring simply locks
/// it again. On Windows the lock is mandatory, so a refusal there may omit the holder's pid:
/// the locked file cannot be read through another handle.
#[derive(Debug)]
pub(crate) struct InstanceLock {
    file: File,
}

impl InstanceLock {
    /// Claims the lock for `instance_id`, refusing while any process, including this one,
    /// holds it.
    pub(crate) fn acquire(cache_dir: &Path, instance_id: &str) -> Result<Self> {
        if !Session::is_valid_mesh_instance_id(instance_id) {
            bail!(
                "Mesh instance id '{instance_id}' is malformed: expected 32 lowercase hex characters. The session file's `mesh_instance_id` is corrupt; remove that line from the session file to mint a fresh id."
            );
        }
        let dir = mesh_cache_dir(cache_dir);
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create directory '{}'", dir.display()))?;
        let path = dir.join(format!("{instance_id}.lock"));

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Failed to open mesh instance lock '{}'", path.display()))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => match read_holder_pid(&mut file) {
                Some(pid) if pid == std::process::id() => bail!(
                    "This session already has mesh enabled in this Coyote process. Run `.mesh off` first, then `.mesh on` to start it again."
                ),
                Some(pid) => bail!(
                    "This session already has mesh enabled in another Coyote process (pid {pid}). Run `.mesh on --fresh` here to join the mesh with a new ephemeral destination, or turn mesh off in the other process first."
                ),
                None => bail!(
                    "This session already has mesh enabled in another Coyote process. Run `.mesh on --fresh` here to join the mesh with a new ephemeral destination, or turn mesh off in the other process first."
                ),
            },
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| {
                    format!("Failed to take the mesh instance lock '{}'", path.display())
                });
            }
        }

        write_holder_pid(&mut file)
            .with_context(|| format!("Failed to write mesh instance lock '{}'", path.display()))?;
        Ok(Self { file })
    }

    #[cfg(test)]
    fn holder_pid(&mut self) -> Option<u32> {
        read_holder_pid(&mut self.file)
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Err(err) = self.file.unlock() {
            warn!(
                "Failed to release a mesh instance lock: {}",
                redact_hashes(&err.to_string())
            );
        }
    }
}

/// How long `ExclusiveLock::acquire` keeps retrying before it refuses. Bounded because a
/// REPL verb must never hang: the holder keeps the lock for one read-modify-write of a
/// small file, so a wait this long means the holder is stuck, not busy.
pub(super) const EXCLUSIVE_LOCK_WAIT: Duration = Duration::from_secs(2);
pub(super) const EXCLUSIVE_LOCK_RETRY: Duration = Duration::from_millis(10);

/// Serialises the writers of one shared file across every Coyote process on a config
/// dir: a kernel advisory lock on the file's `.lock` sibling, held for one
/// read-modify-write and dropped with it. Readers never take it; the file is replaced by
/// rename, so a reader sees either whole version. The lock file is never removed, for
/// the reason `InstanceLock` gives, and holds the holder's pid best effort to word a
/// refusal. On Windows the lock is mandatory and the pid may be unreadable, which only
/// shortens the refusal.
#[derive(Debug)]
pub(super) struct ExclusiveLock {
    file: File,
}

impl ExclusiveLock {
    /// Waits up to `wait`, retrying every `EXCLUSIVE_LOCK_RETRY`, then refuses naming
    /// `what` (the list the lock guards) and the holder when its pid can be read.
    pub(super) fn acquire(lock_path: &Path, what: &str, wait: Duration) -> Result<Self> {
        let mut file = open_lock_file(lock_path)?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(EXCLUSIVE_LOCK_RETRY);
                }
                Err(TryLockError::WouldBlock) => bail!(
                    "Another Coyote process holds the mesh {what} lock '{}'{}; try again.",
                    lock_path.display(),
                    holder_suffix(read_holder_pid(&mut file))
                ),
                Err(TryLockError::Error(err)) => {
                    return Err(err).with_context(|| {
                        format!(
                            "Failed to take the mesh {what} lock '{}'",
                            lock_path.display()
                        )
                    });
                }
            }
        }
        if let Err(err) = write_holder_pid(&mut file) {
            debug!(
                "Mesh {what} lock '{}' holds no pid: {}",
                lock_path.display(),
                redact_hashes(&err.to_string())
            );
        }
        Ok(Self { file })
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        if let Err(err) = self.file.unlock() {
            warn!(
                "Failed to release a mesh file lock: {}",
                redact_hashes(&err.to_string())
            );
        }
    }
}

pub(super) fn holder_suffix(pid: Option<u32>) -> String {
    pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
}

/// Opens `lock_path` without taking the lock. The parent is created first (owner-only on
/// unix) because the lock may be taken before the first write creates the file it
/// guards. The file holds a pid and nothing secret, so on Windows it keeps the
/// permissions it inherits from the directory.
pub(super) fn open_lock_file(lock_path: &Path) -> Result<File> {
    if let Some(parent) = lock_path.parent() {
        create_private_dir(parent)?;
    }
    refuse_symlink(lock_path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(lock_path)
        .with_context(|| format!("Failed to open mesh lock '{}'", lock_path.display()))
}

/// For `read_holder_pid` in a process that finds the lock taken.
pub(super) fn write_holder_pid(file: &mut File) -> std::io::Result<()> {
    file.set_len(0)?;
    file.write_all(std::process::id().to_string().as_bytes())
}

/// The content may be empty or partial while the holder is between `try_lock` and finishing
/// its write, so `None` covers that window as well as unparseable leftovers and Windows,
/// where reading a region another handle has locked fails. A killed holder never reaches
/// this path: its death released the kernel lock. The pid is only ever used to word the
/// refusal, so a stale or reused pid costs nothing but a misleading hint.
pub(super) fn read_holder_pid(file: &mut File) -> Option<u32> {
    let mut content = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut content).ok()?;
    content.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use std::path::PathBuf;

    const ID_A: &str = "0123456789abcdef0123456789abcdef";
    const ID_B: &str = "fedcba9876543210fedcba9876543210";
    const ID_C: &str = "00000000000000000000000000000001";
    const ID_D: &str = "00000000000000000000000000000002";

    fn lock_path(cache_dir: &Path, id: &str) -> PathBuf {
        cache_dir.join("mesh").join(format!("{id}.lock"))
    }

    #[test]
    fn acquire_writes_pid() {
        let cache = TempDir::new("lock-basic");
        assert!(
            !cache.path.join("mesh").exists(),
            "the mesh cache dir must not exist before a lock is taken"
        );

        let mut lock = InstanceLock::acquire(&cache.path, ID_A).unwrap();

        assert!(lock_path(&cache.path, ID_A).exists());
        assert_eq!(lock.holder_pid(), Some(std::process::id()));
    }

    // LockFileEx is mandatory: on Windows a second handle cannot read the locked region.
    #[cfg(unix)]
    #[test]
    fn acquire_writes_pid_readable_through_another_handle() {
        let cache = TempDir::new("lock-basic-second-handle");
        let path = lock_path(&cache.path, ID_A);

        let _lock = InstanceLock::acquire(&cache.path, ID_A).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
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

    // LockFileEx is mandatory: on Windows a second handle cannot read the locked region, so
    // the refusal there cannot tell this process from another.
    #[cfg(unix)]
    #[test]
    fn second_acquire_in_same_process_fails_naming_this_process_and_remedy() {
        let cache = TempDir::new("lock-held");
        let path = lock_path(&cache.path, ID_B);
        let _held = InstanceLock::acquire(&cache.path, ID_B).unwrap();

        let err = InstanceLock::acquire(&cache.path, ID_B)
            .unwrap_err()
            .to_string();

        assert!(err.contains("in this Coyote process"), "{err}");
        assert!(err.contains(".mesh off"), "{err}");
        assert!(!err.contains("--fresh"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            std::process::id().to_string(),
            "a refused acquire must leave the holder's pid alone"
        );
    }

    #[test]
    fn second_acquire_of_held_lock_fails_naming_remedy() {
        let cache = TempDir::new("lock-held-portable");
        let mut held = InstanceLock::acquire(&cache.path, ID_B).unwrap();

        let err = InstanceLock::acquire(&cache.path, ID_B)
            .unwrap_err()
            .to_string();

        assert!(err.contains("already has mesh enabled"), "{err}");
        assert!(
            err.contains(".mesh off") || err.contains(".mesh on --fresh"),
            "{err}"
        );
        assert_eq!(
            held.holder_pid(),
            Some(std::process::id()),
            "a refused acquire must leave the holder's pid alone"
        );
    }

    #[test]
    fn drop_releases_lock_so_reacquire_succeeds() {
        let cache = TempDir::new("lock-release");
        let path = lock_path(&cache.path, ID_C);
        let lock = InstanceLock::acquire(&cache.path, ID_C).unwrap();

        drop(lock);

        assert!(path.exists(), "releasing must not unlink the lock file");
        let _again = InstanceLock::acquire(&cache.path, ID_C).unwrap();
    }

    #[test]
    fn acquire_takes_over_unlocked_file_with_stale_content() {
        let cache = TempDir::new("lock-stale");
        let path = lock_path(&cache.path, ID_D);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "4000000000 leftover from an older format").unwrap();

        let mut lock = InstanceLock::acquire(&cache.path, ID_D).unwrap();

        assert_eq!(lock.holder_pid(), Some(std::process::id()));
    }

    #[test]
    fn exclusive_lock_waits_for_the_holder_and_refuses_at_the_deadline() {
        let dir = TempDir::new("lock-exclusive-bounded");
        let path = dir.path.join("mesh").join("trust.yaml.lock");

        let held = ExclusiveLock::acquire(&path, "trust file", Duration::ZERO).unwrap();
        let started = Instant::now();
        let err = ExclusiveLock::acquire(&path, "trust file", Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        let waited = started.elapsed();

        assert!(err.contains("holds the mesh trust file lock"), "{err}");
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_secs(2),
            "waited {waited:?}"
        );
        drop(held);
        let _again = ExclusiveLock::acquire(&path, "trust file", Duration::ZERO).unwrap();
        assert!(path.exists(), "releasing must not unlink the lock file");
    }

    #[test]
    fn exclusive_lock_is_taken_once_the_holder_lets_go() {
        let dir = TempDir::new("lock-exclusive-handover");
        let path = dir.path.join("mesh").join("shares.yaml.lock");
        let held = ExclusiveLock::acquire(&path, "share list", Duration::ZERO).unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(held);
        });

        let started = Instant::now();
        let taken = ExclusiveLock::acquire(&path, "share list", EXCLUSIVE_LOCK_WAIT);

        assert!(taken.is_ok(), "{:?}", taken.err());
        assert!(started.elapsed() < EXCLUSIVE_LOCK_WAIT);
        releaser.join().unwrap();
    }
}
