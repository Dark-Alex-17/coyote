use anyhow::{Context, Result, bail};
use log::{debug, warn};
use std::fs::{DirBuilder, File, OpenOptions, TryLockError, remove_file, rename};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Distinguishes the temp files of concurrent `write_private_atomic` calls
/// in one process, so one writer's stale-temp retry cannot unlink another
/// writer's in-flight temp file for the same target.
static WRITE_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_temp_path(path: &Path, seq: u64) -> PathBuf {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".{}.{seq}.tmp", std::process::id()));
    PathBuf::from(tmp_name)
}

/// Current value of the temp-file sequence; a test reads it to predict the
/// temp path the next `write_private_atomic` call will use. Only meaningful
/// under `#[serial]`.
#[cfg(test)]
pub(crate) fn next_temp_seq() -> u64 {
    WRITE_TEMP_SEQ.load(Ordering::Relaxed)
}

/// Writes `content` to `path` through an owner-only (0600) sibling temp file
/// and renames it over the target, so readers see either the old file or the
/// complete new one, and a symlink planted at either path is replaced rather
/// than followed. The temp file is fsynced before the rename so a crash after
/// the rename is journaled cannot leave the target truncated or empty. A stale
/// temp entry is removed and the open retried once.
pub(crate) fn write_private_atomic(path: &Path, content: &str) -> io::Result<()> {
    let seq = WRITE_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = write_temp_path(path, seq);
    let open_tmp = || {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&tmp_path)
    };
    let mut tmp_file = match open_tmp() {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            remove_file(&tmp_path)?;
            open_tmp()?
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = tmp_file
        .write_all(content.as_bytes())
        .and_then(|_| tmp_file.sync_all())
    {
        drop(tmp_file);
        let _ = remove_file(&tmp_path);
        return Err(error);
    }
    drop(tmp_file);
    if let Err(error) = rename(&tmp_path, path) {
        let _ = remove_file(&tmp_path);
        return Err(error);
    }
    Ok(())
}

/// `create_dir_all` with every directory it creates owner-only (0700).
pub(crate) fn create_private_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// True when a writer must leave `dir` alone: it is a symlink, whose
/// target could be anything, or it cannot be inspected at all. A missing
/// directory is not refused. The refusal is logged with `reason`, the
/// caller's word for what it was about to do.
pub(crate) fn refuse_symlinked_dir(dir: &Path, reason: &str) -> bool {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            warn!(
                "Refusing to touch {} ({reason}): it is a symlink",
                dir.display()
            );
            true
        }
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            warn!("Failed to inspect {} ({reason}): {error}", dir.display());
            true
        }
    }
}

/// Errors when `path` is a symlink, so a file about to be opened for
/// writing is never a link to somewhere else. A missing file passes.
pub(crate) fn refuse_symlinked_file(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("{} is a symlink; expected a regular file", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect {}", path.display())),
    }
}

/// How long `ExclusiveLock::acquire` keeps retrying before it refuses.
/// Bounded because a tool call or REPL verb must never hang: a holder
/// keeps the lock for one read-modify-write of a small file, so a wait
/// this long means the holder is stuck, not busy.
pub(crate) const EXCLUSIVE_LOCK_WAIT: Duration = Duration::from_secs(2);
pub(crate) const EXCLUSIVE_LOCK_RETRY: Duration = Duration::from_millis(10);

/// Serialises the writers of one shared file across every Coyote process:
/// a kernel advisory lock on a sibling `.lock` file, held for one
/// read-modify-write and released when the guard drops. Readers never take
/// it; the guarded file is replaced by rename, so a reader sees either
/// whole version. The lock file is never removed: unlinking would race a
/// concurrent opener onto an orphaned inode, and a leftover unlocked file
/// is harmless because acquiring simply locks it again. It holds the
/// holder's pid, best effort, to word a refusal. A crashed holder is
/// released by the kernel when its handle closes. On Windows the lock is
/// mandatory rather than advisory: a second handle cannot read the locked
/// region, so the pid may be unreadable there, which only shortens the
/// refusal; and a directory holding an open lock handle cannot be removed,
/// so a caller that goes on to delete the directory must drop the guard
/// first.
#[derive(Debug)]
pub(crate) struct ExclusiveLock {
    file: File,
    path: PathBuf,
}

