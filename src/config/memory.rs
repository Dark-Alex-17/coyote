use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow};
use chrono::Local;
use fancy_regex::Regex;
use log::warn;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::config::{
    GIT_DIR_NAME, GITIGNORE_FILE_NAME, MEMORY_DIR_NAME, MEMORY_INDEX_FILE_NAME,
    WORKSPACE_COYOTE_DIR_NAME, paths,
};

pub const DEFAULT_MEMORY_CAP_WITH_TOOLS: usize = 6_000;
pub const DEFAULT_MEMORY_CAP_WITHOUT_TOOLS: usize = 12_000;
pub const DEFAULT_MEMORY_RUN_STATE_TTL_DAYS: u32 = 14;

/// A MEMORY.md line that catalogues a drill file. Only lines of this exact shape
/// count as indexing a file; a wikilink cited mid-sentence in a FACTS line does not.
pub static CATALOGUE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^- \[\[(?P<slug>[A-Za-z0-9_.-]+)\]\]:\s*(?P<desc>.*)$").unwrap());

pub fn catalogue_slug(line: &str) -> Option<&str> {
    CATALOGUE_LINE_RE
        .captures(line)
        .ok()
        .flatten()
        .and_then(|caps| caps.name("slug"))
        .map(|m| m.as_str())
}

#[derive(Debug, Clone)]
pub struct WorkspaceMemory {
    pub workspace_root: PathBuf,
    pub dir: PathBuf,
}

pub fn discover_workspace_memory(start: &Path) -> Option<WorkspaceMemory> {
    for dir in start.ancestors() {
        let structured = dir.join(WORKSPACE_COYOTE_DIR_NAME).join(MEMORY_DIR_NAME);
        if structured.join(MEMORY_INDEX_FILE_NAME).exists() {
            return Some(WorkspaceMemory {
                workspace_root: dir.to_path_buf(),
                dir: structured,
            });
        }
    }
    None
}

pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join(GIT_DIR_NAME).exists() {
            return Some(dir.to_path_buf());
        }
    }

    None
}

pub fn bootstrap_workspace_memory(git_root: &Path) -> Result<PathBuf> {
    let mem_dir = paths::workspace_memory_dir_for(git_root);
    fs::create_dir_all(&mem_dir)
        .with_context(|| format!("create memory dir {}", mem_dir.display()))?;

    let index_path = mem_dir.join(MEMORY_INDEX_FILE_NAME);
    if !index_path.exists() {
        fs::write(&index_path, "# Workspace Memory Index\n\n")
            .with_context(|| format!("write {}", index_path.display()))?;
    }

    let gitignore_appended = append_gitignore_entry(git_root)?;
    let suffix = if gitignore_appended {
        " (appended .coyote/memory/ to .gitignore)"
    } else {
        ""
    };
    warn!(
        "auto-bootstrapped workspace memory at {}{}",
        mem_dir.display(),
        suffix
    );

    Ok(mem_dir)
}

pub fn append_gitignore_entry(git_root: &Path) -> Result<bool> {
    let gitignore = git_root.join(GITIGNORE_FILE_NAME);
    let entry = format!("{WORKSPACE_COYOTE_DIR_NAME}/{MEMORY_DIR_NAME}/");
    let entry_no_slash = format!("{WORKSPACE_COYOTE_DIR_NAME}/{MEMORY_DIR_NAME}");

    let existing = fs::read_to_string(&gitignore).unwrap_or_default();
    let already_present = existing.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == entry || trimmed == entry_no_slash
    });

    if already_present {
        return Ok(false);
    }

    let new_content = if existing.is_empty() {
        format!("{entry}\n")
    } else if existing.ends_with('\n') {
        format!("{existing}{entry}\n")
    } else {
        format!("{existing}\n{entry}\n")
    };

    fs::write(&gitignore, new_content).with_context(|| format!("write {}", gitignore.display()))?;

    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryKind {
    User,
    Feedback,
    Project,
    #[default]
    Reference,
    RunState,
}

