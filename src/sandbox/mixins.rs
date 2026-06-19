use std::env;
use std::fs;
use std::fs::{read_dir, read_to_string};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::Value;
use sha2::{Digest, Sha256};

use crate::config::paths;

const SBX_MIXIN_FILE_NAME: &str = "sbx-mixin.yaml";
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
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
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
    fs::write(&spec_path, &bytes)
        .with_context(|| format!("Failed to write {}", spec_path.display()))?;

    debug!(
        "Wrapped mixin {} as kit at {}",
        mixin_path.display(),
        kit_dir.display()
    );

    Ok(kit_dir)
}

pub fn discover() -> Result<Vec<DiscoveredMixin>> {
    let mut out = Vec::new();

    push_if_exists(&mut out, paths::sbx_mixin_file())?;
    push_if_exists(&mut out, paths::global_tools_sbx_mixin_file())?;

    for path in collect_subdir_mixins(&paths::functions_dir()) {
        out.push(read_mixin(path)?);
    }
    for path in collect_subdir_mixins(&paths::agents_data_dir()) {
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
        .get("commands")
        .and_then(|c| c.get("install"))
        .and_then(|i| i.as_sequence())
        .map(|s| s.len())
        .unwrap_or(0);

    let domains = value
        .get("network")
        .and_then(|n| n.get("allowedDomains"))
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

fn collect_subdir_mixins(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let Ok(rd) = read_dir(dir) else { return result };

    let mut entries: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let candidate = entry.path().join(SBX_MIXIN_FILE_NAME);
        if candidate.exists() {
            result.push(candidate);
        }
    }

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

    #[test]
    fn summarize_counts_installs_and_domains() {
        let root = unique_root("sbx-mixin-counts");
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
    fn collect_subdir_mixins_sorts_and_skips_missing() {
        let root = unique_root("sbx-mixin-subdirs");
        for name in ["zebra", "apple", "no-mixin", "mango"] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            if name != "no-mixin" {
                fs::write(dir.join("sbx-mixin.yaml"), "kind: mixin\n").unwrap();
            }
        }

        let found = collect_subdir_mixins(&root);
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
    fn collect_subdir_mixins_returns_empty_for_missing_dir() {
        let absent = env::temp_dir().join("coyote-definitely-not-here-xyz");
        let found = collect_subdir_mixins(&absent);
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
}
