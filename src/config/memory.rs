use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use bm25::{Language, SearchEngine, SearchEngineBuilder};
use chrono::Local;
use fancy_regex::Regex;
use log::warn;
use parking_lot::Mutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::config::{
    GIT_DIR_NAME, GITIGNORE_FILE_NAME, MEMORY_DIR_NAME, MEMORY_INDEX_FILE_NAME,
    WORKSPACE_COYOTE_DIR_NAME, paths,
};

pub const DEFAULT_MEMORY_FACTS_CAP: usize = 4_000;
pub const DEFAULT_MEMORY_DRILL_CAP: usize = 12_000;
pub const DEFAULT_MEMORY_RUN_STATE_TTL_DAYS: u32 = 14;
/// Chars a `description` may hold; it is one MEMORY.md index line.
pub const MEMORY_DESCRIPTION_CAP: usize = 160;
/// Chars a memory body may hold at write; `memory__lint` flags bodies over 2000 chars.
pub const MEMORY_FILE_HARD_CAP: usize = 4_000;

/// A MEMORY.md line that catalogues a drill file. Only lines of this exact shape
/// count as indexing a file; a wikilink cited mid-sentence in a FACTS line does not.
/// The slug class is deliberately wider than the write-side slug grammar so legacy
/// index lines with dots or uppercase are still recognised.
pub static CATALOGUE_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^- \[\[(?P<slug>[A-Za-z0-9_.-]+)\]\]:\s*(?P<desc>.*)$").unwrap());

pub fn catalogue_slug(line: &str) -> Option<&str> {
    catalogue_entry(line).map(|(slug, _)| slug)
}

/// The `(slug, description)` of a catalogue line; `None` for any other line.
pub fn catalogue_entry(line: &str) -> Option<(&str, &str)> {
    let caps = CATALOGUE_LINE_RE.captures(line).ok().flatten()?;
    Some((
        caps.name("slug")?.as_str(),
        caps.name("desc")?.as_str().trim(),
    ))
}

/// Test-only override of where workspace memory discovery starts, and the
/// directory it never walks above. Without it, a test running from the repo
/// root discovers the developer's real `.coyote/memory/MEMORY.md` and
/// injects it into prompts.
#[cfg(test)]
pub(crate) const WORKSPACE_MEMORY_ROOT_TEST_ENV: &str = "COYOTE_TEST_WORKSPACE_MEMORY_ROOT";

#[derive(Debug, Clone)]
pub struct WorkspaceMemory {
    pub workspace_root: PathBuf,
    pub dir: PathBuf,
}

/// The workspace memory a process running in `cwd` sees. Every lookup
/// outside this module goes through here so the test override applies.
pub fn find_workspace_memory(cwd: &Path) -> Option<WorkspaceMemory> {
    let (start, ceiling) = workspace_search_bounds(cwd);
    walk_workspace_memory(&start, ceiling.as_deref())
}

/// Whether a global index, or a workspace index visible from `cwd`, exists;
/// an agent is offered the memory tools only when one does.
pub fn memory_index_exists(cwd: Option<&Path>) -> bool {
    paths::global_memory_index_file().exists() || cwd.and_then(find_workspace_memory).is_some()
}

/// Where a workspace lookup from `cwd` starts and the directory it must not
/// walk above: `cwd` and no ceiling, or the test override for both.
#[cfg(not(test))]
pub fn workspace_search_bounds(cwd: &Path) -> (PathBuf, Option<PathBuf>) {
    (cwd.to_path_buf(), None)
}

#[cfg(test)]
pub fn workspace_search_bounds(cwd: &Path) -> (PathBuf, Option<PathBuf>) {
    match std::env::var_os(WORKSPACE_MEMORY_ROOT_TEST_ENV) {
        Some(root) => {
            let root = PathBuf::from(root);
            assert!(
                root.is_absolute(),
                "{WORKSPACE_MEMORY_ROOT_TEST_ENV} must be absolute, got {}",
                root.display()
            );
            (root.clone(), Some(root))
        }
        None => (cwd.to_path_buf(), None),
    }
}

fn walk_workspace_memory(start: &Path, ceiling: Option<&Path>) -> Option<WorkspaceMemory> {
    for dir in ancestors_within(start, ceiling) {
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

fn find_git_root(start: &Path) -> Option<PathBuf> {
    find_git_root_within(start, None)
}

/// The git root a process running in `cwd` sees, bounded like
/// `find_workspace_memory` so the test override applies.
pub fn find_workspace_git_root(cwd: &Path) -> Option<PathBuf> {
    let (start, ceiling) = workspace_search_bounds(cwd);
    find_git_root_within(&start, ceiling.as_deref())
}

/// Test-only: the bounded walk with an explicit ceiling, for tests that must
/// not read above their own temp root. Ignores the override.
#[cfg(test)]
pub(crate) fn find_git_root_under(start: &Path, ceiling: &Path) -> Option<PathBuf> {
    find_git_root_within(start, Some(ceiling))
}

fn find_git_root_within(start: &Path, ceiling: Option<&Path>) -> Option<PathBuf> {
    for dir in ancestors_within(start, ceiling) {
        if dir.join(GIT_DIR_NAME).exists() {
            return Some(dir.to_path_buf());
        }
    }

    None
}

fn ancestors_within<'a>(
    start: &'a Path,
    ceiling: Option<&'a Path>,
) -> impl Iterator<Item = &'a Path> {
    start
        .ancestors()
        .take_while(move |dir| ceiling.is_none_or(|ceiling| dir.starts_with(ceiling)))
}

pub fn bootstrap_workspace_memory(git_root: &Path) -> Result<PathBuf> {
    create_workspace_memory(git_root, true)
}

pub fn create_workspace_memory(root: &Path, gitignore: bool) -> Result<PathBuf> {
    let mem_dir = paths::workspace_memory_dir_for(root);
    fs::create_dir_all(&mem_dir)
        .with_context(|| format!("create memory dir {}", mem_dir.display()))?;

    let index_path = mem_dir.join(MEMORY_INDEX_FILE_NAME);
    if !index_path.exists() {
        fs::write(&index_path, "# Workspace Memory Index\n\n")
            .with_context(|| format!("write {}", index_path.display()))?;
    }

    let gitignore_appended = gitignore && append_gitignore_entry(root)?;
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

/// Where `coyote --init-memory workspace` roots the store: the git root, else `cwd`.
pub fn init_workspace_root(cwd: &Path) -> PathBuf {
    find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

/// Writes the `--init-memory workspace` index, appending the .gitignore entry only inside a git repo.
pub fn write_workspace_memory_index(index_path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = index_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create memory dir {}", parent.display()))?;
    }
    fs::write(index_path, content).with_context(|| format!("write {}", index_path.display()))?;
    if let Some(git_root) = find_git_root(index_path) {
        append_gitignore_entry(&git_root)?;
    }
    Ok(())
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

/// The fields `MemoryFrontmatter` omits when `None`; a rewrite drops them from
/// the source mapping in that case.
const OPTIONAL_FRONTMATTER_KEYS: [&str; 4] = ["created", "updated", "superseded_by", "expires"];

#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub frontmatter: MemoryFrontmatter,
    pub body: String,
    /// The front matter as read from disk, empty for a file not loaded from
    /// one. A render starts from it, so key order and keys the struct does not
    /// model survive a rewrite.
    pub source: serde_yaml::Mapping,
}

impl MemoryFile {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("read memory file {}", path.display()))?;
        let (frontmatter, source, body) = parse_frontmatter(&raw)
            .with_context(|| format!("parse frontmatter in {}", path.display()))?;

        Ok(Self {
            path: path.to_path_buf(),
            frontmatter,
            body,
            source,
        })
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.path, self.render()?)?;

        Ok(())
    }

    /// The file contents `save` writes.
    pub fn render(&self) -> Result<String> {
        let frontmatter_yaml = serde_yaml::to_string(&self.merged_frontmatter()?)?;
        Ok(format!("---\n{}---\n\n{}", frontmatter_yaml, self.body))
    }

    /// `source` with each typed field overwritten in place, appended when the
    /// source lacks it, or removed when it is a `None` optional.
    fn merged_frontmatter(&self) -> Result<serde_yaml::Mapping> {
        let serde_yaml::Value::Mapping(typed) = serde_yaml::to_value(&self.frontmatter)? else {
            bail!("front matter did not serialize to a mapping");
        };
        let mut merged = self.source.clone();
        for key in OPTIONAL_FRONTMATTER_KEYS {
            if !typed.contains_key(key) {
                merged.shift_remove(key);
            }
        }
        for (key, value) in typed {
            merged.insert(key, value);
        }
        Ok(merged)
    }

    pub fn char_len(&self) -> usize {
        self.body.chars().count()
    }

    pub fn is_stale(&self, today: &str) -> bool {
        is_stale_by(
            self.frontmatter.superseded_by.as_deref(),
            self.frontmatter.expires.as_deref(),
            today,
        )
    }
}