impl ExclusiveLock {
    /// Waits up to `wait`, retrying every `EXCLUSIVE_LOCK_RETRY`, then
    /// refuses naming `what` (what the lock guards) and the holder when
    /// its pid can be read.
    pub(crate) fn acquire(lock_path: &Path, what: &str, wait: Duration) -> Result<Self> {
        let mut file = open_lock_file(lock_path)?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(EXCLUSIVE_LOCK_RETRY);
                }
                Err(TryLockError::WouldBlock) => bail!(
                    "The {what} lock '{}' is held{}; try again.",
                    lock_path.display(),
                    holder_suffix(read_holder_pid(&mut file))
                ),
                Err(TryLockError::Error(error)) => {
                    return Err(error).with_context(|| {
                        format!("Failed to take the {what} lock '{}'", lock_path.display())
                    });
                }
            }
        }
        if let Err(error) = write_holder_pid(&mut file) {
            debug!(
                "The {what} lock '{}' holds no pid: {error}",
                lock_path.display()
            );
        }
        Ok(Self {
            file,
            path: lock_path.to_path_buf(),
        })
    }
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            warn!(
                "Failed to release the lock '{}': {error}",
                self.path.display()
            );
        }
    }
}

fn holder_suffix(pid: Option<u32>) -> String {
    pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
}

/// Opens `lock_path` without taking the lock. The caller owns the
/// directory: a missing parent is an error, so a caller that checked for
/// the directory never recreates it by locking. The file holds a pid and
/// nothing secret, so on Windows it keeps the permissions it inherits
/// from the directory.
fn open_lock_file(lock_path: &Path) -> Result<File> {
    refuse_symlinked_file(lock_path)?;
    open_lock_file_unchecked(lock_path)
}

/// The open behind `open_lock_file`. On unix the kernel refuses to follow
/// a symlink too, so a link planted between the lstat check and the open
/// is still refused, with the same wording as the check.
fn open_lock_file_unchecked(lock_path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(lock_path) {
        Ok(file) => Ok(file),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            bail!(
                "{} is a symlink; expected a regular file",
                lock_path.display()
            )
        }
        Err(error) => {
            Err(error).with_context(|| format!("Failed to open lock file {}", lock_path.display()))
        }
    }
}

/// For `read_holder_pid` in a process that finds the lock taken.
fn write_holder_pid(file: &mut File) -> io::Result<()> {
    file.set_len(0)?;
    file.write_all(std::process::id().to_string().as_bytes())
}

