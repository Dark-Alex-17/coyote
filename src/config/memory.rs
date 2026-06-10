use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{paths, MEMORY_DIR_NAME, MEMORY_INDEX_FILE_NAME, WORKSPACE_MEMORY_DIR_NAME, WORKSPACE_MEMORY_FILE_NAME};

#[derive(Debug, Clone)]
pub enum WorkspaceMemory {
    Structured { workspace_root: PathBuf, dir: PathBuf },
    Lite { workspace_root: PathBuf, file: PathBuf },
}

pub fn discover_workspace_memory(start: &Path) -> Option<WorkspaceMemory> {
    for dir in start.ancestors() {
        let structured = dir
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        if structured.join(MEMORY_INDEX_FILE_NAME).exists() {
            return Some(WorkspaceMemory::Structured {
                workspace_root: dir.to_path_buf(),
                dir: structured,
            });
        }

        let lite = dir.join(WORKSPACE_MEMORY_FILE_NAME);
        if lite.exists() {
            return Some(WorkspaceMemory::Lite {
                workspace_root: dir.to_path_buf(),
                file: lite,
            });
        }
    }
    None
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MemoryFrontmatter {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub frontmatter: MemoryFrontmatter,
    pub body: String,
}

impl MemoryFile {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read memory file {}", path.display()))?;
        let (frontmatter, body) = parse_frontmatter(&raw)
            .with_context(|| format!("parse frontmatter in {}", path.display()))?;

        Ok(Self {
            path: path.to_path_buf(),
            frontmatter,
            body,
        })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let frontmatter_yaml = serde_yaml::to_string(&self.frontmatter)?;
        let content = format!("---\n{}---\n\n{}", frontmatter_yaml, self.body);

        fs::write(&self.path, content)?;

        Ok(())
    }

    pub fn char_len(&self) -> usize {
        self.body.chars().count()
    }
}

fn parse_frontmatter(raw: &str) -> Result<(MemoryFrontmatter, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return Ok((MemoryFrontmatter::default(), raw.to_string()));
    }

    let after = &trimmed[3..];
    let Some(end) = after.find("\n---") else {
        return Ok((MemoryFrontmatter::default(), raw.to_string()));
    };
    let yaml = &after[..end];
    let body = after[end + 4..].trim_start_matches('\n').to_string();
    let frontmatter: MemoryFrontmatter =
        serde_yaml::from_str(yaml.trim()).context("parse YAML frontmatter")?;

    Ok((frontmatter, body))
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    pub global_dir: PathBuf,
    pub workspace: Option<WorkspaceMemory>,
}

impl MemoryStore {
    pub fn new(cwd: &Path) -> Self {
        Self {
            global_dir: paths::global_memory_dir(),
            workspace: discover_workspace_memory(cwd),
        }
    }

    pub fn load_global_index(&self) -> Result<Option<String>> {
        let path = self.global_dir.join(MEMORY_INDEX_FILE_NAME);

        if path.exists() {
            Ok(Some(fs::read_to_string(path)?))
        } else {
            Ok(None)
        }
    }

    pub fn load_workspace_index(&self) -> Result<Option<String>> {
        match &self.workspace {
            None => Ok(None),
            Some(WorkspaceMemory::Lite { file, .. }) => Ok(Some(fs::read_to_string(file)?)),
            Some(WorkspaceMemory::Structured { dir, .. }) => {
                let index = dir.join(MEMORY_INDEX_FILE_NAME);
                if index.exists() {
                    Ok(Some(fs::read_to_string(index)?))
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub fn list_files(&self) -> Result<Vec<MemoryFile>> {
        let mut out = Vec::new();

        if self.global_dir.exists() {
            collect_md_files(&self.global_dir, &mut out)?;
        }

        if let Some(WorkspaceMemory::Structured { dir, .. }) = &self.workspace {
            collect_md_files(dir, &mut out)?;
        }

        Ok(out)
    }
}

fn collect_md_files(dir: &Path, out: &mut Vec<MemoryFile>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        if path.file_name().and_then(|n| n.to_str()) == Some(MEMORY_INDEX_FILE_NAME) {
            continue;
        }

        match MemoryFile::load(&path) {
            Ok(f) => out.push(f),
            Err(e) => log::warn!("skip malformed memory file {}: {}", path.display(), e),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, time};
    use time::SystemTime;

    fn temp_root(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-memory-{label}-{unique}"));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn loads_global_and_workspace_indexes_from_test_dirs() {
        let root = temp_root("phase1");
        let workspace = root.join("workspace");
        let workspace_memory_dir = workspace
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&workspace_memory_dir).unwrap();
        fs::write(
            workspace_memory_dir.join(MEMORY_INDEX_FILE_NAME),
            "workspace-content",
        )
        .unwrap();

        let global = root.join("global");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join(MEMORY_INDEX_FILE_NAME), "global-content").unwrap();

        let store = MemoryStore {
            global_dir: global,
            workspace: discover_workspace_memory(&workspace),
        };

        assert_eq!(
            store.load_global_index().unwrap().as_deref(),
            Some("global-content")
        );
        assert_eq!(
            store.load_workspace_index().unwrap().as_deref(),
            Some("workspace-content")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_discovery_prefers_structured_over_lite() {
        let root = temp_root("prefer");
        let workspace = root.join("ws");
        let structured = workspace
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join(MEMORY_INDEX_FILE_NAME), "s").unwrap();
        fs::write(workspace.join(WORKSPACE_MEMORY_FILE_NAME), "l").unwrap();

        let found = discover_workspace_memory(&workspace);
        assert!(matches!(found, Some(WorkspaceMemory::Structured { .. })));

        let _ = fs::remove_dir_all(&root);
    }
}
