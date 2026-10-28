use std::fs::{DirBuilder, OpenOptions, copy, read_dir, remove_file, rename};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use log::warn;

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

/// Copies the regular files under `src` into `dst`, recreating directories
/// owner-only. A symlink is skipped with a warning rather than followed, and
/// an entry `skip` accepts is left out together with everything beneath it.
pub(crate) fn copy_dir_all(src: &Path, dst: &Path, skip: &dyn Fn(&Path) -> bool) -> io::Result<()> {
    create_private_dir_all(dst)?;
    for entry in read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        if skip(&path) {
            continue;
        }
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_symlink() {
            warn!(
                "skip symlink {} while copying {}",
                path.display(),
                src.display()
            );
        } else if file_type.is_dir() {
            copy_dir_all(&path, &target, skip)?;
        } else if file_type.is_file() {
            copy(&path, &target)?;
        }
    }
    Ok(())
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

    #[test]
    fn copy_dir_all_copies_regular_files_and_leaves_out_skipped_entries() {
        let dir = TempDir::new("copy");
        let src = dir.0.join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.md"), "a").unwrap();
        fs::write(src.join("nested").join("b.md"), "b").unwrap();
        fs::write(src.join(".catalogue.yaml"), "cache").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(src.join("a.md"), src.join("link.md")).unwrap();

        let dst = dir.0.join("backup").join("dst");
        let skip = |path: &Path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(".catalogue."))
        };
        copy_dir_all(&src, &dst, &skip).unwrap();

        assert_eq!(fs::read_to_string(dst.join("a.md")).unwrap(), "a");
        assert_eq!(
            fs::read_to_string(dst.join("nested").join("b.md")).unwrap(),
            "b"
        );
        assert!(!dst.join(".catalogue.yaml").exists());
        assert!(fs::symlink_metadata(dst.join("link.md")).is_err());
        #[cfg(unix)]
        assert_eq!(mode_of(&dst), 0o700);
    }
}