impl MemoryKind {
    pub const NAMES: [&'static str; 5] = ["user", "feedback", "project", "reference", "run-state"];

    const ALL: [MemoryKind; 5] = [
        Self::User,
        Self::Feedback,
        Self::Project,
        Self::Reference,
        Self::RunState,
    ];

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

impl FromStr for MemoryKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::NAMES
            .iter()
            .position(|name| *name == value)
            .map(|i| Self::ALL[i])
            .ok_or_else(|| {
                anyhow!(
                    "Invalid value '{value}', expected one of: {}",
                    Self::NAMES.join(", ")
                )
            })
    }
}

/// The `type:` front-matter field as both the kind Coyote acts on and the scalar
/// actually on disk. Unknown or missing values act as `Reference` but are written
/// back unchanged, so loading and re-saving a file never rewrites its `type:` line.
#[derive(Debug, Clone, PartialEq)]
pub struct KindField {
    parsed: MemoryKind,
    raw: serde_yaml::Value,
}

impl KindField {
    pub fn kind(&self) -> MemoryKind {
        self.parsed
    }

    pub fn raw_kind(&self) -> &serde_yaml::Value {
        &self.raw
    }

    pub fn is_known(&self) -> bool {
        self.raw
            .as_str()
            .is_some_and(|s| MemoryKind::from_str(s).is_ok())
    }
}

impl Default for KindField {
    fn default() -> Self {
        Self {
            parsed: MemoryKind::default(),
            raw: serde_yaml::Value::Null,
        }
    }
}

impl From<MemoryKind> for KindField {
    fn from(kind: MemoryKind) -> Self {
        Self {
            parsed: kind,
            raw: serde_yaml::Value::String(kind.name().to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for KindField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_yaml::Value::deserialize(deserializer)?;
        if raw.is_mapping() || raw.is_sequence() {
            return Err(de::Error::custom("`type` must be a scalar"));
        }
        let parsed = raw
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        Ok(Self { parsed, raw })
    }
}

impl Serialize for KindField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(serializer)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MemoryFrontmatter {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: KindField,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
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

    pub fn is_stale(&self, today: &str) -> bool {
        self.frontmatter.superseded_by.is_some()
            || self
                .frontmatter
                .expires
                .as_deref()
                .is_some_and(|expires| expires < today)
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

pub fn today_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub include_stale: bool,
}

impl Retention {
    pub fn live() -> Self {
        Self {
            include_stale: false,
        }
    }

    pub fn all() -> Self {
        Self {
            include_stale: true,
        }
    }
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
            Some(ws) => {
                let index = ws.dir.join(MEMORY_INDEX_FILE_NAME);
                if index.exists() {
                    Ok(Some(fs::read_to_string(index)?))
                } else {
                    Ok(None)
                }
            }
        }
    }

    pub fn list_files(&self, retention: Retention) -> Result<Vec<MemoryFile>> {
        let mut out = Vec::new();

        if self.global_dir.exists() {
            collect_md_files(&self.global_dir, &mut out)?;
        }

        if let Some(ws) = &self.workspace {
            collect_md_files(&ws.dir, &mut out)?;
        }

        if !retention.include_stale {
            let today = today_string();
            out.retain(|f| !f.is_stale(&today));
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
            consumed, cap
        );
    }

    if !with_tools {
        let mut budget = cap.saturating_sub(consumed);
        let mut files = store.list_files(Retention::live())?;
        files.retain(|f| f.frontmatter.kind.kind() != MemoryKind::RunState);
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
            .join(WORKSPACE_COYOTE_DIR_NAME)
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
    fn workspace_discovery_ignores_root_instructions_file() {
        let root = temp_root("no_lite");
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("COYOTE.md"), "instructions, not memory").unwrap();

        assert!(discover_workspace_memory(&workspace).is_none());

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
            .join(WORKSPACE_COYOTE_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(
            structured.join(MEMORY_INDEX_FILE_NAME),
            "workspace-index-content",
        )
        .unwrap();
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
            .join(WORKSPACE_COYOTE_DIR_NAME)
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
            .join(WORKSPACE_COYOTE_DIR_NAME)
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
        assert_eq!(fm.kind.kind(), MemoryKind::User);
        assert_eq!(body, "Body text\n");
    }

    #[test]
    fn parse_frontmatter_handles_missing_block() {
        let raw = "# Just markdown, no frontmatter\nbody";

        let (fm, body) = parse_frontmatter(raw).unwrap();

        assert_eq!(fm.name, "");
        assert_eq!(fm.kind.kind(), MemoryKind::Reference);
        assert!(fm.kind.raw_kind().is_null());
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
                kind: MemoryKind::User.into(),
                ..Default::default()
            },
            body: "Hello world\nmore text".into(),
        };
        file.save().unwrap();
        let loaded = MemoryFile::load(&path).unwrap();
        assert_eq!(loaded.frontmatter.name, "test");
        assert_eq!(loaded.frontmatter.description.as_deref(), Some("a test"));
        assert_eq!(loaded.frontmatter.kind.kind(), MemoryKind::User);
        assert_eq!(loaded.body, "Hello world\nmore text");

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("type: user"), "kind must serialize as 'type:'");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_kind_parses_its_names_and_teaches_on_bad_input() {
        for name in MemoryKind::NAMES {
            assert_eq!(name.parse::<MemoryKind>().unwrap().name(), name);
        }
        assert_eq!(
            "run-state".parse::<MemoryKind>().unwrap(),
            MemoryKind::RunState
        );

        let err = "bogus".parse::<MemoryKind>().unwrap_err().to_string();
        assert!(err.starts_with("Invalid value 'bogus'"), "{err}");
        for name in MemoryKind::NAMES {
            assert!(err.contains(name), "{err} should list {name}");
        }
    }

