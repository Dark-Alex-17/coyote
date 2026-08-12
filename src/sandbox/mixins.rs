use std::env;
use std::fs;
use std::fs::{read_dir, read_to_string};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde_yaml::Value;
use sha2::{Digest, Sha256};

use crate::config::paths;

const SBX_MIXIN_FILE_NAME: &str = "sbx-mixin.yaml";
const SBX_MIXIN_FILE_SUFFIX: &str = ".sbx-mixin.yaml";
const KIT_SPEC_FILE_NAME: &str = "spec.yaml";
const MIXIN_FILES_DIR_NAME: &str = "files";

#[derive(Debug, Clone)]
pub struct DiscoveredMixin {
    pub path: PathBuf,
    pub label: String,
    pub install_count: usize,
    pub domain_count: usize,
}

impl DiscoveredMixin {
    pub fn kit_path(&self) -> Result<PathBuf> {
        if self.path.is_dir() {
            return Ok(self.path.clone());
        }

        wrap_mixin_as_kit(&self.path)
    }
}

pub fn wrap_mixin_as_kit(mixin_path: &Path) -> Result<PathBuf> {
    let bytes = fs::read(mixin_path)
        .with_context(|| format!("Failed to read sbx mixin {}", mixin_path.display()))?;
    let label = mixin_path.display().to_string();

    let files = mixin_path
        .parent()
        .map(|p| p.join(MIXIN_FILES_DIR_NAME))
        .filter(|p| p.is_dir())
        .map(|dir| collect_staged_files(&dir))
        .transpose()?
        .unwrap_or_default();

    stage_kit(&bytes, &files, &label)
}

pub fn wrap_mixin_bytes_as_kit(bytes: &[u8], label: &str) -> Result<PathBuf> {
    stage_kit(bytes, &[], label)
}

struct StagedFile {
    relpath: PathBuf,
    mode: u32,
    bytes: Vec<u8>,
}

fn stage_kit(spec_bytes: &[u8], files: &[StagedFile], label: &str) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(spec_bytes);
    for f in files {
        let rel_str = f.relpath.to_str().ok_or_else(|| {
            anyhow!(
                "Non-UTF-8 path inside mixin {MIXIN_FILES_DIR_NAME}/: {}",
                f.relpath.display()
            )
        })?;
        hasher.update(b"\0COYOTE_MIXIN_FILE\0");
        hasher.update((rel_str.len() as u64).to_le_bytes());
        hasher.update(rel_str.as_bytes());
        hasher.update(f.mode.to_le_bytes());
        hasher.update((f.bytes.len() as u64).to_le_bytes());
        hasher.update(&f.bytes);
    }
    let hash = format!("{:x}", hasher.finalize());

    let kit_dir = paths::sbx_mixin_kits_dir().join(&hash);
    let spec_path = kit_dir.join(KIT_SPEC_FILE_NAME);
    let files_dst = kit_dir.join(MIXIN_FILES_DIR_NAME);

    let spec_matches = fs::read(&spec_path).is_ok_and(|existing| existing == spec_bytes);
    let files_ready = files.is_empty() || files_dst.is_dir();
    if spec_matches && files_ready {
        return Ok(kit_dir);
    }

    fs::create_dir_all(&kit_dir)
        .with_context(|| format!("Failed to create mixin kit dir {}", kit_dir.display()))?;
    fs::write(&spec_path, spec_bytes)
        .with_context(|| format!("Failed to write {}", spec_path.display()))?;

    if !files.is_empty() {
        if files_dst.exists() {
            fs::remove_dir_all(&files_dst).with_context(|| {
                format!(
                    "Failed to clear stale mixin files at {}",
                    files_dst.display()
                )
            })?;
        }
        for f in files {
            let dst = files_dst.join(&f.relpath);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create dir {}", parent.display()))?;
            }
            fs::write(&dst, &f.bytes)
                .with_context(|| format!("Failed to write staged mixin file {}", dst.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dst, fs::Permissions::from_mode(f.mode))
                    .with_context(|| format!("Failed to set mode on {}", dst.display()))?;
            }
        }
    }

    debug!("Wrapped mixin {label} as kit at {}", kit_dir.display());

    Ok(kit_dir)
}

