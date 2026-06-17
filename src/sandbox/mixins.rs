use std::env;
use std::fs::{read_dir, read_to_string};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_yaml::Value;

use crate::config::paths;

const SBX_MIXIN_FILE_NAME: &str = "sbx-mixin.yaml";

#[derive(Debug, Clone)]
pub struct DiscoveredMixin {
    pub path: PathBuf,
    pub label: String,
    pub install_count: usize,
    pub domain_count: usize,
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
}