    #[test]
    fn kind_field_reads_null_and_unknown_scalars_as_reference_keeping_raw() {
        let (null_fm, _) = parse_frontmatter("---\nname: n\ntype: null\n---\nx\n").unwrap();
        assert_eq!(null_fm.kind.kind(), MemoryKind::Reference);
        assert!(null_fm.kind.raw_kind().is_null());
        assert!(!null_fm.kind.is_known());

        let (absent_fm, _) = parse_frontmatter("---\nname: n\n---\nx\n").unwrap();
        assert!(absent_fm.kind.raw_kind().is_null());

        let (gotcha_fm, _) = parse_frontmatter("---\nname: n\ntype: gotcha\n---\nx\n").unwrap();
        assert_eq!(gotcha_fm.kind.kind(), MemoryKind::Reference);
        assert_eq!(gotcha_fm.kind.raw_kind().as_str(), Some("gotcha"));
        assert!(!gotcha_fm.kind.is_known());

        let (known_fm, _) = parse_frontmatter("---\nname: n\ntype: run-state\n---\nx\n").unwrap();
        assert_eq!(known_fm.kind.kind(), MemoryKind::RunState);
        assert!(known_fm.kind.is_known());

        let (number_fm, _) = parse_frontmatter("---\nname: n\ntype: 7\n---\nx\n").unwrap();
        assert_eq!(number_fm.kind.kind(), MemoryKind::Reference);
        assert_eq!(number_fm.kind.raw_kind().as_u64(), Some(7));

        assert!(parse_frontmatter("---\nname: n\ntype: [a, b]\n---\nx\n").is_err());
    }