fn collect_staged_files(root: &Path) -> Result<Vec<StagedFile>> {
    let mut out = Vec::new();
    walk_staged_files(root, Path::new(""), &mut out)?;
    Ok(out)
}

fn walk_staged_files(abs_dir: &Path, rel_dir: &Path, out: &mut Vec<StagedFile>) -> Result<()> {
    let rd = fs::read_dir(abs_dir)
        .with_context(|| format!("Failed to read mixin files dir {}", abs_dir.display()))?;
    let mut entries: Vec<_> = rd
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("Failed to iterate mixin files dir {}", abs_dir.display()))?;
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to stat {}", entry.path().display()))?;
        let abs = entry.path();
        let rel = rel_dir.join(entry.file_name());

        if file_type.is_symlink() {
            bail!(
                "Symlinks are not allowed inside a mixin {MIXIN_FILES_DIR_NAME}/ tree: {}",
                abs.display()
            );
        }

        if file_type.is_dir() {
            walk_staged_files(&abs, &rel, out)?;
        } else if file_type.is_file() {
            let bytes = fs::read(&abs)
                .with_context(|| format!("Failed to read staged mixin file {}", abs.display()))?;
            let mode = staged_file_mode(&entry)?;
            out.push(StagedFile {
                relpath: rel,
                mode,
                bytes,
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
fn staged_file_mode(entry: &fs::DirEntry) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    let meta = entry
        .metadata()
        .with_context(|| format!("Failed to stat {}", entry.path().display()))?;
    Ok(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn staged_file_mode(_entry: &fs::DirEntry) -> Result<u32> {
    Ok(0o644)
}

pub fn discover() -> Result<Vec<DiscoveredMixin>> {
    let mut out = Vec::new();

    push_if_exists(&mut out, paths::sbx_mixin_file())?;
    push_if_exists(&mut out, paths::global_tools_sbx_mixin_file())?;

    for path in collect_mixins(&paths::functions_dir(), &[ScanMode::SubdirNamed]) {
        out.push(read_mixin(path)?);
    }
    for path in collect_mixins(
        &paths::agents_data_dir(),
        &[ScanMode::SubdirNamed, ScanMode::SubdirFlat],
    ) {
        out.push(read_mixin(path)?);
    }
    for path in collect_mixins(&paths::rags_dir(), &[ScanMode::Flat]) {
        out.push(read_mixin(path)?);
    }

    if let Ok(cwd) = env::current_dir()
        && let Some(path) = paths::find_workspace_sbx_mixin(&cwd)
    {
        out.push(read_mixin(path)?);
    }

    Ok(out)
}

pub fn summarize(path: &Path) -> Result<(usize, usize)> {
    let content = read_to_string(path)
        .with_context(|| format!("Failed to read sbx mixin {}", path.display()))?;
    let value: Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse sbx mixin {}", path.display()))?;

    let installs = value
        .get("setup")
        .and_then(|s| s.get("install"))
        .or_else(|| value.get("commands").and_then(|c| c.get("install")))
        .and_then(|i| i.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0);

    let domains = value
        .get("permissions")
        .and_then(|p| p.get("network"))
        .and_then(|n| n.get("allow"))
        .or_else(|| value.get("network").and_then(|n| n.get("allowedDomains")))
        .and_then(|d| d.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0);

    Ok((installs, domains))
}

pub fn log_discovery(mixins: &[DiscoveredMixin], disabled: bool) {
    if disabled {
        info!("Mixin discovery disabled via --no-mixins.");
        return;
    }

    if mixins.is_empty() {
        info!("No sbx mixins discovered.");
        return;
    }

    let header = format!("Applying {} sbx mixin(s):", mixins.len());
    info!("{header}");
    println!("{header}");

    for m in mixins {
        let line = format!(
            "  {}  (adds: {} install{}, {} domain{})",
            m.label,
            m.install_count,
            if m.install_count == 1 { "" } else { "s" },
            m.domain_count,
            if m.domain_count == 1 { "" } else { "s" },
        );
        info!("{line}");
        println!("{line}");
    }
}

fn push_if_exists(out: &mut Vec<DiscoveredMixin>, path: PathBuf) -> Result<()> {
    if path.exists() {
        out.push(read_mixin(path)?);
    }
    Ok(())
}

fn read_mixin(path: PathBuf) -> Result<DiscoveredMixin> {
    let label = path.display().to_string();
    let (install_count, domain_count) = summarize(&path)?;

    Ok(DiscoveredMixin {
        path,
        label,
        install_count,
        domain_count,
    })
}

/// One on-disk layout a mixin scan can look for. A scan takes a set of these,
/// and each mode contributes only the shape it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanMode {
    /// `<dir>/*.sbx-mixin.yaml`
    Flat,
    /// `<dir>/*/sbx-mixin.yaml`
    SubdirNamed,
    /// `<dir>/*/*.sbx-mixin.yaml`
    SubdirFlat,
}

/// Collects mixin paths under `dir` for every requested layout. Missing or
/// unreadable directories yield nothing rather than an error — these paths are
/// all optional on disk.
///
/// Order is deterministic: flat matches first (sorted by file name), then each
/// subdirectory in sorted order, contributing its named mixin before its
/// suffixed ones.
fn collect_mixins(dir: &Path, modes: &[ScanMode]) -> Vec<PathBuf> {
    let mut result = Vec::new();

    if modes.contains(&ScanMode::Flat) {
        result.extend(suffixed_mixins_in(dir));
    }

    let named = modes.contains(&ScanMode::SubdirNamed);
    let subdir_flat = modes.contains(&ScanMode::SubdirFlat);
    if !named && !subdir_flat {
        return result;
    }

    for subdir in subdirs_of(dir) {
        if named {
            let candidate = subdir.join(SBX_MIXIN_FILE_NAME);
            if candidate.exists() {
                result.push(candidate);
            }
        }
        if subdir_flat {
            result.extend(suffixed_mixins_in(&subdir));
        }
    }

    result
}

fn suffixed_mixins_in(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(rd) = read_dir(dir) else { return result };

    let mut entries: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(SBX_MIXIN_FILE_SUFFIX))
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    result.extend(entries.into_iter().map(|e| e.path()));
    result
}

fn subdirs_of(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(rd) = read_dir(dir) else { return result };

    let mut entries: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    result.extend(entries.into_iter().map(|e| e.path()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time;

    fn unique_root(prefix: &str) -> PathBuf {
        let nanos = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-{prefix}-{nanos}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn file_names(paths: &[PathBuf]) -> Vec<&str> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect()
    }

    #[test]
    fn summarize_counts_installs_and_domains() {
        let root = unique_root("sbx-mixin-counts");
        let path = root.join("sbx-mixin.yaml");
        fs::write(
            &path,
            r#"
schemaVersion: "2"
kind: mixin
setup:
  install:
    - command: "echo hi"
    - command: "echo bye"
permissions:
  network:
    allow:
      - "a.example.com:443"
      - "b.example.com:443"
      - "c.example.com:443"
"#,
        )
        .unwrap();

        assert_eq!(summarize(&path).unwrap(), (2, 3));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_falls_back_to_v1_field_paths() {
        let root = unique_root("sbx-mixin-counts-v1");
        let path = root.join("sbx-mixin.yaml");
        fs::write(
            &path,
            r#"
schemaVersion: "1"
kind: mixin
commands:
  install:
    - command: "echo hi"
    - command: "echo bye"
network:
  allowedDomains:
    - "a.example.com:443"
    - "b.example.com:443"
    - "c.example.com:443"
"#,
        )
        .unwrap();

        assert_eq!(summarize(&path).unwrap(), (2, 3));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_treats_missing_blocks_as_zero() {
        let root = unique_root("sbx-mixin-empty");
        let path = root.join("sbx-mixin.yaml");
        fs::write(&path, "schemaVersion: \"1\"\nkind: mixin\n").unwrap();

        assert_eq!(summarize(&path).unwrap(), (0, 0));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_returns_err_on_malformed_yaml() {
        let root = unique_root("sbx-mixin-bad");
        let path = root.join("sbx-mixin.yaml");
        fs::write(&path, "this: is: not: yaml: ::").unwrap();

        let err = summarize(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&path.display().to_string()),
            "expected error to mention path; got: {msg}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn subdir_named_scan_sorts_and_skips_missing() {
        let root = unique_root("sbx-mixin-subdirs");
        for name in ["zebra", "apple", "no-mixin", "mango"] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            if name != "no-mixin" {
                fs::write(dir.join("sbx-mixin.yaml"), "kind: mixin\n").unwrap();
            }
        }

        let found = collect_mixins(&root, &[ScanMode::SubdirNamed]);
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(names, vec!["apple", "mango", "zebra"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn subdir_named_scan_returns_empty_for_missing_dir() {
        let absent = env::temp_dir().join("coyote-definitely-not-here-xyz");
        let found = collect_mixins(&absent, &[ScanMode::SubdirNamed]);
        assert!(found.is_empty());
    }

    mod wrap_as_kit {
        use super::*;
        use serial_test::serial;
        use std::ffi::OsString;

        struct TestCacheDirGuard {
            key: String,
            previous: Option<OsString>,
            path: PathBuf,
        }

        impl TestCacheDirGuard {
            fn new() -> Self {
                let key = crate::utils::get_env_name("cache_dir");
                let previous = env::var_os(&key);
                let nanos = time::SystemTime::now()
                    .duration_since(time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = env::temp_dir().join(format!("coyote-mixin-wrap-cache-{nanos}"));
                fs::create_dir_all(&path).unwrap();
                unsafe {
                    env::set_var(&key, &path);
                }
                Self {
                    key,
                    previous,
                    path,
                }
            }
        }

        impl Drop for TestCacheDirGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.previous {
                        Some(v) => env::set_var(&self.key, v),
                        None => env::remove_var(&self.key),
                    }
                }
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        fn write_mixin(name: &str, content: &str) -> PathBuf {
            let root = unique_root(&format!("wrap-src-{name}"));
            let path = root.join("sbx-mixin.yaml");
            fs::write(&path, content).unwrap();
            path
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_creates_spec_yaml_with_original_content() {
            let _guard = TestCacheDirGuard::new();
            let content = "schemaVersion: \"1\"\nkind: mixin\nname: probe\n";
            let mixin = write_mixin("content", content);

            let kit_dir = wrap_mixin_as_kit(&mixin).unwrap();
            let spec = kit_dir.join("spec.yaml");

            assert!(spec.exists(), "spec.yaml must exist in wrapped kit dir");
            assert_eq!(fs::read_to_string(&spec).unwrap(), content);
        }

        #[test]
        #[serial]
        fn wrap_mixin_bytes_as_kit_writes_spec_yaml() {
            let _guard = TestCacheDirGuard::new();
            let content = b"schemaVersion: '2'\nkind: mixin\nname: generated\n";

            let kit_dir = wrap_mixin_bytes_as_kit(content, "generated").unwrap();
            let spec = kit_dir.join("spec.yaml");

            assert!(spec.exists(), "spec.yaml must exist in wrapped kit dir");
            assert_eq!(fs::read(&spec).unwrap(), content);
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_is_deterministic_for_identical_content() {
            let _guard = TestCacheDirGuard::new();
            let content = "schemaVersion: \"1\"\nkind: mixin\nname: probe\n";
            let mixin_one = write_mixin("dedup-1", content);
            let mixin_two = write_mixin("dedup-2", content);

            let kit_a = wrap_mixin_as_kit(&mixin_one).unwrap();
            let kit_b = wrap_mixin_as_kit(&mixin_two).unwrap();

            assert_eq!(
                kit_a, kit_b,
                "same content should share the same content-addressed kit dir"
            );
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_different_content_yields_different_dirs() {
            let _guard = TestCacheDirGuard::new();
            let mixin_a = write_mixin("diff-a", "kind: mixin\nname: a\n");
            let mixin_b = write_mixin("diff-b", "kind: mixin\nname: b\n");

            let kit_a = wrap_mixin_as_kit(&mixin_a).unwrap();
            let kit_b = wrap_mixin_as_kit(&mixin_b).unwrap();

            assert_ne!(
                kit_a, kit_b,
                "different content must hash to different kit dirs"
            );
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_is_idempotent_on_cache_hit() {
            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("idempotent", "kind: mixin\nname: probe\n");

            let kit_first = wrap_mixin_as_kit(&mixin).unwrap();
            let spec = kit_first.join("spec.yaml");
            let mtime_first = fs::metadata(&spec).unwrap().modified().unwrap();

            std::thread::sleep(std::time::Duration::from_millis(10));

            let kit_second = wrap_mixin_as_kit(&mixin).unwrap();
            let mtime_second = fs::metadata(kit_second.join("spec.yaml"))
                .unwrap()
                .modified()
                .unwrap();

            assert_eq!(kit_first, kit_second);
            assert_eq!(
                mtime_first, mtime_second,
                "cache hit must not rewrite spec.yaml"
            );
        }

        #[test]
        #[serial]
        fn kit_path_passes_through_existing_directory() {
            let _guard = TestCacheDirGuard::new();
            let dir = unique_root("kit-path-dir-passthrough");

            let m = DiscoveredMixin {
                path: dir.clone(),
                label: "vault".into(),
                install_count: 1,
                domain_count: 1,
            };

            assert_eq!(m.kit_path().unwrap(), dir);
        }

        #[test]
        #[serial]
        fn kit_path_wraps_file_into_kit_dir() {
            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("kit-path-wrap", "kind: mixin\nname: probe\n");

            let m = DiscoveredMixin {
                path: mixin.clone(),
                label: mixin.display().to_string(),
                install_count: 0,
                domain_count: 0,
            };

            let wrapped = m.kit_path().unwrap();
            assert!(wrapped.is_dir(), "kit_path of a file should be a directory");
            assert!(wrapped.join("spec.yaml").exists());
            assert_ne!(
                wrapped, mixin,
                "kit_path should not return the original file path"
            );
        }

        fn write_staged_file(mixin: &Path, rel: &str, content: &[u8]) {
            let dst = mixin.parent().unwrap().join(MIXIN_FILES_DIR_NAME).join(rel);
            fs::create_dir_all(dst.parent().unwrap()).unwrap();
            fs::write(&dst, content).unwrap();
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_copies_sibling_files_tree_into_kit() {
            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("files-copy", "kind: mixin\nname: probe\n");
            write_staged_file(&mixin, "home/hello.md", b"# hello\n");
            write_staged_file(&mixin, "home/nested/deep.txt", b"deep\n");

            let kit_dir = wrap_mixin_as_kit(&mixin).unwrap();

            assert!(kit_dir.join("spec.yaml").exists());
            let files_root = kit_dir.join(MIXIN_FILES_DIR_NAME);
            assert!(files_root.is_dir(), "kit dir must contain a files/ tree");
            assert_eq!(
                fs::read(files_root.join("home/hello.md")).unwrap(),
                b"# hello\n"
            );
            assert_eq!(
                fs::read(files_root.join("home/nested/deep.txt")).unwrap(),
                b"deep\n"
            );
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_hash_changes_when_a_staged_file_is_edited() {
            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("files-hash-content", "kind: mixin\nname: probe\n");
            write_staged_file(&mixin, "home/note.md", b"before\n");
            let kit_before = wrap_mixin_as_kit(&mixin).unwrap();

            write_staged_file(&mixin, "home/note.md", b"after\n");
            let kit_after = wrap_mixin_as_kit(&mixin).unwrap();

            assert_ne!(
                kit_before, kit_after,
                "editing a staged file must invalidate the kit hash"
            );
            assert_eq!(
                fs::read(kit_after.join("files/home/note.md")).unwrap(),
                b"after\n"
            );
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_hash_changes_when_a_staged_file_is_added() {
            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("files-hash-added", "kind: mixin\nname: probe\n");
            write_staged_file(&mixin, "home/one.md", b"one\n");
            let kit_before = wrap_mixin_as_kit(&mixin).unwrap();

            write_staged_file(&mixin, "home/two.md", b"two\n");
            let kit_after = wrap_mixin_as_kit(&mixin).unwrap();

            assert_ne!(
                kit_before, kit_after,
                "adding a staged file must invalidate the kit hash"
            );
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_hash_unchanged_when_no_files_dir() {
            let _guard = TestCacheDirGuard::new();
            let content = "kind: mixin\nname: legacy\n";
            let mixin = write_mixin("legacy-no-files", content);

            let with_helper = wrap_mixin_as_kit(&mixin).unwrap();
            let bytes_only = wrap_mixin_bytes_as_kit(content.as_bytes(), "legacy").unwrap();

            assert_eq!(
                with_helper, bytes_only,
                "mixins without a sibling files/ must keep the legacy bytes-only hash to reuse existing cache dirs"
            );
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_ignores_sibling_files_that_is_not_a_directory() {
            let _guard = TestCacheDirGuard::new();
            let content = "kind: mixin\nname: probe\n";
            let mixin = write_mixin("files-not-a-dir", content);
            fs::write(mixin.parent().unwrap().join(MIXIN_FILES_DIR_NAME), b"decoy").unwrap();

            let wrapped = wrap_mixin_as_kit(&mixin).unwrap();
            let bytes_only = wrap_mixin_bytes_as_kit(content.as_bytes(), "probe").unwrap();

            assert_eq!(
                wrapped, bytes_only,
                "a regular file named files must be ignored, not staged"
            );
            assert!(!wrapped.join(MIXIN_FILES_DIR_NAME).exists());
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_rebuilds_files_when_cache_dir_missing_files_tree() {
            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("files-rebuild", "kind: mixin\nname: probe\n");
            write_staged_file(&mixin, "home/hello.md", b"hi\n");

            let kit_dir = wrap_mixin_as_kit(&mixin).unwrap();
            let files_dst = kit_dir.join(MIXIN_FILES_DIR_NAME);
            fs::remove_dir_all(&files_dst).unwrap();
            assert!(!files_dst.exists());

            let kit_again = wrap_mixin_as_kit(&mixin).unwrap();

            assert_eq!(kit_again, kit_dir, "kit path is content-addressed");
            assert!(
                files_dst.is_dir(),
                "a partial cache (spec present, files/ missing) must be rebuilt"
            );
            assert_eq!(fs::read(files_dst.join("home/hello.md")).unwrap(), b"hi\n");
        }

        #[test]
        #[serial]
        fn wrap_mixin_as_kit_deterministic_with_staged_files() {
            let _guard = TestCacheDirGuard::new();
            let content = "kind: mixin\nname: probe\n";
            let mixin_one = write_mixin("determ-1", content);
            write_staged_file(&mixin_one, "home/note.md", b"same\n");
            let mixin_two = write_mixin("determ-2", content);
            write_staged_file(&mixin_two, "home/note.md", b"same\n");

            let kit_a = wrap_mixin_as_kit(&mixin_one).unwrap();
            let kit_b = wrap_mixin_as_kit(&mixin_two).unwrap();

            assert_eq!(
                kit_a, kit_b,
                "identical spec+files must produce the same content-addressed kit dir"
            );
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn wrap_mixin_as_kit_rejects_symlinks_inside_files_tree() {
            use std::os::unix::fs::symlink;

            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("files-symlink", "kind: mixin\nname: probe\n");
            let files_dir = mixin.parent().unwrap().join(MIXIN_FILES_DIR_NAME);
            fs::create_dir_all(&files_dir).unwrap();
            let target = files_dir.join("target.txt");
            fs::write(&target, b"real").unwrap();
            symlink(&target, files_dir.join("link.txt")).unwrap();

            let err = wrap_mixin_as_kit(&mixin).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("Symlinks are not allowed"),
                "expected symlink rejection, got: {msg}"
            );
        }

        #[cfg(unix)]
        #[test]
        #[serial]
        fn wrap_mixin_as_kit_preserves_executable_bit() {
            use std::os::unix::fs::PermissionsExt;

            let _guard = TestCacheDirGuard::new();
            let mixin = write_mixin("files-exec", "kind: mixin\nname: probe\n");
            write_staged_file(&mixin, "bin/run.sh", b"#!/bin/sh\necho hi\n");
            let src = mixin
                .parent()
                .unwrap()
                .join(MIXIN_FILES_DIR_NAME)
                .join("bin/run.sh");
            fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();

            let kit_dir = wrap_mixin_as_kit(&mixin).unwrap();
            let dst = kit_dir.join("files/bin/run.sh");
            let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o777;

            assert_eq!(
                mode, 0o755,
                "executable bit must survive the copy into the kit dir"
            );
        }
    }

    #[test]
    fn flat_scan_matches_rag_sidecars_by_suffix() {
        let root = unique_root("flat-mixins");
        fs::write(root.join("company-docs.sbx-mixin.yaml"), "kind: mixin\n").unwrap();
        fs::write(root.join("alpha.sbx-mixin.yaml"), "kind: mixin\n").unwrap();
        fs::write(root.join("company-docs.yaml"), "driver: qdrant\n").unwrap();
        fs::write(root.join("notes.yaml"), "driver: yaml\n").unwrap();
        fs::create_dir_all(root.join("decoy.sbx-mixin.yaml")).unwrap();

        let found = collect_mixins(&root, &[ScanMode::Flat]);
        assert_eq!(
            file_names(&found),
            vec!["alpha.sbx-mixin.yaml", "company-docs.sbx-mixin.yaml"]
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// Every scan site in `discover()` picks its modes assuming each mode owns
    /// exactly one layout and nothing else. `agents_data_dir()` requests two
    /// modes at once, so an overlap would collect the same file twice and
    /// `create_sandbox` would pass it as two `--kit` flags.
    #[test]
    fn each_scan_mode_owns_exactly_one_layout() {
        let root = unique_root("scan-mode-ownership");
        let agent = root.join("researcher");
        fs::create_dir_all(&agent).unwrap();
        let flat = root.join("company-docs.sbx-mixin.yaml");
        let subdir_named = agent.join("sbx-mixin.yaml");
        let subdir_flat = agent.join("handbook.sbx-mixin.yaml");
        for path in [&flat, &subdir_named, &subdir_flat] {
            fs::write(path, "kind: mixin\n").unwrap();
        }

        assert_eq!(collect_mixins(&root, &[ScanMode::Flat]), vec![flat.clone()]);
        assert_eq!(
            collect_mixins(&root, &[ScanMode::SubdirNamed]),
            vec![subdir_named.clone()]
        );
        assert_eq!(
            collect_mixins(&root, &[ScanMode::SubdirFlat]),
            vec![subdir_flat.clone()]
        );

        let all = collect_mixins(
            &root,
            &[ScanMode::Flat, ScanMode::SubdirNamed, ScanMode::SubdirFlat],
        );
        assert_eq!(all, vec![flat, subdir_named, subdir_flat]);

        let mut deduped = all.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            all.len(),
            "no mixin may be collected twice: {all:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn flat_scan_tolerates_a_missing_directory() {
        let root = unique_root("flat-missing");
        let absent = root.join("nope");
        assert!(collect_mixins(&absent, &[ScanMode::Flat]).is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    /// `generate_rag_sbx_mixin` writes an agent-scoped RAG sidecar next to the
    /// rag yaml, at `<agents>/<agent>/<rag>.sbx-mixin.yaml`. Before `SubdirFlat`
    /// existed, nothing scanned that shape and attaching a Qdrant RAG from
    /// inside an agent produced no network allow rule and no credential.
    #[test]
    fn agent_scoped_rag_sidecar_is_discovered() {
        let root = unique_root("agent-scoped-rag");
        let agent = root.join("researcher");
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("company-docs.sbx-mixin.yaml"), "kind: mixin\n").unwrap();
        fs::write(agent.join("company-docs.yaml"), "driver: qdrant\n").unwrap();

        let found = collect_mixins(&root, &[ScanMode::SubdirNamed, ScanMode::SubdirFlat]);
        assert_eq!(found, vec![agent.join("company-docs.sbx-mixin.yaml")]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agent_level_mixin_and_rag_sidecars_are_both_discovered() {
        let root = unique_root("agent-both-shapes");
        let agent = root.join("researcher");
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("sbx-mixin.yaml"), "kind: mixin\n").unwrap();
        fs::write(agent.join("zebra.sbx-mixin.yaml"), "kind: mixin\n").unwrap();
        fs::write(agent.join("alpha.sbx-mixin.yaml"), "kind: mixin\n").unwrap();

        let found = collect_mixins(&root, &[ScanMode::SubdirNamed, ScanMode::SubdirFlat]);
        assert_eq!(
            file_names(&found),
            vec![
                "sbx-mixin.yaml",
                "alpha.sbx-mixin.yaml",
                "zebra.sbx-mixin.yaml"
            ]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn subdir_flat_scan_ignores_a_directory_named_like_a_mixin() {
        let root = unique_root("subdir-flat-decoy");
        let agent = root.join("researcher");
        fs::create_dir_all(agent.join("decoy.sbx-mixin.yaml")).unwrap();

        assert!(collect_mixins(&root, &[ScanMode::SubdirFlat]).is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