/// The content may be empty or partial while the holder is between
/// `try_lock` and finishing its write, so `None` covers that window as
/// well as unparseable leftovers and Windows, where reading a region
/// another handle has locked fails. A killed holder never reaches this
/// path: its death released the kernel lock. The pid only words the
/// refusal, so a stale or reused pid costs nothing but a misleading hint.
fn read_holder_pid(file: &mut File) -> Option<u32> {
    let mut content = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut content).ok()?;
    content.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("coyote-fs-{label}-{}-{unique}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_entries(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[serial]
    fn writes_content_privately_and_leaves_no_temp() {
        let dir = TempDir::new("write");
        let target = dir.0.join("out.yaml");
        write_private_atomic(&target, "hello: world\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello: world\n");
        #[cfg(unix)]
        assert_eq!(mode_of(&target), 0o600);
        assert!(temp_entries(&dir.0).is_empty());
    }

    #[test]
    #[serial]
    fn overwrites_existing_file() {
        let dir = TempDir::new("overwrite");
        let target = dir.0.join("out.yaml");
        fs::write(&target, "old").unwrap();
        write_private_atomic(&target, "new").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(temp_entries(&dir.0).is_empty());
    }

    #[test]
    #[serial]
    fn stale_temp_is_removed_and_retried() {
        let dir = TempDir::new("stale");
        let target = dir.0.join("out.yaml");
        let stale = write_temp_path(&target, next_temp_seq());
        fs::write(&stale, "junk").unwrap();
        write_private_atomic(&target, "fresh").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "fresh");
        assert!(!stale.exists());
        assert!(temp_entries(&dir.0).is_empty());
    }

    #[test]
    fn creates_nested_private_dirs() {
        let dir = TempDir::new("dirs");
        let leaf = dir.0.join("a").join("b").join("c");
        create_private_dir_all(&leaf).unwrap();
        assert!(leaf.is_dir());
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&leaf), 0o700);
            assert_eq!(mode_of(&dir.0.join("a").join("b")), 0o700);
            assert_eq!(mode_of(&dir.0.join("a")), 0o700);
        }
    }

    const LOCK_WHAT: &str = "test record";

    #[test]
    fn exclusive_lock_refuses_a_held_lock_naming_the_holder_and_releases_on_drop() {
        let dir = TempDir::new("lock-held");
        let path = dir.0.join("record.lock");
        let held = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO).unwrap();

        let error = ExclusiveLock::acquire(&path, LOCK_WHAT, EXCLUSIVE_LOCK_RETRY * 3)
            .unwrap_err()
            .to_string();

        assert!(error.contains("The test record lock"), "{error}");
        assert!(error.contains("is held"), "{error}");
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.ends_with("; try again."), "{error}");
        // The lock is mandatory on Windows, so the refused opener cannot
        // read the pid the holder wrote.
        #[cfg(unix)]
        assert!(
            error.contains(&format!("(pid {})", std::process::id())),
            "{error}"
        );

        drop(held);
        let _again = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO).unwrap();
        assert!(path.exists(), "releasing must not unlink the lock file");
    }

    #[test]
    fn exclusive_lock_is_taken_once_the_holder_lets_go() {
        let dir = TempDir::new("lock-handover");
        let path = dir.0.join("record.lock");
        let held = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO).unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(EXCLUSIVE_LOCK_RETRY * 2);
            drop(held);
        });

        let taken = ExclusiveLock::acquire(&path, LOCK_WHAT, EXCLUSIVE_LOCK_WAIT);

        assert!(taken.is_ok(), "{:?}", taken.err());
        releaser.join().unwrap();
    }

    #[test]
    fn exclusive_lock_records_the_holder_pid_in_a_pre_created_parent() {
        let dir = TempDir::new("lock-parent");
        let parent = dir.0.join("deeper");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("record.lock");
        #[cfg(unix)]
        let parent_mode = mode_of(&parent);

        let mut held = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO).unwrap();

        assert_eq!(read_holder_pid(&mut held.file), Some(std::process::id()));
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&path), 0o600);
            assert_eq!(mode_of(&parent), parent_mode);
        }
    }

    #[test]
    fn exclusive_lock_never_creates_a_missing_parent() {
        let dir = TempDir::new("lock-no-parent");
        let path = dir.0.join("deeper").join("record.lock");

        let error = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO).unwrap_err();

        let cause = error.root_cause().downcast_ref::<io::Error>();
        assert_eq!(
            cause.map(io::Error::kind),
            Some(io::ErrorKind::NotFound),
            "{error:#}"
        );
        assert!(
            format!("{error:#}").contains("Failed to open lock file"),
            "{error:#}"
        );
        assert!(!dir.0.join("deeper").exists());
    }

    #[test]
    fn exclusive_lock_takes_over_an_unlocked_file_with_stale_content() {
        let dir = TempDir::new("lock-stale");
        let path = dir.0.join("record.lock");
        fs::write(&path, "4000000000 leftover").unwrap();

        let mut held = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO).unwrap();

        assert_eq!(read_holder_pid(&mut held.file), Some(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_lock_refuses_a_symlinked_lock_file() {
        let dir = TempDir::new("lock-symlink");
        let target = dir.0.join("elsewhere");
        fs::write(&target, "").unwrap();
        let path = dir.0.join("record.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let error = ExclusiveLock::acquire(&path, LOCK_WHAT, Duration::ZERO)
            .unwrap_err()
            .to_string();

        assert!(error.contains("is a symlink"), "{error}");
        assert_symlink_target_untouched(&path, &target);
    }

    /// Goes straight to the open, skipping the lstat check, the way a
    /// link planted between the check and the open would be met.
    #[cfg(unix)]
    #[test]
    fn exclusive_lock_refuses_a_symlinked_lock_file_even_without_the_lstat_check() {
        let dir = TempDir::new("lock-symlink-open");
        let target = dir.0.join("elsewhere");
        fs::write(&target, "").unwrap();
        let path = dir.0.join("record.lock");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let error = open_lock_file_unchecked(&path).unwrap_err().to_string();

        assert!(error.contains("is a symlink"), "{error}");
        assert_symlink_target_untouched(&path, &target);
    }

    #[cfg(unix)]
    fn assert_symlink_target_untouched(link: &Path, target: &Path) {
        assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(fs::metadata(target).unwrap().len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinked_paths_pass_regular_and_missing_entries_only() {
        let dir = TempDir::new("refuse-symlink");
        let file = dir.0.join("file");
        fs::write(&file, "x").unwrap();
        let sub = dir.0.join("sub");
        fs::create_dir(&sub).unwrap();
        let file_link = dir.0.join("file-link");
        std::os::unix::fs::symlink(&file, &file_link).unwrap();
        let dir_link = dir.0.join("dir-link");
        std::os::unix::fs::symlink(&sub, &dir_link).unwrap();

        assert!(refuse_symlinked_file(&file).is_ok());
        assert!(refuse_symlinked_file(&dir.0.join("missing")).is_ok());
        assert!(refuse_symlinked_file(&file_link).is_err());
        assert!(!refuse_symlinked_dir(&sub, "test"));
        assert!(!refuse_symlinked_dir(&dir.0.join("missing"), "test"));
        assert!(refuse_symlinked_dir(&dir_link, "test"));
    }
}