    #[test]
    fn resaving_a_loaded_file_keeps_its_type_line_byte_identical() {
        let root = temp_root("type_roundtrip");
        for scalar in ["null", "gotcha", "user"] {
            let path = root.join(format!("{scalar}.md"));
            fs::write(
                &path,
                format!("---\nname: f\ndescription: d\ntype: {scalar}\n---\n\nbody\n"),
            )
            .unwrap();

            MemoryFile::load(&path).unwrap().save().unwrap();

            let saved = fs::read_to_string(&path).unwrap();
            let type_line = format!("\ntype: {scalar}\n");
            assert!(saved.contains(&type_line), "{scalar}: {saved}");
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn catalogue_slug_accepts_only_catalogue_lines() {
        assert_eq!(
            catalogue_slug("- [[auth-flow]]: how auth works"),
            Some("auth-flow")
        );
        assert_eq!(catalogue_slug("- [[a.b_c]]:"), Some("a.b_c"));
        assert_eq!(
            catalogue_slug("- Prefer [[auth-flow]] over passwords"),
            None
        );
        assert_eq!(catalogue_slug("  - [[auth-flow]]: indented"), None);
        assert_eq!(catalogue_slug("- [[auth-flow]] no colon"), None);
        assert_eq!(catalogue_slug("- auth-flow"), None);
    }

    fn write_store_file(dir: &Path, name: &str, extra_frontmatter: &str) {
        fs::write(
            dir.join(format!("{name}.md")),
            format!("---\nname: {name}\n{extra_frontmatter}---\n{name} body\n"),
        )
        .unwrap();
    }

    #[test]
    fn list_files_live_hides_stale_files_while_all_keeps_them() {
        let root = temp_root("retention");
        let global = root.join("global");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join(MEMORY_INDEX_FILE_NAME), "idx").unwrap();
        write_store_file(&global, "fresh", "");
        write_store_file(&global, "future", "expires: 2999-01-01\n");
        write_store_file(&global, "bygone", "expires: 2000-01-01\n");
        write_store_file(&global, "replaced", "superseded_by: fresh\n");

        let store = MemoryStore {
            global_dir: global,
            workspace: None,
        };

        let mut live: Vec<String> = store
            .list_files(Retention::live())
            .unwrap()
            .into_iter()
            .map(|f| f.frontmatter.name)
            .collect();
        live.sort();
        assert_eq!(live, vec!["fresh", "future"]);

        let mut all: Vec<String> = store
            .list_files(Retention::all())
            .unwrap()
            .into_iter()
            .map(|f| f.frontmatter.name)
            .collect();
        all.sort();
        assert_eq!(all, vec!["bygone", "fresh", "future", "replaced"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_memory_section_without_tools_skips_run_state_and_stale_files() {
        let root = temp_root("drill_skips");
        let workspace = root.join("ws");
        let structured = workspace
            .join(WORKSPACE_COYOTE_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join(MEMORY_INDEX_FILE_NAME), "idx").unwrap();
        write_store_file(&structured, "durable", "type: user\n");
        write_store_file(&structured, "scratch", "type: run-state\n");
        write_store_file(&structured, "bygone", "expires: 2000-01-01\n");

        let store = MemoryStore {
            global_dir: root.join("global"),
            workspace: discover_workspace_memory(&workspace),
        };

        let section = build_memory_section(&store, false, 6_000)
            .unwrap()
            .expect("memory section should exist");
        assert!(section.contains("durable body"));
        assert!(!section.contains("scratch body"));
        assert!(!section.contains("bygone body"));
        assert!(!section.contains("memory file(s) omitted"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discover_walks_up_from_nested_dir() {
        let root = temp_root("walk_up");
        let workspace = root.join("ws");
        let mem_dir = workspace
            .join(WORKSPACE_COYOTE_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join(MEMORY_INDEX_FILE_NAME), "idx").unwrap();
        let nested = workspace.join("src").join("deep").join("path");
        fs::create_dir_all(&nested).unwrap();

        let found = discover_workspace_memory(&nested).expect("workspace memory should be found");
        assert_eq!(found.dir, mem_dir);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_returns_dir_containing_git_dir() {
        let root = temp_root("git_root");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();

        assert_eq!(find_git_root(&repo), Some(repo.clone()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_walks_up_from_nested_dir() {
        let root = temp_root("git_root_walk");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let nested = repo.join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_git_root(&nested), Some(repo));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_treats_git_file_as_repo_marker() {
        let root = temp_root("git_root_worktree");
        let worktree = root.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(GIT_DIR_NAME),
            "gitdir: /elsewhere/.git/worktrees/wt\n",
        )
        .unwrap();

        assert_eq!(find_git_root(&worktree), Some(worktree));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_returns_none_when_no_git() {
        let root = temp_root("git_root_missing");
        let bare = root.join("bare");
        fs::create_dir_all(&bare).unwrap();

        assert_eq!(find_git_root(&bare), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_creates_structured_layout_and_index() {
        let root = temp_root("bootstrap_layout");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();

        let mem_dir = bootstrap_workspace_memory(&repo).unwrap();

        assert_eq!(mem_dir, paths::workspace_memory_dir_for(&repo));
        assert!(mem_dir.is_dir());
        let index = mem_dir.join(MEMORY_INDEX_FILE_NAME);
        assert!(index.exists());
        let body = fs::read_to_string(&index).unwrap();
        assert!(body.starts_with("# Workspace Memory Index"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_creates_gitignore_when_absent() {
        let root = temp_root("bootstrap_gi_new");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();

        bootstrap_workspace_memory(&repo).unwrap();

        let gi = repo.join(GITIGNORE_FILE_NAME);
        assert!(gi.exists());
        let body = fs::read_to_string(&gi).unwrap();
        assert!(body.contains(".coyote/memory/"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_appends_to_existing_gitignore_without_trailing_newline() {
        let root = temp_root("bootstrap_gi_append");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        fs::write(repo.join(GITIGNORE_FILE_NAME), "target/").unwrap();

        bootstrap_workspace_memory(&repo).unwrap();

        let body = fs::read_to_string(repo.join(GITIGNORE_FILE_NAME)).unwrap();
        assert!(body.contains("target/"));
        assert!(body.contains(".coyote/memory/"));
        assert!(body.ends_with('\n'));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_is_idempotent_on_gitignore_entry() {
        let root = temp_root("bootstrap_gi_idempotent");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let original = "target/\n.coyote/memory/\n";
        fs::write(repo.join(GITIGNORE_FILE_NAME), original).unwrap();

        bootstrap_workspace_memory(&repo).unwrap();

        let body = fs::read_to_string(repo.join(GITIGNORE_FILE_NAME)).unwrap();
        assert_eq!(body, original, "gitignore must be untouched");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_treats_entry_without_trailing_slash_as_present() {
        let root = temp_root("bootstrap_gi_no_slash");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let original = ".coyote/memory\n";
        fs::write(repo.join(GITIGNORE_FILE_NAME), original).unwrap();

        bootstrap_workspace_memory(&repo).unwrap();

        let body = fs::read_to_string(repo.join(GITIGNORE_FILE_NAME)).unwrap();
        assert_eq!(body, original);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn bootstrap_does_not_clobber_existing_index() {
        let root = temp_root("bootstrap_existing_index");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let mem_dir = paths::workspace_memory_dir_for(&repo);
        fs::create_dir_all(&mem_dir).unwrap();
        let preserved = "# Custom Index\n\n- [[foo]]: keep me\n";
        fs::write(mem_dir.join(MEMORY_INDEX_FILE_NAME), preserved).unwrap();

        bootstrap_workspace_memory(&repo).unwrap();

        let body = fs::read_to_string(mem_dir.join(MEMORY_INDEX_FILE_NAME)).unwrap();
        assert_eq!(body, preserved);

        let _ = fs::remove_dir_all(&root);
    }
}
