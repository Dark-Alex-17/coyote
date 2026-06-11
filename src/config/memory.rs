use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::warn;
use serde::{Deserialize, Serialize};

use crate::config::{paths, MEMORY_DIR_NAME, MEMORY_INDEX_FILE_NAME, WORKSPACE_MEMORY_DIR_NAME, WORKSPACE_MEMORY_FILE_NAME};

pub const DEFAULT_MEMORY_CAP_WITH_TOOLS: usize = 6_000;
pub const DEFAULT_MEMORY_CAP_WITHOUT_TOOLS: usize = 12_000;

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

pub fn build_memory_section(
    store: &MemoryStore,
    with_tools: bool,
    cap: usize,
) -> Result<Option<String>> {
    let global_index = store.load_global_index()?;
    let workspace_index = store.load_workspace_index()?;

    if global_index.is_none() && workspace_index.is_none() {
        return Ok(None);
    }

    let mut buf = String::from("<memory>\n");
    let mut consumed = 0usize;

    if let Some(s) = &global_index {
        buf.push_str("<global_index>\n");
        buf.push_str(s);
        buf.push_str("\n</global_index>\n");
        consumed += s.chars().count();
    }
    
    if let Some(s) = &workspace_index {
        buf.push_str("<workspace_index>\n");
        buf.push_str(s);
        buf.push_str("\n</workspace_index>\n");
        consumed += s.chars().count();
    }

    if consumed > cap {
        warn!(
            "memory indexes ({} chars) exceed cap ({} chars); injecting fully - \
             consider raising memory_cap_* in config or shrinking MEMORY.md",
            consumed,
            cap
        );
    }

    if !with_tools {
        let mut budget = cap.saturating_sub(consumed);
        let mut files = store.list_files()?;
        files.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
        let mut omitted = 0usize;
        for f in files {
            let needed = f.body.chars().count() + 50;
            if needed > budget {
                omitted += 1;
                continue;
            }
            buf.push_str(&format!("<file name=\"{}\">\n", f.frontmatter.name));
            buf.push_str(&f.body);
            buf.push_str("\n</file>\n");
            budget = budget.saturating_sub(needed);
        }
        
        if omitted > 0 {
            buf.push_str(&format!(
                "<!-- {} memory file(s) omitted; enable function calling for full access -->\n",
                omitted
            ));
        }
    }

    buf.push_str("</memory>");
    Ok(Some(buf))
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
            Err(e) => warn!("skip malformed memory file {}: {}", path.display(), e),
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

    #[test]
    fn build_memory_section_returns_none_when_no_memory_exists() {
        let root = temp_root("none");
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let store = MemoryStore {
            global_dir: root.join("global"),
            workspace: discover_workspace_memory(&workspace),
        };

        assert!(build_memory_section(&store, true, 6_000).unwrap().is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_memory_section_injects_only_indexes_with_tools_on() {
        let root = temp_root("indexes_only");
        let workspace = root.join("ws");
        let structured = workspace
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join(MEMORY_INDEX_FILE_NAME), "workspace-index-content").unwrap();
        fs::write(
            structured.join("foo.md"),
            "---\nname: foo\n---\nfoo body that should not appear\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("global"),
            workspace: discover_workspace_memory(&workspace),
        };

        let section = build_memory_section(&store, true, 6_000)
            .unwrap()
            .expect("memory section should exist");
        assert!(section.contains("workspace-index-content"));
        assert!(!section.contains("foo body that should not appear"));
        assert!(section.starts_with("<memory>"));
        assert!(section.ends_with("</memory>"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_memory_section_injects_drill_bodies_alphabetically_without_tools() {
        let root = temp_root("drill_bodies");
        let workspace = root.join("ws");
        let structured = workspace
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join(MEMORY_INDEX_FILE_NAME), "idx").unwrap();
        fs::write(
            structured.join("zebra.md"),
            "---\nname: zebra\n---\nzebra body\n",
        )
        .unwrap();
        fs::write(
            structured.join("alpha.md"),
            "---\nname: alpha\n---\nalpha body\n",
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("global"),
            workspace: discover_workspace_memory(&workspace),
        };

        let section = build_memory_section(&store, false, 6_000)
            .unwrap()
            .expect("memory section should exist");
        let alpha_pos = section.find("alpha body").expect("alpha body missing");
        let zebra_pos = section.find("zebra body").expect("zebra body missing");
        assert!(alpha_pos < zebra_pos, "drill bodies must be alphabetical");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_memory_section_omits_drill_bodies_when_cap_exceeded() {
        let root = temp_root("cap");
        let workspace = root.join("ws");
        let structured = workspace
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join(MEMORY_INDEX_FILE_NAME), "idx").unwrap();
        let big_body = "x".repeat(200);
        fs::write(
            structured.join("big.md"),
            format!("---\nname: big\n---\n{}\n", big_body),
        )
        .unwrap();

        let store = MemoryStore {
            global_dir: root.join("global"),
            workspace: discover_workspace_memory(&workspace),
        };

        let section = build_memory_section(&store, false, 100)
            .unwrap()
            .expect("memory section should exist");
        assert!(!section.contains(&big_body));
        assert!(section.contains("memory file(s) omitted"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_frontmatter_extracts_yaml() {
        let raw = "---\nname: foo\ndescription: a thing\ntype: user\n---\nBody text\n";
        
        let (fm, body) = parse_frontmatter(raw).unwrap();
        
        assert_eq!(fm.name, "foo");
        assert_eq!(fm.description.as_deref(), Some("a thing"));
        assert_eq!(fm.kind.as_deref(), Some("user"));
        assert_eq!(body, "Body text\n");
    }

    #[test]
    fn parse_frontmatter_handles_missing_block() {
        let raw = "# Just markdown, no frontmatter\nbody";
        
        let (fm, body) = parse_frontmatter(raw).unwrap();
        
        assert_eq!(fm.name, "");
        assert!(fm.kind.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn parse_frontmatter_handles_unterminated_block() {
        let raw = "---\nname: oops\nno closing delimiter\n# rest of doc";
        
        let (fm, body) = parse_frontmatter(raw).unwrap();
        
        assert_eq!(fm.name, "");
        assert_eq!(body, raw);
    }

    #[test]
    fn memory_file_save_and_load_roundtrip() {
        let root = temp_root("roundtrip");
        let path = root.join("test.md");
        let file = MemoryFile {
            path: path.clone(),
            frontmatter: MemoryFrontmatter {
                name: "test".into(),
                description: Some("a test".into()),
                kind: Some("user".into()),
            },
            body: "Hello world\nmore text".into(),
        };
        file.save().unwrap();
        let loaded = MemoryFile::load(&path).unwrap();
        assert_eq!(loaded.frontmatter.name, "test");
        assert_eq!(loaded.frontmatter.description.as_deref(), Some("a test"));
        assert_eq!(loaded.frontmatter.kind.as_deref(), Some("user"));
        assert_eq!(loaded.body, "Hello world\nmore text");

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("type: user"), "kind must serialize as 'type:'");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_walks_up_from_nested_dir() {
        let root = temp_root("walk_up");
        let workspace = root.join("ws");
        let mem_dir = workspace
            .join(WORKSPACE_MEMORY_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join(MEMORY_INDEX_FILE_NAME), "idx").unwrap();
        let nested = workspace.join("src").join("deep").join("path");
        fs::create_dir_all(&nested).unwrap();

        let found = discover_workspace_memory(&nested);
        assert!(matches!(found, Some(WorkspaceMemory::Structured { .. })));

        let _ = fs::remove_dir_all(&root);
    }
}
