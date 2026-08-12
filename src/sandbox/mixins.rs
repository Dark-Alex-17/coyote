use std::env;
use std::fs;
use std::fs::{read_dir, read_to_string};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::Value;
use sha2::{Digest, Sha256};

use crate::config::paths;

const SBX_MIXIN_FILE_NAME: &str = "sbx-mixin.yaml";
const SBX_MIXIN_FILE_SUFFIX: &str = ".sbx-mixin.yaml";
const KIT_SPEC_FILE_NAME: &str = "spec.yaml";

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
    wrap_mixin_bytes_as_kit(&bytes, &mixin_path.display().to_string())
}

pub fn wrap_mixin_bytes_as_kit(bytes: &[u8], label: &str) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let hash = format!("{:x}", hasher.finalize());

    let kit_dir = paths::sbx_mixin_kits_dir().join(&hash);
    let spec_path = kit_dir.join(KIT_SPEC_FILE_NAME);

    if let Ok(existing) = fs::read(&spec_path)
        && existing == bytes
    {
        return Ok(kit_dir);
    }

    fs::create_dir_all(&kit_dir)
        .with_context(|| format!("Failed to create mixin kit dir {}", kit_dir.display()))?;
    fs::write(&spec_path, bytes)
        .with_context(|| format!("Failed to write {}", spec_path.display()))?;

    debug!("Wrapped mixin {label} as kit at {}", kit_dir.display());

    Ok(kit_dir)
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