fn is_stale_by(superseded_by: Option<&str>, expires: Option<&str>, today: &str) -> bool {
    superseded_by.is_some() || expires.is_some_and(|expires| expires < today)
}

fn parse_frontmatter(raw: &str) -> Result<(MemoryFrontmatter, serde_yaml::Mapping, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        return Ok((Default::default(), Default::default(), raw.to_string()));
    }

    let after = &trimmed[3..];
    let Some(end) = after.find("\n---") else {
        return Ok((Default::default(), Default::default(), raw.to_string()));
    };
    let yaml = after[..end].trim();
    let body = after[end + 4..].trim_start_matches('\n').to_string();
    let source = match serde_yaml::from_str(yaml).context("parse YAML frontmatter")? {
        serde_yaml::Value::Mapping(mapping) => mapping,
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        _ => bail!("YAML frontmatter is not a mapping"),
    };
    // Deserializing from text rather than from `source` lets a plain scalar such as
    // `description: 42` or `name: 2024` load into a String field.
    let frontmatter: MemoryFrontmatter = if source.is_empty() {
        MemoryFrontmatter::default()
    } else {
        serde_yaml::from_str(yaml).context("parse YAML frontmatter")?
    };

    Ok((frontmatter, source, body))
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
            workspace: find_workspace_memory(cwd),
        }
    }

    /// A store over explicit test dirs: the workspace is found by walking up
    /// from `start`, ignoring the override and never above `ceiling`.
    #[cfg(test)]
    pub(crate) fn for_test(global_dir: PathBuf, start: &Path, ceiling: &Path) -> Self {
        Self {
            global_dir,
            workspace: walk_workspace_memory(start, Some(ceiling)),
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

    fn scope_dirs(&self) -> Vec<(Scope, &Path)> {
        let mut dirs = vec![(Scope::Global, self.global_dir.as_path())];
        if let Some(ws) = &self.workspace {
            dirs.push((Scope::Workspace, ws.dir.as_path()));
        }
        dirs
    }

    /// Every drill file of both scopes, stale ones included. The result is
    /// shared until a drill file in either scope dir is added, removed or
    /// rewritten.
    pub fn catalogue(&self) -> Result<Arc<Catalogue>> {
        let dirs = self.scope_dirs();
        let key: Vec<PathBuf> = dirs.iter().map(|(_, dir)| dir.to_path_buf()).collect();
        let stamps = drill_file_stamps(&dirs)?;
        if let Some((cached, catalogue)) = CATALOGUE_CACHE.lock().get(&key)
            && *cached == stamps
        {
            return Ok(Arc::clone(catalogue));
        }

        let catalogue = Arc::new(Catalogue::load(&dirs)?);
        CATALOGUE_CACHE
            .lock()
            .insert(key, (stamps, Arc::clone(&catalogue)));
        Ok(catalogue)
    }

    /// Up to `limit` entries the filter admits, best bm25 score over name and
    /// description first, ties broken by the most recent `updated`.
    pub fn search(&self, query: &str, limit: usize, filter: &Filter) -> Result<Vec<Hit>> {
        let catalogue = self.catalogue()?;
        let entries = catalogue.entries();
        let today = today_string();
        let mut scored: Vec<(&CatalogueEntry, f32)> = catalogue
            .engine
            .search(query, entries.len())
            .into_iter()
            .filter(|result| result.score > 0.0)
            .map(|result| (&entries[result.document.id], result.score))
            .filter(|(entry, _)| filter.admits(entry, &today))
            .collect();
        scored.sort_by(|(a, a_score), (b, b_score)| {
            b_score
                .total_cmp(a_score)
                .then_with(|| b.updated.cmp(&a.updated))
        });
        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(entry, score)| Hit {
                entry: entry.clone(),
                score,
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Filter {
    pub kind: Option<MemoryKind>,
    pub scope: Option<Scope>,
    pub include_stale: bool,
}

impl Filter {
    pub fn admits(&self, entry: &CatalogueEntry, today: &str) -> bool {
        self.kind.is_none_or(|kind| entry.kind.kind() == kind)
            && self.scope.is_none_or(|scope| entry.scope == scope)
            && (self.include_stale || !entry.is_stale(today))
    }
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub entry: CatalogueEntry,
    pub score: f32,
}

/// A drill file's front matter as the catalogue holds it; the body is not kept.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogueEntry {
    pub name: String,
    pub description: Option<String>,
    pub kind: KindField,
    pub scope: Scope,
    pub path: PathBuf,
    pub char_len: usize,
    pub updated: Option<String>,
    pub superseded_by: Option<String>,
    pub expires: Option<String>,
}

impl CatalogueEntry {
    fn new(file: MemoryFile, scope: Scope) -> Self {
        let char_len = file.char_len();
        let fm = file.frontmatter;
        Self {
            name: fm.name,
            description: fm.description,
            kind: fm.kind,
            scope,
            path: file.path,
            char_len,
            updated: fm.updated,
            superseded_by: fm.superseded_by,
            expires: fm.expires,
        }
    }

    pub fn is_stale(&self, today: &str) -> bool {
        is_stale_by(
            self.superseded_by.as_deref(),
            self.expires.as_deref(),
            today,
        )
    }
}

/// The global scope's entries, then the workspace's, each in path order.
pub struct Catalogue {
    entries: Vec<CatalogueEntry>,
    engine: SearchEngine<usize>,
}

impl Catalogue {
    fn load(dirs: &[(Scope, &Path)]) -> Result<Self> {
        let mut entries = Vec::new();
        for (scope, dir) in dirs {
            if !dir.exists() {
                continue;
            }
            let mut files = Vec::new();
            collect_md_files(dir, &mut files)?;
            files.sort_by(|a, b| a.path.cmp(&b.path));
            entries.extend(files.into_iter().map(|f| CatalogueEntry::new(f, *scope)));
        }
        let documents = entries.iter().enumerate().map(|(id, entry)| {
            bm25::Document::new(
                id,
                format!(
                    "{} {}",
                    entry.name,
                    entry.description.as_deref().unwrap_or_default()
                ),
            )
        });
        let engine = SearchEngineBuilder::<usize>::with_documents(Language::English, documents)
            .k1(1.5)
            .b(0.75)
            .build();
        Ok(Self { entries, engine })
    }

    pub fn entries(&self) -> &[CatalogueEntry] {
        &self.entries
    }
}

/// Size and mtime of one drill file. A cached catalogue is reused only while
/// every stamp still matches, so an in-place rewrite invalidates it even when
/// the directory's own mtime does not move.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
}

type CatalogueCache = HashMap<Vec<PathBuf>, (Vec<FileStamp>, Arc<Catalogue>)>;

static CATALOGUE_CACHE: LazyLock<Mutex<CatalogueCache>> = LazyLock::new(Default::default);

fn drill_file_stamps(dirs: &[(Scope, &Path)]) -> Result<Vec<FileStamp>> {
    let mut stamps = Vec::new();
    for (_, dir) in dirs {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_drill_file(&path) {
                continue;
            }
            let meta = entry.metadata()?;
            stamps.push(FileStamp {
                path,
                modified: meta.modified().ok(),
                len: meta.len(),
            });
        }
    }
    stamps.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(stamps)
}

/// The rendered `<memory>` block plus the figures behind it: how many chars
/// the facts and catalogue lines held before the cap, and what the cap cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySection {
    pub text: String,
    pub facts_chars: usize,
    pub catalogue_chars: usize,
    pub truncated: Option<Truncated>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Lines and chars a cap omitted, across facts and catalogue together;
/// `global_lines + workspace_lines == lines` always holds.
pub struct Truncated {
    pub lines: usize,
    pub chars: usize,
    pub global_lines: usize,
    pub workspace_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Workspace,
}

impl Scope {
    pub const NAMES: [&'static str; 2] = ["global", "workspace"];

    const ALL: [Scope; 2] = [Self::Global, Self::Workspace];

    pub fn name(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

impl FromStr for Scope {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::NAMES
            .iter()
            .position(|name| *name == value)
            .map(|i| Self::ALL[i])
            .ok_or_else(|| {
                anyhow!(
                    "Invalid scope '{value}', expected one of: {}",
                    Self::NAMES.join(", ")
                )
            })
    }
}

/// Renders both MEMORY.md indexes facts first: every non-catalogue line of
/// the global index, then of the workspace index, under `<facts>`; the
/// catalogue lines of the workspace index, then of the global index, under
/// `<catalogue>`. The whole is cut at a line boundary to `facts_cap` chars,
/// so a catalogue too long for the cap loses its tail before any fact does,
/// and a workspace index too long for the cap loses its own facts before any
/// global one. Without tools, drill file bodies follow inside whatever
/// `drill_cap` leaves after the section so far.
pub fn build_memory_section(
    store: &MemoryStore,
    with_tools: bool,
    facts_cap: usize,
    drill_cap: usize,
) -> Result<Option<MemorySection>> {
    let global_index = store.load_global_index()?;
    let workspace_index = store.load_workspace_index()?;

    if global_index.is_none() && workspace_index.is_none() {
        return Ok(None);
    }

    let mut global_facts: Vec<(Scope, &str)> = Vec::new();
    let mut workspace_facts: Vec<(Scope, &str)> = Vec::new();
    let mut catalogue: Vec<(Scope, &str)> = Vec::new();
    for (scope, index) in [
        (Scope::Workspace, &workspace_index),
        (Scope::Global, &global_index),
    ] {
        let Some(index) = index else {
            continue;
        };
        let scope_facts = match scope {
            Scope::Global => &mut global_facts,
            Scope::Workspace => &mut workspace_facts,
        };
        for line in index.lines() {
            if catalogue_entry(line).is_some() {
                catalogue.push((scope, line));
            } else {
                scope_facts.push((scope, line));
            }
        }
        while scope_facts.last().is_some_and(|(_, l)| l.trim().is_empty()) {
            scope_facts.pop();
        }
    }
    let facts: Vec<(Scope, &str)> = global_facts.into_iter().chain(workspace_facts).collect();
    let facts_chars = joined_chars(&facts);
    let catalogue_chars = joined_chars(&catalogue);
    let (facts_body, catalogue_body, truncated) =
        cut_at_line_boundary(&facts, &catalogue, facts_cap);

    let mut buf = String::from("<memory>\n");
    push_block(&mut buf, "facts", &facts_body);
    push_block(&mut buf, "catalogue", &catalogue_body);
    if let Some(cut) = &truncated {
        buf.push_str(&format!(
            "<!-- memory facts truncated: {} lines / {} chars omitted (global {}, workspace {}); run memory__lint -->\n",
            cut.lines, cut.chars, cut.global_lines, cut.workspace_lines
        ));
    }

    if !with_tools {
        let mut budget = drill_cap.saturating_sub(count_chars(&buf));
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
    Ok(Some(MemorySection {
        text: buf,
        facts_chars,
        catalogue_chars,
        truncated,
    }))
}

fn count_chars(s: &str) -> usize {
    s.chars().count()
}

/// Chars of `lines` once joined by newlines.
fn joined_chars(lines: &[(Scope, &str)]) -> usize {
    lines.iter().map(|(_, l)| count_chars(l)).sum::<usize>() + lines.len().saturating_sub(1)
}

fn join_lines(lines: &[(Scope, &str)]) -> String {
    lines.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n")
}

/// The facts and catalogue bodies once the line sequence `facts ++ catalogue`
/// is cut at the last line boundary within `cap` chars (counted as joined by
/// newlines). A first line longer than the cap is cut at the cap rather than
/// dropped, and counts as one omitted line of its own scope.
fn cut_at_line_boundary(
    facts: &[(Scope, &str)],
    catalogue: &[(Scope, &str)],
    cap: usize,
) -> (String, String, Option<Truncated>) {
    let total_lines = facts.len() + catalogue.len();
    let mut kept = 0usize;
    let mut used = 0usize;
    for (i, (_, line)) in facts.iter().chain(catalogue).enumerate() {
        let next = used + count_chars(line) + usize::from(i > 0);
        if next > cap {
            break;
        }
        used = next;
        kept += 1;
    }
    if kept == total_lines {
        return (join_lines(facts), join_lines(catalogue), None);
    }
    let total_chars = joined_chars(facts)
        + joined_chars(catalogue)
        + usize::from(!facts.is_empty() && !catalogue.is_empty());
    let omitted = |chars: usize| {
        let global_lines = facts
            .iter()
            .chain(catalogue)
            .skip(kept)
            .filter(|(scope, _)| *scope == Scope::Global)
            .count();
        Truncated {
            lines: total_lines - kept,
            chars,
            global_lines,
            workspace_lines: total_lines - kept - global_lines,
        }
    };
    if kept == 0 {
        let first = facts
            .first()
            .or(catalogue.first())
            .map_or("", |(_, line)| line);
        let head: String = first.chars().take(cap).collect();
        let cut = Some(omitted(total_chars - cap));
        return if facts.is_empty() {
            (String::new(), head, cut)
        } else {
            (head, String::new(), cut)
        };
    }
    let kept_facts = kept.min(facts.len());
    (
        join_lines(&facts[..kept_facts]),
        join_lines(&catalogue[..kept - kept_facts]),
        Some(omitted(total_chars - used)),
    )
}

fn push_block(buf: &mut String, tag: &str, body: &str) {
    buf.push_str(&format!("<{tag}>\n"));
    if !body.is_empty() {
        buf.push_str(body);
        buf.push('\n');
    }
    buf.push_str(&format!("</{tag}>\n"));
}

fn collect_md_files(dir: &Path, out: &mut Vec<MemoryFile>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !is_drill_file(&path) {
            continue;
        }

        match MemoryFile::load(&path) {
            Ok(f) => out.push(f),
            Err(e) => warn!("skip malformed memory file {}: {}", path.display(), e),
        }
    }

    Ok(())
}

fn is_drill_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
        && path.file_name().and_then(|n| n.to_str()) != Some(MEMORY_INDEX_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::EnvVarGuard;
    use serial_test::serial;
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

        let store = MemoryStore::for_test(global, &workspace, &root);

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

        assert!(walk_workspace_memory(&workspace, Some(&root)).is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_memory_section_returns_none_when_no_memory_exists() {
        let root = temp_root("none");
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let store = MemoryStore::for_test(root.join("global"), &workspace, &root);

        assert!(
            build_memory_section(&store, true, 4_000, 12_000)
                .unwrap()
                .is_none()
        );

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

        let store = MemoryStore::for_test(root.join("global"), &workspace, &root);

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist")
            .text;
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

        let store = MemoryStore::for_test(root.join("global"), &workspace, &root);

        let section = build_memory_section(&store, false, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist")
            .text;
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

        let store = MemoryStore::for_test(root.join("global"), &workspace, &root);

        let section = build_memory_section(&store, false, 4_000, 100)
            .unwrap()
            .expect("memory section should exist")
            .text;
        assert!(!section.contains(&big_body));
        assert!(section.contains("memory file(s) omitted"));

        let _ = fs::remove_dir_all(&root);
    }

    /// A store with both indexes on disk; `workspace_files` are extra
    /// (name, body) drill files in the workspace memory dir.
    fn two_index_store(
        label: &str,
        global_index: &str,
        workspace_index: &str,
        workspace_files: &[(&str, &str)],
    ) -> (PathBuf, MemoryStore) {
        let root = temp_root(label);
        let workspace = root.join("ws");
        let structured = workspace
            .join(WORKSPACE_COYOTE_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&structured).unwrap();
        fs::write(structured.join(MEMORY_INDEX_FILE_NAME), workspace_index).unwrap();
        for (name, body) in workspace_files {
            fs::write(
                structured.join(format!("{name}.md")),
                format!("---\nname: {name}\n---\n{body}\n"),
            )
            .unwrap();
        }
        let global = root.join("global");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join(MEMORY_INDEX_FILE_NAME), global_index).unwrap();
        let store = MemoryStore::for_test(global, &workspace, &root);
        (root, store)
    }

    /// A global index with one non-catalogue line plus two drill files.
    const GLOBAL_INDEX: &str = "PLACEHOLDER_CHECK\n- [[g-one]]: desc\n- [[g-two]]: desc";
    /// A workspace index with headings, four facts and two drill files.
    const WORKSPACE_INDEX: &str = "# Workspace Memory Index\n\n## Always-on facts\n\
        - **Fact A.** text\n- **Fact B.** text\n- **Fact C.** text\n- **Fact D.** text\n\n\
        ## Drill files\n- [[w-one]]: desc\n- [[w-two]]: desc";
    /// The facts lines of both, global first (154 chars).
    const GLOBAL_THEN_WORKSPACE_FACTS: &str = "PLACEHOLDER_CHECK\n# Workspace Memory Index\n\n\
        ## Always-on facts\n- **Fact A.** text\n- **Fact B.** text\n- **Fact C.** text\n\
        - **Fact D.** text\n\n## Drill files";

    /// `count` catalogue lines of 60 chars each, slugs `{prefix}-NNN`, joined by newlines.
    fn catalogue_lines(prefix: &str, count: usize) -> String {
        (0..count)
            .map(|i| format!("- [[{prefix}-{i:03}]]: {}", "d".repeat(48 - prefix.len())))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tag_body<'a>(section: &'a str, tag: &str) -> &'a str {
        let open = format!("<{tag}>\n");
        let close = format!("</{tag}>\n");
        let start = section.find(&open).expect("open tag") + open.len();
        let end = section[start..].find(&close).expect("close tag") + start;
        section[start..end].strip_suffix('\n').unwrap_or("")
    }

    #[test]
    fn facts_render_before_the_catalogue_across_both_scopes() {
        let (root, store) = two_index_store("facts_first", GLOBAL_INDEX, WORKSPACE_INDEX, &[]);

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist");

        let facts = tag_body(&section.text, "facts");
        assert_eq!(facts, GLOBAL_THEN_WORKSPACE_FACTS);
        assert!(
            facts.lines().all(|l| catalogue_entry(l).is_none()),
            "{facts}"
        );
        assert_eq!(
            tag_body(&section.text, "catalogue"),
            "- [[w-one]]: desc\n- [[w-two]]: desc\n- [[g-one]]: desc\n- [[g-two]]: desc"
        );
        assert!(section.text.find("<facts>").unwrap() < section.text.find("<catalogue>").unwrap());
        assert_eq!(section.facts_chars, 154);
        assert_eq!(section.catalogue_chars, 4 * 17 + 3);
        assert_eq!(section.truncated, None);
        assert!(!section.text.contains("truncated"), "{}", section.text);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn four_workspace_facts_survive_a_4000_char_cap_with_a_huge_catalogue() {
        let global = format!("PLACEHOLDER_CHECK\n{}", catalogue_lines("g", 400));
        let (root, store) = two_index_store("huge_catalogue", &global, WORKSPACE_INDEX, &[]);

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist");

        let facts = tag_body(&section.text, "facts");
        assert_eq!(facts, GLOBAL_THEN_WORKSPACE_FACTS);
        let catalogue = tag_body(&section.text, "catalogue");
        assert!(catalogue.starts_with("- [[w-one]]: desc\n- [[w-two]]: desc\n- [[g-000]]:"));
        assert!(facts.chars().count() + 1 + catalogue.chars().count() <= 4_000);
        // 154 fact chars and the two 17-char workspace lines (36 with their
        // newlines) leave room for 62 of the 400 global lines at 61 each.
        assert_eq!(catalogue.lines().count(), 64);
        let kept_chars = 154 + 36 + 62 * 61;
        let total_chars = 154 + 1 + 35 + 1 + (400 * 60 + 399);
        assert_eq!(section.facts_chars, 154);
        assert_eq!(section.catalogue_chars, 35 + 1 + 400 * 60 + 399);
        assert_eq!(
            section.truncated,
            Some(Truncated {
                lines: 338,
                chars: total_chars - kept_chars,
                global_lines: 338,
                workspace_lines: 0,
            })
        );
        assert!(section.text.ends_with(&format!(
            "</catalogue>\n<!-- memory facts truncated: 338 lines / {} chars omitted \
             (global 338, workspace 0); run memory__lint -->\n</memory>",
            total_chars - kept_chars
        )));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn global_facts_survive_a_workspace_facts_overflow() {
        let global_facts = [
            "- **G one.** text",
            "- **G two.** text",
            "- **G three.** text",
        ];
        let workspace_facts: Vec<String> = (0..70)
            .map(|i| format!("- **Workspace fact {i:02}.** {}", "w".repeat(33)))
            .collect();
        let workspace = workspace_facts.join("\n");
        assert!(workspace.chars().count() > 4_000);
        let (root, store) = two_index_store(
            "workspace_overflow",
            &global_facts.join("\n"),
            &workspace,
            &[],
        );

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist");

        let facts = tag_body(&section.text, "facts");
        assert!(facts.starts_with(&global_facts.join("\n")), "{facts}");
        assert!(
            !facts.contains(workspace_facts.last().unwrap().as_str()),
            "{facts}"
        );
        assert_eq!(tag_body(&section.text, "catalogue"), "");
        let cut = section.truncated.as_ref().expect("truncated");
        assert!(cut.lines >= 1);
        assert_eq!(cut.global_lines, 0);
        assert_eq!(cut.workspace_lines, cut.lines);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cut_falls_on_a_line_boundary() {
        let (root, store) = two_index_store(
            "line_boundary",
            &catalogue_lines("g", 40),
            &catalogue_lines("w", 40),
            &[],
        );

        let section = build_memory_section(&store, true, 200, 12_000)
            .unwrap()
            .expect("memory section should exist");

        assert!(section.text.contains("<facts>\n</facts>\n<catalogue>\n"));
        let catalogue = tag_body(&section.text, "catalogue");
        assert!(
            catalogue
                .lines()
                .all(|l| l.chars().count() == 60 && catalogue_entry(l).is_some()),
            "{catalogue:?}"
        );
        // Three 60-char lines and two newlines; a fourth line would need 243.
        assert_eq!(catalogue.chars().count(), 182);
        assert!(catalogue.starts_with("- [[w-000]]:"));
        assert_eq!(section.facts_chars, 0);
        assert_eq!(section.catalogue_chars, 80 * 60 + 79);
        assert_eq!(
            section.truncated,
            Some(Truncated {
                lines: 77,
                chars: 80 * 60 + 79 - 182,
                global_lines: 40,
                workspace_lines: 37,
            })
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn under_cap_is_untouched() {
        let (root, store) = two_index_store(
            "under_cap",
            "global-content\n- [[g]]: desc",
            "workspace-content\n- [[w]]: desc",
            &[],
        );

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist");

        assert_eq!(
            section,
            MemorySection {
                text: "<memory>\n<facts>\nglobal-content\nworkspace-content\n</facts>\n\
                       <catalogue>\n- [[w]]: desc\n- [[g]]: desc\n</catalogue>\n</memory>"
                    .into(),
                facts_chars: 32,
                catalogue_chars: 27,
                truncated: None,
            }
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_body_exactly_at_the_cap_is_untouched_and_one_under_loses_a_line() {
        let (root, store) = two_index_store(
            "exact_cap",
            "global-content\n- [[g]]: desc",
            "workspace-content\n- [[w]]: desc",
            &[],
        );

        let exact = build_memory_section(&store, true, 60, 12_000)
            .unwrap()
            .expect("memory section should exist");
        assert_eq!(exact.truncated, None);
        assert_eq!(
            tag_body(&exact.text, "catalogue"),
            "- [[w]]: desc\n- [[g]]: desc"
        );

        let short = build_memory_section(&store, true, 59, 12_000)
            .unwrap()
            .expect("memory section should exist");
        assert_eq!(tag_body(&short.text, "catalogue"), "- [[w]]: desc");
        assert_eq!(
            short.truncated,
            Some(Truncated {
                lines: 1,
                chars: 14,
                global_lines: 1,
                workspace_lines: 0,
            })
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_first_line_longer_than_the_cap_is_cut_at_the_cap() {
        let (root, store) = two_index_store("long_line", "- [[g]]: desc", &"x".repeat(50), &[]);

        let section = build_memory_section(&store, true, 20, 12_000)
            .unwrap()
            .expect("memory section should exist");

        assert_eq!(tag_body(&section.text, "facts"), "x".repeat(20));
        assert_eq!(tag_body(&section.text, "catalogue"), "");
        assert_eq!(
            section.truncated,
            Some(Truncated {
                lines: 2,
                chars: 50 + 1 + 13 - 20,
                global_lines: 1,
                workspace_lines: 1,
            })
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_first_catalogue_line_longer_than_the_cap_is_cut_at_the_cap_when_there_are_no_facts() {
        let row = format!("- [[g]]: {}", "d".repeat(60));
        let (root, store) = two_index_store("long_catalogue_line", &row, "", &[]);

        let section = build_memory_section(&store, true, 20, 12_000)
            .unwrap()
            .expect("memory section should exist");

        assert_eq!(tag_body(&section.text, "facts"), "");
        assert_eq!(tag_body(&section.text, "catalogue"), &row[..20]);
        assert_eq!(section.facts_chars, 0);
        assert_eq!(section.catalogue_chars, row.len());
        assert_eq!(
            section.truncated,
            Some(Truncated {
                lines: 1,
                chars: row.len() - 20,
                global_lines: 1,
                workspace_lines: 0,
            })
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_trailing_blank_line_in_an_index_is_not_carried_into_the_facts() {
        let (root, store) = two_index_store(
            "trailing_blank",
            GLOBAL_INDEX,
            "# Workspace Memory Index\n\n",
            &[],
        );

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist");

        let facts = tag_body(&section.text, "facts");
        assert_eq!(facts, "PLACEHOLDER_CHECK\n# Workspace Memory Index");
        assert_eq!(section.facts_chars, count_chars(facts));
        assert_eq!(section.truncated, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_one_megabyte_index_renders_within_cap_plus_notice() {
        let global = catalogue_lines("g", 17_200);
        assert!(global.len() >= 1 << 20);
        let (root, store) = two_index_store("one_megabyte", &global, WORKSPACE_INDEX, &[]);

        let section = build_memory_section(&store, true, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist");

        let notice_start = section
            .text
            .find("<!-- memory facts truncated:")
            .expect("notice");
        let notice_len = section.text[notice_start..].find("-->\n").unwrap() + 4;
        let tag_overhead =
            "<memory>\n<facts>\n\n</facts>\n<catalogue>\n\n</catalogue>\n</memory>".len();
        assert!(
            section.text.chars().count() <= 4_000 + notice_len + tag_overhead,
            "{}",
            section.text.chars().count()
        );
        assert!(tag_body(&section.text, "facts").contains("- **Fact D.** text"));
        assert!(section.truncated.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn drill_bodies_fill_what_the_drill_cap_leaves_after_the_section() {
        // "<memory>\n<facts>\nidx\n</facts>\n<catalogue>\n</catalogue>\n" is 55
        // chars; the body is stored with its trailing newline, 101 chars, and
        // costs 151 of what is left.
        let body = "y".repeat(100);
        let (root, store) = two_index_store("drill_budget", "", "idx", &[("small", &body)]);

        let fits = build_memory_section(&store, false, 4_000, 206)
            .unwrap()
            .expect("memory section should exist");
        assert!(
            fits.text.contains("<file name=\"small\">\n"),
            "{}",
            fits.text
        );
        assert!(!fits.text.contains("memory file(s) omitted"));

        let short = build_memory_section(&store, false, 4_000, 205)
            .unwrap()
            .expect("memory section should exist");
        assert!(!short.text.contains(&body));
        assert!(
            short.text.contains("1 memory file(s) omitted"),
            "{}",
            short.text
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn without_tools_truncates_facts_and_omits_drill_bodies() {
        let big_body = "x".repeat(200);
        let (root, store) = two_index_store(
            "no_tools_cap",
            &catalogue_lines("g", 40),
            &catalogue_lines("w", 40),
            &[("big", &big_body)],
        );

        let section = build_memory_section(&store, false, 200, 300)
            .unwrap()
            .expect("memory section should exist")
            .text;

        assert!(section.contains(
            "</catalogue>\n<!-- memory facts truncated: 77 lines / 4697 chars omitted \
             (global 40, workspace 37); run memory__lint -->\n"
        ));
        assert!(!section.contains(&big_body));
        assert!(
            section.contains("1 memory file(s) omitted; enable function calling for full access")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_frontmatter_extracts_yaml() {
        let raw = "---\nname: foo\ndescription: a thing\ntype: user\n---\nBody text\n";

        let (fm, _, body) = parse_frontmatter(raw).unwrap();

        assert_eq!(fm.name, "foo");
        assert_eq!(fm.description.as_deref(), Some("a thing"));
        assert_eq!(fm.kind.kind(), MemoryKind::User);
        assert_eq!(body, "Body text\n");
    }

    #[test]
    fn parse_frontmatter_handles_missing_block() {
        let raw = "# Just markdown, no frontmatter\nbody";

        let (fm, _, body) = parse_frontmatter(raw).unwrap();

        assert_eq!(fm.name, "");
        assert_eq!(fm.kind.kind(), MemoryKind::Reference);
        assert!(fm.kind.raw_kind().is_null());
        assert_eq!(body, raw);
    }

    #[test]
    fn parse_frontmatter_handles_unterminated_block() {
        let raw = "---\nname: oops\nno closing delimiter\n# rest of doc";

        let (fm, _, body) = parse_frontmatter(raw).unwrap();

        assert_eq!(fm.name, "");
        assert_eq!(body, raw);
    }

    #[test]
    fn parse_frontmatter_reads_an_empty_block_as_the_defaults() {
        let (fm, source, body) = parse_frontmatter("---\n---").unwrap();

        assert_eq!(fm.name, "");
        assert!(fm.description.is_none());
        assert!(fm.kind.raw_kind().is_null());
        assert!(source.is_empty());
        assert_eq!(body, "");
    }

    #[test]
    fn parse_frontmatter_rejects_a_block_that_is_not_a_mapping() {
        let err = parse_frontmatter("---\n- a\n---").unwrap_err();

        assert_eq!(err.to_string(), "YAML frontmatter is not a mapping");
    }

    #[test]
    fn parse_frontmatter_reads_plain_scalars_as_text() {
        let (fm, _, _) =
            parse_frontmatter("---\nname: 2024\ndescription: 42\ncreated: 2026\n---\nbody\n")
                .unwrap();
        assert_eq!(fm.name, "2024");
        assert_eq!(fm.description.as_deref(), Some("42"));

        let (fm, _, _) = parse_frontmatter("---\nname: flag\ndescription: yes\n---\n").unwrap();
        assert_eq!(fm.description.as_deref(), Some("yes"));
    }

    #[test]
    fn a_file_with_plain_scalar_front_matter_survives_a_render_round_trip() {
        let root = temp_root("scalar-roundtrip");
        let path = root.join("2024.md");
        fs::write(
            &path,
            "---\nname: 2024\ndescription: 42\ntype: user\n---\nbody\n",
        )
        .unwrap();

        let loaded = MemoryFile::load(&path).unwrap();
        fs::write(&path, loaded.render().unwrap()).unwrap();
        let reloaded = MemoryFile::load(&path).unwrap();

        assert_eq!(reloaded.frontmatter.name, "2024");
        assert_eq!(reloaded.frontmatter.description.as_deref(), Some("42"));
        assert_eq!(reloaded.body, "body\n");

        let _ = fs::remove_dir_all(&root);
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
            source: Default::default(),
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
        let (null_fm, _, _) = parse_frontmatter("---\nname: n\ntype: null\n---\nx\n").unwrap();
        assert_eq!(null_fm.kind.kind(), MemoryKind::Reference);
        assert!(null_fm.kind.raw_kind().is_null());
        assert!(!null_fm.kind.is_known());

        let (absent_fm, _, _) = parse_frontmatter("---\nname: n\n---\nx\n").unwrap();
        assert!(absent_fm.kind.raw_kind().is_null());

        let (gotcha_fm, _, _) = parse_frontmatter("---\nname: n\ntype: gotcha\n---\nx\n").unwrap();
        assert_eq!(gotcha_fm.kind.kind(), MemoryKind::Reference);
        assert_eq!(gotcha_fm.kind.raw_kind().as_str(), Some("gotcha"));
        assert!(!gotcha_fm.kind.is_known());

        let (known_fm, _, _) =
            parse_frontmatter("---\nname: n\ntype: run-state\n---\nx\n").unwrap();
        assert_eq!(known_fm.kind.kind(), MemoryKind::RunState);
        assert!(known_fm.kind.is_known());

        let (number_fm, _, _) = parse_frontmatter("---\nname: n\ntype: 7\n---\nx\n").unwrap();
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
    fn a_render_keeps_the_source_key_order_and_unknown_keys() {
        let root = temp_root("key_order");
        let path = root.join("ordered.md");
        fs::write(
            &path,
            "---\nupdated: 2020-01-01\ntags: [a, b]\nname: ordered\nowner: alice\n\
             type: user\nmeta:\n  nested:\n    depth: 2\n  list: [1, 2]\n\
             expires: 2999-01-01\ncreated: 2019-01-01\n---\n\nbody\n",
        )
        .unwrap();
        let keys = |mapping: &serde_yaml::Mapping| {
            mapping
                .keys()
                .map(|k| k.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        let mut file = MemoryFile::load(&path).unwrap();
        let source = file.source.clone();
        file.frontmatter.updated = Some("2026-03-01".into());
        file.frontmatter.description = Some("added".into());
        file.frontmatter.expires = None;
        file.save().unwrap();
        let reloaded = MemoryFile::load(&path).unwrap();

        assert_eq!(
            keys(&reloaded.source),
            [
                "updated",
                "tags",
                "name",
                "owner",
                "type",
                "meta",
                "created",
                "description"
            ]
        );
        assert_eq!(reloaded.source["updated"], "2026-03-01");
        assert_eq!(reloaded.source["description"], "added");
        for key in ["tags", "name", "owner", "type", "meta", "created"] {
            assert_eq!(reloaded.source[key], source[key], "{key}");
        }

        reloaded.save().unwrap();
        let again = MemoryFile::load(&path).unwrap();
        assert_eq!(again.source, reloaded.source);
        assert_eq!(again.body, reloaded.body);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn optional_frontmatter_keys_are_the_fields_skipped_when_none() {
        assert_eq!(
            OPTIONAL_FRONTMATTER_KEYS,
            ["created", "updated", "superseded_by", "expires"]
        );

        let keys = |frontmatter: &MemoryFrontmatter| match serde_yaml::to_value(frontmatter) {
            Ok(serde_yaml::Value::Mapping(mapping)) => mapping
                .keys()
                .map(|k| k.as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            other => panic!("front matter did not serialize to a mapping: {other:?}"),
        };
        let date = Some("2024-01-01".to_string());
        // Exhaustive on purpose: a new field fails to compile here until it is
        // set, so a new optional cannot hide from the key difference.
        let all_some = MemoryFrontmatter {
            name: String::new(),
            description: Some(String::new()),
            kind: KindField::default(),
            created: date.clone(),
            updated: date.clone(),
            superseded_by: date.clone(),
            expires: date,
        };
        let required = keys(&MemoryFrontmatter::default());
        let optional: Vec<String> = keys(&all_some)
            .into_iter()
            .filter(|key| !required.contains(key))
            .collect();

        assert_eq!(optional, OPTIONAL_FRONTMATTER_KEYS);
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

        let store = MemoryStore::for_test(root.join("global"), &workspace, &root);

        let section = build_memory_section(&store, false, 4_000, 12_000)
            .unwrap()
            .expect("memory section should exist")
            .text;
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

        let found =
            walk_workspace_memory(&nested, Some(&root)).expect("workspace memory should be found");
        assert_eq!(found.dir, mem_dir);

        let _ = fs::remove_dir_all(&root);
    }

    fn workspace_with_index(root: &Path, name: &str) -> PathBuf {
        let workspace = root.join(name);
        let mem_dir = workspace
            .join(WORKSPACE_COYOTE_DIR_NAME)
            .join(MEMORY_DIR_NAME);
        fs::create_dir_all(&mem_dir).unwrap();
        fs::write(mem_dir.join(MEMORY_INDEX_FILE_NAME), name).unwrap();
        workspace
    }

    #[test]
    #[serial]
    fn store_new_discovers_from_cwd_without_the_test_root_override() {
        let _unset = EnvVarGuard::unset(WORKSPACE_MEMORY_ROOT_TEST_ENV);
        let root = temp_root("new_from_cwd");
        let workspace = workspace_with_index(&root, "ws");

        let store = MemoryStore::new(&workspace);
        let found = store.workspace.expect("workspace memory should be found");
        assert_eq!(found.workspace_root, workspace);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[serial]
    fn store_new_starts_the_walk_at_the_test_root_override() {
        let root = temp_root("new_override");
        let cwd_workspace = workspace_with_index(&root, "cwd");
        let override_workspace = workspace_with_index(&root, "override");
        let _env = EnvVarGuard::set(WORKSPACE_MEMORY_ROOT_TEST_ENV, &override_workspace);

        let store = MemoryStore::new(&cwd_workspace);
        let found = store.workspace.expect("workspace memory should be found");
        assert_eq!(found.workspace_root, override_workspace);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[serial]
    fn store_new_never_walks_above_the_test_root_override() {
        let root = temp_root("override_ceiling");
        let indexed = workspace_with_index(&root, "indexed");
        let pinned = indexed.join("nested");
        fs::create_dir_all(&pinned).unwrap();
        let _env = EnvVarGuard::set(WORKSPACE_MEMORY_ROOT_TEST_ENV, &pinned);

        assert!(
            MemoryStore::new(&pinned).workspace.is_none(),
            "the index above the override is out of reach"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// The shared test guard points the override at its own temp workspace
    /// root (the parent of the directory it hands out as the workspace
    /// config dir), which holds no memory index, so a store built from any
    /// cwd under the guard carries no workspace memory even when that cwd
    /// sits inside a workspace that has one. Dropping the guard puts the cwd
    /// walk back.
    #[test]
    #[serial]
    fn usage_probe_the_test_config_dir_guard_pins_the_workspace_memory_root_to_its_own_workspace() {
        let _unset = EnvVarGuard::unset(WORKSPACE_MEMORY_ROOT_TEST_ENV);
        let root = temp_root("guard_pins_memory_root");
        let indexed = workspace_with_index(&root, "indexed");
        let cwd = indexed.join("src").join("deep");
        fs::create_dir_all(&cwd).unwrap();

        let guard = crate::testing::TestConfigDirGuard::new("memory-root-pin");
        let pinned = PathBuf::from(
            env::var_os(WORKSPACE_MEMORY_ROOT_TEST_ENV).expect("the guard sets the override"),
        );
        let workspace_config_dir = PathBuf::from(
            env::var_os(crate::utils::get_env_name("workspace_config_dir"))
                .expect("the guard sets the workspace config dir"),
        );
        assert_eq!(
            pinned.join(WORKSPACE_COYOTE_DIR_NAME),
            workspace_config_dir,
            "the override is the guard's workspace root"
        );
        assert!(
            !pinned
                .join(WORKSPACE_COYOTE_DIR_NAME)
                .join(MEMORY_DIR_NAME)
                .join(MEMORY_INDEX_FILE_NAME)
                .exists(),
            "the guard's workspace carries no memory index"
        );
        assert!(
            MemoryStore::new(&cwd).workspace.is_none(),
            "under the guard the indexed workspace around the cwd is never discovered"
        );

        drop(guard);
        assert!(env::var_os(WORKSPACE_MEMORY_ROOT_TEST_ENV).is_none());
        let found = MemoryStore::new(&cwd)
            .workspace
            .expect("without the guard the cwd walk finds the index again");
        assert_eq!(found.workspace_root, indexed);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_under_returns_dir_containing_git_dir() {
        let root = temp_root("git_root");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();

        assert_eq!(find_git_root_under(&repo, &root), Some(repo.clone()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_under_walks_up_from_nested_dir() {
        let root = temp_root("git_root_walk");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let nested = repo.join("a").join("b").join("c");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_git_root_under(&nested, &root), Some(repo));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_under_stops_at_the_ceiling() {
        let root = temp_root("git_root_ceiling");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let ceiling = repo.join("a");
        let nested = ceiling.join("b");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(find_git_root_under(&nested, &ceiling), None);
        assert_eq!(find_git_root_under(&nested, &repo), Some(repo));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_under_treats_git_file_as_repo_marker() {
        let root = temp_root("git_root_worktree");
        let worktree = root.join("worktree");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(GIT_DIR_NAME),
            "gitdir: /elsewhere/.git/worktrees/wt\n",
        )
        .unwrap();

        assert_eq!(find_git_root_under(&worktree, &root), Some(worktree));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_git_root_under_returns_none_when_no_git() {
        let root = temp_root("git_root_missing");
        let bare = root.join("bare");
        fs::create_dir_all(&bare).unwrap();

        assert_eq!(find_git_root_under(&bare, &root), None);

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

    #[test]
    fn create_workspace_memory_without_gitignore_leaves_gitignore_alone() {
        let root = temp_root("create_no_gitignore");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();

        let mem_dir = create_workspace_memory(&repo, false).unwrap();

        assert!(mem_dir.join(MEMORY_INDEX_FILE_NAME).exists());
        assert!(!repo.join(GITIGNORE_FILE_NAME).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_workspace_root_outside_git_creates_store_under_cwd() {
        let root = temp_root("init_no_git");
        let cwd = root.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        assert_eq!(find_git_root_under(&cwd, &root), None);

        let index = paths::workspace_memory_index_file_for(&init_workspace_root(&cwd));
        assert_eq!(
            index,
            cwd.join(WORKSPACE_COYOTE_DIR_NAME)
                .join(MEMORY_DIR_NAME)
                .join(MEMORY_INDEX_FILE_NAME)
        );
        assert!(!cwd.join(WORKSPACE_COYOTE_DIR_NAME).exists());

        write_workspace_memory_index(&index, "# Workspace Memory Index\n\n").unwrap();

        assert!(cwd.join(WORKSPACE_COYOTE_DIR_NAME).is_dir());
        assert!(index.exists());
        assert!(!cwd.join(GITIGNORE_FILE_NAME).exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_workspace_root_prefers_git_root() {
        let root = temp_root("init_git");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(GIT_DIR_NAME)).unwrap();
        let nested = repo.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(init_workspace_root(&nested), repo);

        let _ = fs::remove_dir_all(&root);
    }

    fn catalogue_names(store: &MemoryStore) -> Vec<String> {
        store
            .catalogue()
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    #[test]
    fn catalogue_reads_front_matter_of_both_scopes_but_never_the_index() {
        let (root, store) = two_index_store(
            "catalogue_scopes",
            "- [[listed]]: from the index",
            "idx",
            &[("unindexed", "w body")],
        );
        write_store_file(
            &store.global_dir,
            "listed",
            "description: front matter wins\ntype: user\nupdated: 2024-01-02\nexpires: 2000-01-01\n",
        );

        let catalogue = store.catalogue().unwrap();

        let entries = catalogue.entries();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].name, "listed");
        assert_eq!(entries[0].scope, Scope::Global);
        assert_eq!(entries[0].description.as_deref(), Some("front matter wins"));
        assert_eq!(entries[0].kind.kind(), MemoryKind::User);
        assert_eq!(entries[0].updated.as_deref(), Some("2024-01-02"));
        assert!(entries[0].is_stale("2024-01-01"));
        assert_eq!(entries[1].name, "unindexed");
        assert_eq!(entries[1].scope, Scope::Workspace);
        assert_eq!(entries[1].char_len, "w body\n".len());
        assert!(!entries[1].is_stale("2024-01-01"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn catalogue_is_reused_until_a_drill_file_is_added_rewritten_or_removed() {
        let (root, store) = two_index_store("catalogue_cache", "", "idx", &[("one", "body")]);
        let dir = store.workspace.as_ref().unwrap().dir.clone();

        let first = store.catalogue().unwrap();
        assert!(Arc::ptr_eq(&first, &store.catalogue().unwrap()), "hit");

        fs::write(dir.join(MEMORY_INDEX_FILE_NAME), "idx edited").unwrap();
        assert!(
            Arc::ptr_eq(&first, &store.catalogue().unwrap()),
            "an index edit is no catalogue change"
        );

        write_store_file(&dir, "two", "");
        let added = store.catalogue().unwrap();
        assert!(!Arc::ptr_eq(&first, &added), "a new file is a miss");
        assert_eq!(catalogue_names(&store), vec!["one", "two"]);

        write_store_file(&dir, "two", "description: rewritten in place\n");
        let rewritten = store.catalogue().unwrap();
        assert!(!Arc::ptr_eq(&added, &rewritten), "an overwrite is a miss");
        assert_eq!(
            rewritten.entries()[1].description.as_deref(),
            Some("rewritten in place")
        );

        fs::remove_file(dir.join("one.md")).unwrap();
        assert_eq!(catalogue_names(&store), vec!["two"]);

        let _ = fs::remove_dir_all(&root);
    }

    fn hit_names(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.entry.name.as_str()).collect()
    }

    #[test]
    fn search_ranks_a_description_match_above_a_slug_only_match() {
        let (root, store) = two_index_store("search_rank", "", "idx", &[]);
        let dir = store.workspace.as_ref().unwrap().dir.clone();
        write_store_file(&dir, "deploy", "description: unrelated words\n");
        write_store_file(
            &dir,
            "release-notes",
            "description: how we deploy the release train\n",
        );
        write_store_file(&dir, "cooking", "description: pasta recipes\n");

        let hits = store
            .search("deploy release", 10, &Filter::default())
            .unwrap();

        assert_eq!(hit_names(&hits), vec!["release-notes", "deploy"]);
        assert!(hits[0].score > hits[1].score);
        let top = store
            .search("deploy release", 1, &Filter::default())
            .unwrap();
        assert_eq!(hit_names(&top), vec!["release-notes"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_breaks_score_ties_by_most_recent_update() {
        let (root, store) = two_index_store("search_ties", "", "idx", &[]);
        let dir = store.workspace.as_ref().unwrap().dir.clone();
        write_store_file(
            &dir,
            "alpha",
            "description: shared topic\nupdated: 2024-01-01\n",
        );
        write_store_file(
            &dir,
            "bravo",
            "description: shared topic\nupdated: 2024-06-01\n",
        );

        let hits = store.search("topic", 10, &Filter::default()).unwrap();

        assert_eq!(hit_names(&hits), vec!["bravo", "alpha"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_excludes_stale_entries_unless_asked() {
        let (root, store) = two_index_store("search_stale", "", "idx", &[]);
        let dir = store.workspace.as_ref().unwrap().dir.clone();
        write_store_file(&dir, "live", "description: widget facts\n");
        write_store_file(
            &dir,
            "bygone",
            "description: widget facts\nexpires: 2000-01-01\n",
        );
        write_store_file(
            &dir,
            "replaced",
            "description: widget facts\nsuperseded_by: live\n",
        );

        let live = store.search("widget", 10, &Filter::default()).unwrap();
        assert_eq!(hit_names(&live), vec!["live"]);

        let all = Filter {
            include_stale: true,
            ..Filter::default()
        };
        let mut names = hit_names(&store.search("widget", 10, &all).unwrap())
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec!["bygone", "live", "replaced"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_filters_by_type_and_scope() {
        let (root, store) = two_index_store("search_filters", "", "idx", &[]);
        let dir = store.workspace.as_ref().unwrap().dir.clone();
        write_store_file(
            &store.global_dir,
            "g-user",
            "description: gadget\ntype: user\n",
        );
        write_store_file(&dir, "w-user", "description: gadget\ntype: user\n");
        write_store_file(&dir, "w-project", "description: gadget\ntype: project\n");

        let search = |filter: Filter| {
            let mut names: Vec<String> = store
                .search("gadget", 10, &filter)
                .unwrap()
                .into_iter()
                .map(|h| h.entry.name)
                .collect();
            names.sort();
            names
        };

        assert_eq!(
            search(Filter {
                kind: Some(MemoryKind::User),
                ..Filter::default()
            }),
            vec!["g-user", "w-user"]
        );
        assert_eq!(
            search(Filter {
                scope: Some(Scope::Workspace),
                ..Filter::default()
            }),
            vec!["w-project", "w-user"]
        );
        assert_eq!(
            search(Filter {
                kind: Some(MemoryKind::User),
                scope: Some(Scope::Global),
                include_stale: false,
            }),
            vec!["g-user"]
        );
        assert_eq!(search(Filter::default()).len(), 3);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_over_five_thousand_entries_is_fast_once_warm() {
        let (root, store) = two_index_store("search_5k", "", "idx", &[]);
        let dir = store.workspace.as_ref().unwrap().dir.clone();
        for i in 0..5_000 {
            write_store_file(
                &dir,
                &format!("note-{i:04}"),
                &format!("description: topic{} detail{i}\n", i % 50),
            );
        }
        assert_eq!(store.catalogue().unwrap().entries().len(), 5_000);
        store.search("topic3", 10, &Filter::default()).unwrap();

        let started = time::Instant::now();
        let hits = store
            .search("topic7 detail4207", 10, &Filter::default())
            .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(hits[0].entry.name, "note-4207");
        assert_eq!(hits.len(), 10);
        assert!(elapsed < time::Duration::from_millis(200), "{elapsed:?}");

        let _ = fs::remove_dir_all(&root);
    }
}
