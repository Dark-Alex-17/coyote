use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDateTime};
use serde::Deserialize;

use crate::config::memory::{
    KindField, MEMORY_DESCRIPTION_CAP, MEMORY_FILE_HARD_CAP, MemoryFile, MemoryFrontmatter,
    MemoryKind, catalogue_entry, discover_workspace_memory,
};
use crate::config::{
    MEMORY_INDEX_FILE_NAME, MigrateScope, RoundChains, paths, sanitize_display_text,
};
use crate::function::memory::{marks_older_round, round_chain_name, scan_round_siblings};
use crate::utils::fs::{copy_dir_all, write_private_atomic};

const BACKUP_DIR_NAME: &str = "memory-backups";
const DRILL_FILES_HEADING: &str = "## Drill files";
const FACTS_PREVIEW_LINES: usize = 20;
const SECTION_LIST_CAP: usize = 50;
const STALE_BEFORE_APPLY: &str = "nothing was rewritten";
const STALE_DURING_APPLY: &str = "it was not rewritten";

/// Migrates the memory dirs `scope` names in place when `apply` is set, after
/// backing each one up under the cache dir; otherwise only reports what it
/// would change. `round_chains` decides whether older rounds of a chain are
/// pointed at the newest, as `memory__write` does.
pub fn run(scope: MigrateScope, apply: bool, round_chains: RoundChains) -> Result<Report> {
    let cwd = env::current_dir().context("get cwd")?;
    let targets = targets(scope, paths::global_memory_dir(), &cwd);
    migrate(
        &targets,
        &paths::cache_dir().join(BACKUP_DIR_NAME),
        apply,
        round_chains,
        Local::now().naive_local(),
    )
}

#[derive(Deserialize)]
struct RoundChainsSetting {
    #[serde(default)]
    memory_round_chains: RoundChains,
}

/// The config file's `memory_round_chains`, read on its own so other keys that
/// are invalid but still well-formed YAML do not block a migration. A file
/// that does not parse as YAML fails the run.
pub fn configured_round_chains() -> Result<RoundChains> {
    round_chains_in(&paths::config_file())
}

fn round_chains_in(config_path: &Path) -> Result<RoundChains> {
    let content = match fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RoundChains::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", config_path.display())),
    };
    if content.trim().is_empty() {
        return Ok(RoundChains::default());
    }
    let setting: RoundChainsSetting = serde_yaml::from_str(&content)
        .with_context(|| format!("read memory_round_chains from {}", config_path.display()))?;
    Ok(setting.memory_round_chains)
}

struct Target {
    label: &'static str,
    dir: Option<PathBuf>,
}

fn targets(scope: MigrateScope, global_dir: PathBuf, cwd: &Path) -> Vec<Target> {
    let mut out = Vec::new();
    if matches!(scope, MigrateScope::Global | MigrateScope::All) {
        out.push(Target {
            label: "global",
            dir: Some(global_dir),
        });
    }
    if matches!(scope, MigrateScope::Workspace | MigrateScope::All) {
        out.push(Target {
            label: "workspace",
            dir: discover_workspace_memory(cwd).map(|ws| ws.dir),
        });
    }
    out
}

fn migrate(
    targets: &[Target],
    backup_root: &Path,
    apply: bool,
    round_chains: RoundChains,
    now: NaiveDateTime,
) -> Result<Report> {
    let today = now.format("%Y-%m-%d").to_string();
    let stamp = now.format("%Y%m%dT%H%M%S").to_string();
    let mut scopes = plan_all(targets, &today, round_chains)?;
    let failure = if apply {
        apply_all(&mut scopes, backup_root, &stamp)?
    } else {
        None
    };
    Ok(Report {
        apply,
        scopes,
        failure,
    })
}

fn plan_all(
    targets: &[Target],
    today: &str,
    round_chains: RoundChains,
) -> Result<Vec<ScopeOutcome>> {
    targets
        .iter()
        .map(
            |target| match target.dir.as_ref().filter(|dir| dir.is_dir()) {
                Some(dir) => Ok(ScopeOutcome::Migrated(Box::new(plan_scope(
                    target.label,
                    dir,
                    today,
                    round_chains,
                )?))),
                None => Ok(ScopeOutcome::Skipped {
                    label: target.label,
                    dir: target.dir.clone(),
                }),
            },
        )
        .collect()
}

/// Every scope's index and planned rewrites are checked before the first
/// backup, so one stale scope refuses the whole run with nothing written. A
/// failure after that stops at the failing scope, whose report keeps the
/// backup and the files already written; the scopes after it are marked not
/// applied.
fn apply_all(
    scopes: &mut [ScopeOutcome],
    backup_root: &Path,
    stamp: &str,
) -> Result<Option<ScopeFailure>> {
    for scope in scopes.iter() {
        if let ScopeOutcome::Migrated(report) = scope
            && report.change_count() > 0
        {
            ensure_index_unchanged(report, STALE_BEFORE_APPLY)?;
            for rewrite in &report.rewrites {
                ensure_rewrite_unchanged(rewrite, STALE_BEFORE_APPLY)?;
            }
        }
    }
    let failed = scopes.iter_mut().enumerate().find_map(|(i, scope)| {
        let ScopeOutcome::Migrated(report) = scope else {
            return None;
        };
        if report.change_count() == 0 {
            return None;
        }
        let error = apply_scope(report, backup_root, stamp).err()?;
        Some((
            i,
            ScopeFailure {
                label: report.label,
                backup: report.backup.clone(),
                error,
            },
        ))
    });
    let Some((i, failure)) = failed else {
        return Ok(None);
    };
    for scope in &mut scopes[i + 1..] {
        if let ScopeOutcome::Migrated(report) = scope {
            report.not_applied = report.change_count() > 0;
        }
    }
    Ok(Some(failure))
}

pub struct Report {
    apply: bool,
    scopes: Vec<ScopeOutcome>,
    failure: Option<ScopeFailure>,
}

impl Report {
    /// The error of a scope that failed partway through `--apply`, naming
    /// its backup. Print the report first: it lists what was already written.
    pub fn into_result(self) -> Result<()> {
        let Some(failure) = self.failure else {
            return Ok(());
        };
        let summary = match &failure.backup {
            Some(backup) => format!(
                "{} migration stopped partway; its backup is at {}",
                failure.label,
                shown(backup)
            ),
            None => format!(
                "{} migration failed before any of its files were written",
                failure.label
            ),
        };
        Err(failure.error.context(summary))
    }
}

struct ScopeFailure {
    label: &'static str,
    backup: Option<PathBuf>,
    error: anyhow::Error,
}

enum ScopeOutcome {
    Skipped {
        label: &'static str,
        dir: Option<PathBuf>,
    },
    Migrated(Box<ScopeReport>),
}

struct Drift {
    slug: String,
    index_desc: String,
    frontmatter_desc: String,
}

struct Rewrite {
    file: MemoryFile,
    /// The file's bytes when it was planned; apply replaces only these.
    planned: Vec<u8>,
}

#[derive(Default)]
struct ScopeReport {
    label: &'static str,
    dir: PathBuf,
    facts_lines: Vec<String>,
    catalogue_lines_dropped: usize,
    kept_no_file: Vec<(String, String)>,
    kept_unreadable_file: Vec<(String, String)>,
    drill_heading_dropped: bool,
    backfilled: Vec<String>,
    drift: Vec<Drift>,
    superseded_stamps: Vec<(String, String)>,
    run_state_stamps: Vec<String>,
    oversized: Vec<(String, usize)>,
    long_descriptions: Vec<(String, usize)>,
    unknown_types: Vec<(String, String)>,
    expired: Vec<(String, String)>,
    missing_targets: Vec<(String, String)>,
    orphans: Vec<String>,
    unreadable: Vec<String>,
    index: Option<String>,
    new_index: Option<String>,
    rewrites: Vec<Rewrite>,
    backup: Option<PathBuf>,
    written: Vec<PathBuf>,
    not_applied: bool,
}

impl ScopeReport {
    fn change_count(&self) -> usize {
        self.rewrites.len() + usize::from(self.new_index.is_some())
    }
}

fn plan_scope(
    label: &'static str,
    dir: &Path,
    today: &str,
    round_chains: RoundChains,
) -> Result<ScopeReport> {
    let mut report = ScopeReport {
        label,
        dir: dir.to_path_buf(),
        ..Default::default()
    };
    let mut on_disk = BTreeMap::new();
    let mut unreadable_slugs = BTreeSet::new();
    let mut files = load_drill_files(
        dir,
        &mut on_disk,
        &mut unreadable_slugs,
        &mut report.unreadable,
    )?;
    let mut changed: BTreeSet<String> = BTreeSet::new();

    let index_path = dir.join(MEMORY_INDEX_FILE_NAME);
    let index = match fs::symlink_metadata(&index_path) {
        Ok(meta) if !meta.is_file() => return Err(irregular_index(&index_path)),
        Ok(_) => Some(
            fs::read_to_string(&index_path)
                .with_context(|| format!("read {}", index_path.display()))?,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("stat {}", index_path.display())),
    };
    let mut catalogued: HashSet<String> = HashSet::new();
    if let Some(index) = &index {
        let mut kept: Vec<&str> = Vec::new();
        for raw in index.split_inclusive('\n') {
            let line = raw.trim_end_matches(['\r', '\n']);
            let Some((slug, line_desc)) = catalogue_entry(line) else {
                kept.push(raw);
                continue;
            };
            let entry = (slug.to_string(), line_desc.to_string());
            if !dir.join(format!("{slug}.md")).exists() {
                report.kept_no_file.push(entry);
                kept.push(raw);
                continue;
            }
            let Some(file) = files.get_mut(slug) else {
                report.kept_unreadable_file.push(entry);
                kept.push(raw);
                continue;
            };
            report.catalogue_lines_dropped += 1;
            catalogued.insert(slug.to_string());
            let file_desc = file
                .frontmatter
                .description
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(String::from);
            match file_desc {
                None if !line_desc.is_empty() => {
                    file.frontmatter.description = Some(line_desc.to_string());
                    report.backfilled.push(slug.to_string());
                    changed.insert(slug.to_string());
                }
                Some(frontmatter_desc)
                    if !line_desc.is_empty() && frontmatter_desc != line_desc =>
                {
                    report.drift.push(Drift {
                        slug: slug.to_string(),
                        index_desc: line_desc.to_string(),
                        frontmatter_desc,
                    });
                }
                _ => {}
            }
        }

        if let Some(start) = kept.iter().position(|l| l.trim() == DRILL_FILES_HEADING) {
            let end = kept[start + 1..]
                .iter()
                .position(|l| ends_drill_section(l))
                .map_or(kept.len(), |n| start + 1 + n);
            if kept[start + 1..end].iter().all(|l| l.trim().is_empty()) {
                let at_end = end == kept.len();
                kept.drain(start..end);
                // Mid-index, the blank line before the heading now separates the
                // previous section from the next one; at the end it is stray.
                if at_end {
                    while kept.last().is_some_and(|l| l.trim().is_empty()) {
                        kept.pop();
                    }
                }
                report.drill_heading_dropped = true;
            }
        }

        report.facts_lines = kept
            .iter()
            .map(|raw| raw.trim_end_matches(['\r', '\n']))
            .filter(|line| !line.trim().is_empty())
            .map(String::from)
            .collect();
        let rewritten = kept.concat();
        if rewritten != *index {
            report.new_index = Some(rewritten);
        }
    }
    report.index = index;

    stamp_round_chains(
        dir,
        round_chains,
        &mut files,
        &unreadable_slugs,
        &mut report,
        &mut changed,
    )?;
    for slug in &changed {
        if let Some(file) = files.get_mut(slug) {
            file.frontmatter.updated = Some(today.to_string());
        }
    }

    for (slug, file) in &files {
        let frontmatter = &file.frontmatter;
        if file.char_len() > MEMORY_FILE_HARD_CAP {
            report.oversized.push((slug.clone(), file.char_len()));
        }
        if let Some(desc) = &frontmatter.description {
            let chars = desc.chars().count();
            if chars > MEMORY_DESCRIPTION_CAP {
                report.long_descriptions.push((slug.clone(), chars));
            }
        }
        let kind = &frontmatter.kind;
        if !kind.raw_kind().is_null() && !kind.is_known() {
            let raw = serde_yaml::to_string(kind.raw_kind())?;
            report
                .unknown_types
                .push((slug.clone(), raw.trim().to_string()));
        }
        if let Some(expires) = &frontmatter.expires
            && expires.as_str() < today
        {
            report.expired.push((slug.clone(), expires.clone()));
        }
        if let Some(target) = &frontmatter.superseded_by
            && !files.contains_key(target)
            && !unreadable_slugs.contains(target)
        {
            report.missing_targets.push((slug.clone(), target.clone()));
        }
        if !catalogued.contains(slug) {
            report.orphans.push(slug.clone());
        }
    }

    report.rewrites = changed
        .iter()
        .filter_map(|slug| {
            Some(Rewrite {
                file: files.get(slug)?.clone(),
                planned: on_disk.remove(slug)?,
            })
        })
        .collect();
    Ok(report)
}

/// A `#` or `##` heading closes the drill section; deeper headings belong to it.
fn ends_drill_section(line: &str) -> bool {
    let line = line.trim_start();
    let level = line.chars().take_while(|&c| c == '#').count();
    (1..=2).contains(&level) && line[level..].chars().next().is_none_or(char::is_whitespace)
}

fn irregular_index(path: &Path) -> anyhow::Error {
    anyhow!(
        "{} is not a regular file; migrate only rewrites a plain index",
        path.display()
    )
}

/// The regular `*.md` files of `dir` other than the index, keyed by file
/// stem, with the bytes each was loaded from in `on_disk` and the stems that
/// failed to load in `unreadable_slugs`. Symlinks are left alone so a rewrite
/// never lands outside the dir.
fn load_drill_files(
    dir: &Path,
    on_disk: &mut BTreeMap<String, Vec<u8>>,
    unreadable_slugs: &mut BTreeSet<String>,
    unreadable: &mut Vec<String>,
) -> Result<BTreeMap<String, MemoryFile>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md")
            || entry.file_name() == MEMORY_INDEX_FILE_NAME
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Read before the load, so an edit landing in between makes the bytes
        // stale rather than the parsed file, and apply refuses it.
        let loaded = fs::read(&path)
            .with_context(|| format!("read memory file {}", path.display()))
            .and_then(|bytes| Ok((bytes, MemoryFile::load(&path)?)));
        match loaded {
            Ok((bytes, mut file)) => {
                drop_empty_fields(&mut file.frontmatter);
                files.insert(slug.to_string(), file);
                on_disk.insert(slug.to_string(), bytes);
            }
            Err(e) => {
                unreadable_slugs.insert(slug.to_string());
                unreadable.push(format!("{}: {e:#}", path.display()));
            }
        }
    }
    Ok(files)
}

/// An empty `description` or `type` reads as absent, so it is neither
/// reported nor written back.
fn drop_empty_fields(frontmatter: &mut MemoryFrontmatter) {
    if frontmatter
        .description
        .as_deref()
        .is_some_and(|d| d.trim().is_empty())
    {
        frontmatter.description = None;
    }
    if frontmatter
        .kind
        .raw_kind()
        .as_str()
        .is_some_and(|k| k.trim().is_empty())
    {
        frontmatter.kind = KindField::default();
    }
}

/// Within each `<base>-r<N>` chain, points every unmarked round below the
/// highest at it unless `round_chains` keeps them, and types each member
/// without a `type` as run-state. The highest round comes from the directory
/// listing, so it heads the chain even when its file is unreadable. Only
/// regular files take part, and a readable member whose front matter names
/// another memory is left out and reported, as `memory__write` refuses it.
fn stamp_round_chains(
    dir: &Path,
    round_chains: RoundChains,
    files: &mut BTreeMap<String, MemoryFile>,
    unreadable_slugs: &BTreeSet<String>,
    report: &mut ScopeReport,
    changed: &mut BTreeSet<String>,
) -> Result<()> {
    let bases: BTreeSet<String> = files
        .keys()
        .filter_map(|slug| round_chain_name(slug))
        .map(|(base, _)| base.to_string())
        .collect();
    for base in &bases {
        let mut members = Vec::new();
        for member in scan_round_siblings(dir, base)? {
            let path = dir.join(format!("{}.md", member.name));
            let regular = fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file());
            if !regular && !unreadable_slugs.contains(&member.name) {
                continue;
            }
            if let Some(file) = files.get(&member.name)
                && file.frontmatter.name != member.name
            {
                report.unreadable.push(format!(
                    "{}: front matter name '{}' does not match the file name",
                    path.display(),
                    file.frontmatter.name
                ));
                continue;
            }
            members.push(member);
        }
        let Some(head) = members.last().map(|m| m.name.clone()) else {
            continue;
        };
        for member in &members {
            let slug = &member.name;
            let Some(file) = files.get_mut(slug) else {
                continue;
            };
            if *slug != head
                && marks_older_round(round_chains, file.frontmatter.superseded_by.as_deref())
            {
                file.frontmatter.superseded_by = Some(head.clone());
                report.superseded_stamps.push((slug.clone(), head.clone()));
                changed.insert(slug.clone());
            }
            if file.frontmatter.kind.raw_kind().is_null() {
                file.frontmatter.kind = MemoryKind::RunState.into();
                report.run_state_stamps.push(slug.clone());
                changed.insert(slug.clone());
            }
        }
    }
    Ok(())
}

fn changed_since_planned(path: &Path, outcome: &str) -> anyhow::Error {
    anyhow!(
        "{} changed since the migration was planned, so {outcome}. Rerun --memory-migrate",
        path.display(),
    )
}

fn ensure_index_unchanged(report: &ScopeReport, outcome: &str) -> Result<()> {
    if report.new_index.is_none() {
        return Ok(());
    }
    let index_path = report.dir.join(MEMORY_INDEX_FILE_NAME);
    let meta = fs::symlink_metadata(&index_path)
        .with_context(|| format!("stat {}", index_path.display()))?;
    if !meta.is_file() {
        return Err(irregular_index(&index_path));
    }
    let current = fs::read_to_string(&index_path)
        .with_context(|| format!("read {}", index_path.display()))?;
    if report.index.as_ref() != Some(&current) {
        return Err(changed_since_planned(&index_path, outcome));
    }
    Ok(())
}

/// A rewrite target that vanished or stopped being a regular file counts as
/// changed.
fn ensure_rewrite_unchanged(rewrite: &Rewrite, outcome: &str) -> Result<()> {
    let path = &rewrite.file.path;
    let current = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {
            Some(fs::read(path).with_context(|| format!("read {}", path.display()))?)
        }
        Ok(_) => None,
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if current.as_ref() != Some(&rewrite.planned) {
        return Err(changed_since_planned(path, outcome));
    }
    Ok(())
}

/// `MemoryFile::render` writes `description` and `type` even when unset; a
/// rewrite leaves an unset one out instead of writing it as null.
fn render_rewrite(file: &MemoryFile) -> Result<String> {
    let rendered = file.render()?;
    if file.frontmatter.description.is_some() && !file.frontmatter.kind.raw_kind().is_null() {
        return Ok(rendered);
    }
    let yaml = rendered
        .strip_prefix("---\n")
        .and_then(|rest| rest.strip_suffix(file.body.as_str()))
        .and_then(|rest| rest.strip_suffix("---\n\n"))
        .ok_or_else(|| anyhow!("rendered front matter is not delimited as expected"))?;
    let mut frontmatter: serde_yaml::Mapping = serde_yaml::from_str(yaml)?;
    for key in ["description", "type"] {
        if frontmatter.get(key).is_some_and(serde_yaml::Value::is_null) {
            frontmatter.shift_remove(key);
        }
    }
    Ok(format!(
        "---\n{}---\n\n{}",
        serde_yaml::to_string(&frontmatter)?,
        file.body
    ))
}

fn apply_scope(report: &mut ScopeReport, backup_root: &Path, stamp: &str) -> Result<()> {
    let backup = unused_backup_dir(backup_root, report.label, stamp);
    copy_dir_all(&report.dir, &backup, &|name| {
        name.to_string_lossy().starts_with(".catalogue.")
    })
    .with_context(|| {
        if backup.exists() {
            format!(
                "back up {}: its backup did not complete (partial copy at {})",
                report.dir.display(),
                backup.display()
            )
        } else {
            format!("back up {} to {}", report.dir.display(), backup.display())
        }
    })?;
    report.backup = Some(backup.clone());
    for rewrite in &report.rewrites {
        let path = &rewrite.file.path;
        let rendered = render_rewrite(&rewrite.file).with_context(|| {
            format!(
                "render memory file {} (backup at {})",
                path.display(),
                backup.display()
            )
        })?;
        // An edit landing between this check and the write is still lost.
        ensure_rewrite_unchanged(rewrite, STALE_DURING_APPLY)
            .and_then(|()| Ok(write_private_atomic(path, &rendered)?))
            .with_context(|| {
                format!(
                    "write memory file {} (backup at {})",
                    path.display(),
                    backup.display()
                )
            })?;
        report.written.push(path.clone());
    }
    before_index_write();
    if let Some(index) = &report.new_index {
        let index_path = report.dir.join(MEMORY_INDEX_FILE_NAME);
        // An edit landing between this check and the write is still lost.
        ensure_index_unchanged(report, STALE_DURING_APPLY)
            .and_then(|()| Ok(write_private_atomic(&index_path, index)?))
            .with_context(|| {
                format!(
                    "write {} (backup at {})",
                    index_path.display(),
                    backup.display()
                )
            })?;
        report.written.push(index_path);
    }
    Ok(())
}

/// Runs after the drill files are written, so a test can edit the index in
/// the window the second index check guards. A no-op outside tests.
#[cfg(not(test))]
fn before_index_write() {}

#[cfg(test)]
fn before_index_write() {
    if let Some(hook) = BEFORE_INDEX_WRITE.with(|slot| slot.take()) {
        hook();
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_INDEX_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `apply_scope` with `hook` armed to run once on this thread at
/// `before_index_write`.
#[cfg(test)]
fn apply_scope_with(
    report: &mut ScopeReport,
    backup_root: &Path,
    stamp: &str,
    hook: impl FnOnce() + 'static,
) -> Result<()> {
    BEFORE_INDEX_WRITE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    let result = apply_scope(report, backup_root, stamp);
    BEFORE_INDEX_WRITE.with(|slot| slot.borrow_mut().take());
    result
}

/// A second migration within the same second gets its own backup dir
/// instead of overwriting the first one's.
fn unused_backup_dir(backup_root: &Path, label: &str, stamp: &str) -> PathBuf {
    let mut candidate = backup_root.join(format!("{label}-{stamp}"));
    let mut n = 1;
    while candidate.exists() {
        n += 1;
        candidate = backup_root.join(format!("{label}-{stamp}-{n}"));
    }
    candidate
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for scope in &self.scopes {
            match scope {
                ScopeOutcome::Skipped {
                    label,
                    dir: Some(dir),
                } => writeln!(f, "{label}: skipped, no memory dir at {}", shown(dir))?,
                ScopeOutcome::Skipped { label, dir: None } => writeln!(
                    f,
                    "{label}: skipped, no workspace memory found from the current directory"
                )?,
                ScopeOutcome::Migrated(report) => write_scope(f, report, self.apply)?,
            }
        }
        if !self.apply {
            return writeln!(
                f,
                "report only, nothing written; rerun with --apply to migrate"
            );
        }
        if let Some(failure) = &self.failure {
            writeln!(
                f,
                "stopped: {}: {}",
                failure.label,
                sanitize_display_text(&format!("{:#}", failure.error))
            )?;
        }
        for scope in &self.scopes {
            if let ScopeOutcome::Migrated(report) = scope
                && let Some(backup) = &report.backup
            {
                writeln!(f, "backup: {}", shown(backup))?;
            }
        }
        if self.failure.is_some() {
            writeln!(
                f,
                "next: fix the error and rerun --memory-migrate; each backup above holds its \
                 store as it was before this run"
            )
        } else {
            writeln!(
                f,
                "next: review the remaining sections by hand; migrate leaves them as they are"
            )
        }
    }
}

fn write_scope(f: &mut fmt::Formatter<'_>, report: &ScopeReport, apply: bool) -> fmt::Result {
    writeln!(f, "{}: {}", report.label, shown(&report.dir))?;
    writeln!(f, "  changes: {}", report.change_count())?;
    write_section(
        f,
        "facts lines kept",
        report.facts_lines.clone(),
        FACTS_PREVIEW_LINES,
    )?;
    writeln!(
        f,
        "  catalogue lines dropped: {}",
        report.catalogue_lines_dropped
    )?;
    if report.drill_heading_dropped {
        writeln!(f, "  empty `{DRILL_FILES_HEADING}` heading dropped")?;
    }
    write_section(
        f,
        "catalogue lines kept, no drill file",
        pairs(&report.kept_no_file, ": "),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "catalogue lines kept, drill file unreadable",
        pairs(&report.kept_unreadable_file, ": "),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "descriptions backfilled from the index",
        report.backfilled.clone(),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "description drift, front matter kept",
        report
            .drift
            .iter()
            .map(|d| {
                format!(
                    "{}: index {:?}, front matter {:?}",
                    d.slug, d.index_desc, d.frontmatter_desc
                )
            })
            .collect(),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "superseded_by stamped",
        pairs(&report.superseded_stamps, " -> "),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "type set to run-state",
        report.run_state_stamps.clone(),
        SECTION_LIST_CAP,
    )?;
    if !report.run_state_stamps.is_empty() {
        writeln!(
            f,
            "    these are now run-state: no longer inlined into the prompt, and \
             lint flags each until it has an expires date"
        )?;
    }
    write_section(
        f,
        &format!("bodies over {MEMORY_FILE_HARD_CAP} chars"),
        pairs(&report.oversized, ": "),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        &format!("descriptions over {MEMORY_DESCRIPTION_CAP} chars"),
        pairs(&report.long_descriptions, ": "),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "unknown types",
        pairs(&report.unknown_types, ": "),
        SECTION_LIST_CAP,
    )?;
    write_section(f, "expired", pairs(&report.expired, ": "), SECTION_LIST_CAP)?;
    write_section(
        f,
        "superseded with a missing target",
        pairs(&report.missing_targets, " -> "),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "orphans (already searchable)",
        report.orphans.clone(),
        SECTION_LIST_CAP,
    )?;
    write_section(
        f,
        "unreadable files",
        report.unreadable.clone(),
        SECTION_LIST_CAP,
    )?;
    if apply {
        if report.not_applied {
            writeln!(f, "  not applied: an earlier scope failed")?;
            return write_section(f, "files to rewrite", pending(report), SECTION_LIST_CAP);
        }
        write_section(
            f,
            "written",
            report
                .written
                .iter()
                .map(|p| format!("wrote {}", p.display()))
                .collect(),
            SECTION_LIST_CAP,
        )?;
        if report.change_count() == 0 {
            writeln!(f, "  backup: none, nothing to write")?;
        }
        Ok(())
    } else {
        write_section(f, "files to rewrite", pending(report), SECTION_LIST_CAP)
    }
}

fn pending(report: &ScopeReport) -> Vec<String> {
    let mut pending: Vec<String> = report
        .rewrites
        .iter()
        .map(|rewrite| rewrite.file.path.display().to_string())
        .collect();
    if report.new_index.is_some() {
        pending.push(
            report
                .dir
                .join(MEMORY_INDEX_FILE_NAME)
                .display()
                .to_string(),
        );
    }
    pending
}

fn pairs<A: fmt::Display, B: fmt::Display>(items: &[(A, B)], sep: &str) -> Vec<String> {
    items.iter().map(|(a, b)| format!("{a}{sep}{b}")).collect()
}

fn shown(path: &Path) -> String {
    sanitize_display_text(&path.display().to_string())
}

/// Every item comes from the memory dir, which a cloned repo controls, so
/// terminal escapes in it are stripped before printing.
fn write_section(
    f: &mut fmt::Formatter<'_>,
    title: &str,
    items: Vec<String>,
    cap: usize,
) -> fmt::Result {
    writeln!(f, "  {title}: {}", items.len())?;
    for item in items.iter().take(cap) {
        writeln!(f, "    {}", sanitize_display_text(item))?;
    }
    if items.len() > cap {
        writeln!(f, "    +{} more", items.len() - cap)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::memory::catalogue_slug;
    use crate::config::{MEMORY_DIR_NAME, WORKSPACE_COYOTE_DIR_NAME};
    use chrono::NaiveDate;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!(
                "coyote-memory-migrate-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const GLOBAL_INDEX: &str = "PLACEHOLDER_CHECK\n- [[g-note]]: global note\n\
        artefact line left by an old tool\n";

    const WORKSPACE_FACTS: &str = "## Always-on facts\n\
        - **Fact A.** cites [[plan-r1]] mid-sentence\n\
        - **Fact B.** text\n- **Fact C.** text\n- **Fact D.** text\n";

    fn workspace_index() -> String {
        format!(
            "# Workspace Memory Index\n\n{WORKSPACE_FACTS}\n## Drill files\n\
             - [[plan-r1]]: first round\n- [[plan-r2]]: second round\n\
             - [[plan-r3]]: third round\n- [[drifted]]: index wording\n\
             - [[blank]]: backfilled from the index\n- [[gotcha]]: odd type\n\
             - [[huge]]: big body\n- [[deleted]]: file removed\n\
             - [[broken]]: unreadable file\n"
        )
    }

    struct Fixture {
        _root: TempDir,
        global: PathBuf,
        workspace_root: PathBuf,
        workspace: PathBuf,
        backups: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = TempDir::new(label);
            let global = root.0.join("config").join("memory");
            let workspace_root = root.0.join("repo");
            let workspace = workspace_root
                .join(WORKSPACE_COYOTE_DIR_NAME)
                .join(MEMORY_DIR_NAME);
            let backups = root.0.join("cache").join(BACKUP_DIR_NAME);
            fs::create_dir_all(&global).unwrap();
            fs::create_dir_all(&workspace).unwrap();

            fs::write(global.join(MEMORY_INDEX_FILE_NAME), GLOBAL_INDEX).unwrap();
            write_file(
                &global,
                "g-note",
                "description: global note\ntype: user\n",
                "g",
            );

            fs::write(workspace.join(MEMORY_INDEX_FILE_NAME), workspace_index()).unwrap();
            write_file(
                &workspace,
                "plan-r1",
                "description: first round\ntype: null\n",
                "r1",
            );
            write_file(
                &workspace,
                "plan-r2",
                "description: second round\ntype: project\n",
                "r2",
            );
            write_file(&workspace, "plan-r3", "description: third round\n", "r3");
            write_file(
                &workspace,
                "drifted",
                "description: front matter wording\ntype: user\n",
                "d",
            );
            write_file(&workspace, "blank", "type: user\ntags: [x]\n", "b");
            write_file(
                &workspace,
                "gotcha",
                "description: odd type\ntype: gotcha\n",
                "g",
            );
            write_file(
                &workspace,
                "huge",
                "description: big body\ntype: reference\n",
                &"x".repeat(MEMORY_FILE_HARD_CAP + 1),
            );
            write_file(
                &workspace,
                "orphan",
                "description: never indexed\ntype: user\n",
                "o",
            );
            write_file(
                &workspace,
                "bygone",
                "description: old\ntype: user\nexpires: 2000-01-01\nsuperseded_by: nowhere\n",
                "old",
            );
            write_file(
                &workspace,
                "wordy",
                &format!("description: {}\ntype: user\n", "w".repeat(161)),
                "w",
            );
            fs::write(workspace.join(".catalogue.yaml"), "cache").unwrap();
            fs::write(
                workspace.join("broken.md"),
                "---\nname: [unclosed\n---\n\nb\n",
            )
            .unwrap();

            Self {
                _root: root,
                global,
                workspace_root,
                workspace,
                backups,
            }
        }

        fn targets(&self, scope: MigrateScope) -> Vec<Target> {
            targets(scope, self.global.clone(), &self.workspace_root)
        }

        fn migrate(&self, apply: bool) -> Report {
            migrate(
                &self.targets(MigrateScope::All),
                &self.backups,
                apply,
                RoundChains::default(),
                now(),
            )
            .unwrap()
        }
    }

    fn write_file(dir: &Path, name: &str, extra_frontmatter: &str, body: &str) {
        fs::write(
            dir.join(format!("{name}.md")),
            format!("---\nname: {name}\n{extra_frontmatter}---\n\n{body}\n"),
        )
        .unwrap();
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(12, 30, 45)
            .unwrap()
    }

    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    fn scope<'a>(report: &'a Report, label: &str) -> &'a ScopeReport {
        report
            .scopes
            .iter()
            .find_map(|s| match s {
                ScopeOutcome::Migrated(r) if r.label == label => Some(r.as_ref()),
                _ => None,
            })
            .unwrap()
    }

    fn load(dir: &Path, name: &str) -> MemoryFile {
        MemoryFile::load(&dir.join(format!("{name}.md"))).unwrap()
    }

    #[test]
    fn report_only_writes_nothing() {
        let fx = Fixture::new("report_only");
        let global_before = snapshot(&fx.global);
        let workspace_before = snapshot(&fx.workspace);

        let report = fx.migrate(false);

        assert_eq!(snapshot(&fx.global), global_before);
        assert_eq!(snapshot(&fx.workspace), workspace_before);
        assert!(!fx.backups.exists());
        assert_eq!(scope(&report, "global").change_count(), 1);
        assert_eq!(scope(&report, "workspace").change_count(), 5);
        let text = report.to_string();
        assert!(text.contains("  files to rewrite: 5\n"), "{text}");
        assert!(!text.contains("wrote "), "{text}");
        assert!(text.ends_with("rerun with --apply to migrate\n"), "{text}");
    }

    #[test]
    fn apply_backs_up_the_untouched_dir_before_rewriting() {
        let fx = Fixture::new("apply_backup");
        let global_before = snapshot(&fx.global);
        let workspace_before = snapshot(&fx.workspace);

        let report = fx.migrate(true);

        let global = scope(&report, "global");
        let workspace = scope(&report, "workspace");
        assert_eq!(
            global.backup.as_deref(),
            Some(fx.backups.join("global-20260301T123045").as_path())
        );
        assert_eq!(snapshot(global.backup.as_ref().unwrap()), global_before);
        let catalogue = PathBuf::from(".catalogue.yaml");
        let mut workspace_backup = snapshot(workspace.backup.as_ref().unwrap());
        assert!(!workspace_backup.contains_key(&catalogue));
        assert!(fx.workspace.join(&catalogue).exists());
        workspace_backup.insert(catalogue, b"cache".to_vec());
        assert_eq!(workspace_backup, workspace_before);
        assert_ne!(snapshot(&fx.workspace), workspace_before);
        assert_eq!(workspace.written.len(), 5);

        let text = report.to_string();
        assert!(
            text.contains(&format!(
                "    wrote {}\n",
                fx.workspace.join(MEMORY_INDEX_FILE_NAME).display()
            )),
            "{text}"
        );
        assert!(
            text.contains(
                "  type set to run-state: 2\n    plan-r1\n    plan-r3\n    these are now run-state"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_applied_report_ends_with_every_backup_then_next() {
        let fx = Fixture::new("apply_tail");

        let text = fx.migrate(true).to_string();

        let lines: Vec<&str> = text.lines().collect();
        let tail = [
            format!(
                "backup: {}",
                fx.backups.join("global-20260301T123045").display()
            ),
            format!(
                "backup: {}",
                fx.backups.join("workspace-20260301T123045").display()
            ),
            "next: review the remaining sections by hand; migrate leaves them as they are"
                .to_string(),
        ];
        assert_eq!(lines[lines.len() - tail.len()..], tail, "{text}");
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("backup: ")).count(),
            2,
            "{text}"
        );
        assert!(!text.contains("  backup: "), "{text}");
    }

    #[test]
    fn a_second_apply_changes_nothing() {
        let fx = Fixture::new("idempotent");
        fx.migrate(true);
        let global_after = snapshot(&fx.global);
        let workspace_after = snapshot(&fx.workspace);
        let backups_after = snapshot(&fx.backups);

        let report = fx.migrate(true);

        for label in ["global", "workspace"] {
            let outcome = scope(&report, label);
            assert_eq!(outcome.change_count(), 0, "{label}");
            assert!(outcome.backup.is_none(), "{label}");
        }
        assert_eq!(snapshot(&fx.global), global_after);
        assert_eq!(snapshot(&fx.workspace), workspace_after);
        assert_eq!(snapshot(&fx.backups), backups_after);
        assert!(
            report
                .to_string()
                .contains("  backup: none, nothing to write\n")
        );
    }

    #[test]
    fn apply_keeps_the_facts_and_lines_without_a_drill_file_byte_identical() {
        let fx = Fixture::new("facts");

        let report = fx.migrate(true);

        let workspace_index =
            fs::read_to_string(fx.workspace.join(MEMORY_INDEX_FILE_NAME)).unwrap();
        assert_eq!(
            workspace_index,
            format!(
                "# Workspace Memory Index\n\n{WORKSPACE_FACTS}\n## Drill files\n\
                 - [[deleted]]: file removed\n- [[broken]]: unreadable file\n"
            )
        );
        let global_index = fs::read_to_string(fx.global.join(MEMORY_INDEX_FILE_NAME)).unwrap();
        assert_eq!(
            global_index,
            "PLACEHOLDER_CHECK\nartefact line left by an old tool\n"
        );
        let catalogued: Vec<&str> = workspace_index
            .lines()
            .chain(global_index.lines())
            .filter_map(catalogue_slug)
            .collect();
        assert_eq!(catalogued, vec!["deleted", "broken"]);

        let workspace = scope(&report, "workspace");
        assert!(!workspace.drill_heading_dropped);
        assert_eq!(workspace.catalogue_lines_dropped, 7);
        assert_eq!(
            workspace.kept_no_file,
            vec![("deleted".to_string(), "file removed".to_string())]
        );
        assert_eq!(
            workspace.kept_unreadable_file,
            vec![("broken".to_string(), "unreadable file".to_string())]
        );
        assert_eq!(workspace.unreadable.len(), 1, "{:?}", workspace.unreadable);
        let text = report.to_string();
        assert!(
            text.contains(
                "  catalogue lines kept, no drill file: 1\n    deleted: file removed\n\
                 \x20 catalogue lines kept, drill file unreadable: 1\n    broken: unreadable file\n"
            ),
            "{text}"
        );
        assert_eq!(workspace.facts_lines.len(), 9);
        let global = scope(&report, "global");
        assert_eq!(
            global.facts_lines,
            vec!["PLACEHOLDER_CHECK", "artefact line left by an old tool"]
        );
        assert!(!global.drill_heading_dropped);
    }

    #[cfg(unix)]
    #[test]
    fn a_line_whose_drill_file_is_a_symlink_stays_in_the_index() {
        let fx = Fixture::new("symlinked_drill");
        let outside = fx._root.0.join("outside.md");
        fs::write(&outside, "---\nname: linked\n---\n\nl\n").unwrap();
        std::os::unix::fs::symlink(&outside, fx.global.join("linked.md")).unwrap();
        let index_path = fx.global.join(MEMORY_INDEX_FILE_NAME);
        fs::write(
            &index_path,
            format!("{GLOBAL_INDEX}- [[linked]]: symlinked file\n"),
        )
        .unwrap();

        let report = fx.migrate(true);

        assert_eq!(
            fs::read_to_string(&index_path).unwrap(),
            "PLACEHOLDER_CHECK\nartefact line left by an old tool\n\
             - [[linked]]: symlinked file\n"
        );
        let global = scope(&report, "global");
        assert_eq!(global.catalogue_lines_dropped, 1);
        assert_eq!(
            global.kept_unreadable_file,
            vec![("linked".to_string(), "symlinked file".to_string())]
        );
    }

    #[test]
    fn a_drill_section_left_empty_is_dropped_with_its_heading() {
        let fx = Fixture::new("heading_drop");
        let facts = "## Always-on facts\n- **Fact.** kept\n";
        let index_path = fx.global.join(MEMORY_INDEX_FILE_NAME);
        fs::write(
            &index_path,
            format!("{facts}\n## Drill files\n- [[g-note]]: global note\n\n"),
        )
        .unwrap();

        let report = fx.migrate(true);

        assert_eq!(fs::read_to_string(&index_path).unwrap(), facts);
        assert!(scope(&report, "global").drill_heading_dropped);
        let text = report.to_string();
        assert!(
            text.contains("  empty `## Drill files` heading dropped\n"),
            "{text}"
        );
    }

    #[test]
    fn an_emptied_drill_section_mid_index_is_dropped_and_the_next_section_kept() {
        let fx = Fixture::new("heading_drop_mid");
        let facts = "## Always-on facts\n- **Fact.** kept\n\n";
        let after = "## Later notes\n- **Other.** stays put\n\n# Appendix\ntext\n";
        let index_path = fx.global.join(MEMORY_INDEX_FILE_NAME);
        fs::write(
            &index_path,
            format!("{facts}## Drill files\n- [[g-note]]: global note\n\n{after}"),
        )
        .unwrap();

        let report = fx.migrate(true);

        assert_eq!(
            fs::read_to_string(&index_path).unwrap(),
            format!("{facts}{after}")
        );
        assert!(scope(&report, "global").drill_heading_dropped);
    }

    #[test]
    fn a_drill_section_with_a_subheading_keeps_its_heading() {
        let fx = Fixture::new("heading_keep_sub");
        let facts = "## Always-on facts\n- **Fact.** kept\n\n";
        let rest = "## Drill files\n### Archived\n- **Old.** note\n";
        let index_path = fx.global.join(MEMORY_INDEX_FILE_NAME);
        fs::write(
            &index_path,
            "## Always-on facts\n- **Fact.** kept\n\n## Drill files\n- [[g-note]]: global note\n### Archived\n- **Old.** note\n",
        )
        .unwrap();

        let report = fx.migrate(true);

        assert_eq!(
            fs::read_to_string(&index_path).unwrap(),
            format!("{facts}{rest}")
        );
        assert!(!scope(&report, "global").drill_heading_dropped);
    }

    #[test]
    fn an_empty_description_is_backfilled_and_a_different_one_wins_over_the_index() {
        let fx = Fixture::new("backfill");

        let report = fx.migrate(true);

        assert_eq!(
            load(&fx.workspace, "blank")
                .frontmatter
                .description
                .as_deref(),
            Some("backfilled from the index")
        );
        assert_eq!(
            load(&fx.workspace, "drifted")
                .frontmatter
                .description
                .as_deref(),
            Some("front matter wording")
        );
        let workspace = scope(&report, "workspace");
        assert_eq!(workspace.backfilled, vec!["blank"]);
        assert_eq!(workspace.drift.len(), 1);
        let drift = &workspace.drift[0];
        assert_eq!(
            (
                drift.slug.as_str(),
                drift.index_desc.as_str(),
                drift.frontmatter_desc.as_str()
            ),
            ("drifted", "index wording", "front matter wording")
        );
        assert!(scope(&report, "global").backfilled.is_empty());
    }

    #[test]
    fn a_rewrite_keeps_unknown_keys_and_bumps_updated() {
        let fx = Fixture::new("extra_keys");

        fx.migrate(true);

        let blank = load(&fx.workspace, "blank");
        assert_eq!(
            blank.source.get("tags"),
            Some(&serde_yaml::Value::Sequence(vec!["x".into()]))
        );
        assert_eq!(blank.frontmatter.updated.as_deref(), Some("2026-03-01"));
        let untouched = load(&fx.workspace, "drifted").frontmatter;
        assert_eq!(untouched.updated, None);
    }

    #[test]
    fn an_edited_workspace_index_refuses_the_whole_run() {
        let fx = Fixture::new("index_race");
        let global_before = snapshot(&fx.global);
        let mut scopes = plan_all(
            &fx.targets(MigrateScope::All),
            "2026-03-01",
            RoundChains::default(),
        )
        .unwrap();
        let index_path = fx.workspace.join(MEMORY_INDEX_FILE_NAME);
        let edited = format!("{}- [[late]]: added meanwhile\n", workspace_index());
        fs::write(&index_path, &edited).unwrap();

        let err = apply_all(&mut scopes, &fx.backups, "20260301T123045")
            .err()
            .expect("a stale index refuses the run");

        assert!(
            err.to_string()
                .contains("changed since the migration was planned"),
            "{err:#}"
        );
        assert_eq!(fs::read_to_string(&index_path).unwrap(), edited);
        assert_eq!(snapshot(&fx.global), global_before);
        assert!(!fx.backups.exists());
    }

    #[test]
    fn a_drill_file_edited_after_planning_refuses_the_whole_run() {
        let fx = Fixture::new("drill_race");
        let global_before = snapshot(&fx.global);
        let mut scopes = plan_all(
            &fx.targets(MigrateScope::All),
            "2026-03-01",
            RoundChains::default(),
        )
        .unwrap();
        write_file(
            &fx.workspace,
            "plan-r2",
            "description: edited meanwhile\ntype: project\n",
            "late edit",
        );
        let workspace_before = snapshot(&fx.workspace);

        let err = apply_all(&mut scopes, &fx.backups, "20260301T123045")
            .err()
            .expect("a stale drill file refuses the run");

        assert_eq!(
            err.to_string(),
            format!(
                "{} changed since the migration was planned, so nothing was rewritten. \
                 Rerun --memory-migrate",
                fx.workspace.join("plan-r2.md").display()
            )
        );
        assert_eq!(snapshot(&fx.workspace), workspace_before);
        assert_eq!(snapshot(&fx.global), global_before);
        assert!(!fx.backups.exists());
    }

    #[test]
    fn a_drill_file_removed_after_planning_refuses_the_whole_run() {
        let fx = Fixture::new("drill_gone");
        let mut scopes = plan_all(
            &fx.targets(MigrateScope::All),
            "2026-03-01",
            RoundChains::default(),
        )
        .unwrap();
        let gone = fx.workspace.join("blank.md");
        fs::remove_file(&gone).unwrap();

        let err = apply_all(&mut scopes, &fx.backups, "20260301T123045")
            .err()
            .expect("a vanished drill file refuses the run");

        assert!(
            err.to_string().starts_with(&format!(
                "{} changed since the migration was planned",
                gone.display()
            )),
            "{err:#}"
        );
        assert!(!gone.exists());
        assert!(!fx.backups.exists());
    }

    #[test]
    fn a_workspace_failing_after_global_applied_still_reports_both() {
        let fx = Fixture::new("partial_apply");
        // The name leaves no room for the temp file suffix, so the write fails
        // although the file itself is unchanged since planning.
        let long_base = "z".repeat(245);
        write_file(
            &fx.workspace,
            &format!("{long_base}-r1"),
            "type: user\n",
            "1",
        );
        write_file(
            &fx.workspace,
            &format!("{long_base}-r2"),
            "type: user\n",
            "2",
        );
        let blocked = fx.workspace.join(format!("{long_base}-r1.md"));
        let mut scopes = plan_all(
            &fx.targets(MigrateScope::All),
            "2026-03-01",
            RoundChains::default(),
        )
        .unwrap();

        let failure = apply_all(&mut scopes, &fx.backups, "20260301T123045").unwrap();
        let report = Report {
            apply: true,
            scopes,
            failure,
        };

        let global_backup = fx.backups.join("global-20260301T123045");
        let workspace_backup = fx.backups.join("workspace-20260301T123045");
        let text = report.to_string();
        for expected in [
            format!(
                "    wrote {}\n",
                fx.global.join(MEMORY_INDEX_FILE_NAME).display()
            ),
            format!("    wrote {}\n", fx.workspace.join("blank.md").display()),
        ] {
            assert!(
                text.contains(&expected),
                "{expected:?} missing from\n{text}"
            );
        }
        let lines: Vec<&str> = text.lines().collect();
        let [stopped, tail @ ..] = &lines[lines.len() - 4..] else {
            unreachable!()
        };
        assert!(
            stopped.starts_with(&format!(
                "stopped: workspace: write memory file {} (backup at {}): ",
                blocked.display(),
                workspace_backup.display()
            )),
            "{text}"
        );
        assert_eq!(
            tail,
            [
                format!("backup: {}", global_backup.display()),
                format!("backup: {}", workspace_backup.display()),
                "next: fix the error and rerun --memory-migrate; each backup above holds its \
                 store as it was before this run"
                    .to_string(),
            ],
            "{text}"
        );
        let err = format!("{:#}", report.into_result().unwrap_err());
        assert!(
            err.starts_with(&format!(
                "workspace migration stopped partway; its backup is at {}: write memory file {} \
                 (backup at {})",
                workspace_backup.display(),
                blocked.display(),
                workspace_backup.display()
            )),
            "{err}"
        );
    }

    #[test]
    fn a_global_failure_leaves_the_workspace_unapplied_and_says_so() {
        let fx = Fixture::new("global_fails");
        let workspace_before = snapshot(&fx.workspace);
        fs::create_dir_all(fx.backups.parent().unwrap()).unwrap();
        fs::write(&fx.backups, "not a dir").unwrap();

        let report = migrate(
            &fx.targets(MigrateScope::All),
            &fx.backups,
            true,
            RoundChains::default(),
            now(),
        )
        .unwrap();

        assert_eq!(snapshot(&fx.workspace), workspace_before);
        assert!(scope(&report, "workspace").not_applied);
        let text = report.to_string();
        let workspace_section = text
            .split_once(&format!("workspace: {}\n", fx.workspace.display()))
            .map(|(_, rest)| rest)
            .expect("the workspace section is reported");
        assert!(
            workspace_section
                .contains("  not applied: an earlier scope failed\n  files to rewrite: 5\n"),
            "{text}"
        );
        assert!(
            workspace_section.contains(&format!(
                "    {}\n",
                fx.workspace.join(MEMORY_INDEX_FILE_NAME).display()
            )),
            "{text}"
        );
        assert!(!text.lines().any(|l| l.starts_with("backup: ")), "{text}");
        assert!(text.contains("\nstopped: global: back up "), "{text}");
        let err = format!("{:#}", report.into_result().unwrap_err());
        assert!(
            err.starts_with("global migration failed before any of its files were written: "),
            "{err}"
        );
    }

    #[test]
    fn repo_text_is_printed_without_terminal_escapes() {
        let fx = Fixture::new("escapes");
        fs::write(
            fx.global.join(MEMORY_INDEX_FILE_NAME),
            "- [[ghost]]: \u{1b}[31mred\u{1b}]0;title\u{7}\n",
        )
        .unwrap();

        let text = fx.migrate(false).to_string();

        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{7}'), "{text:?}");
        assert!(text.contains("    ghost: red\n"), "{text}");
    }

    #[test]
    fn an_index_that_is_not_a_regular_file_is_refused() {
        let fx = Fixture::new("index_dir");
        let index_path = fx.global.join(MEMORY_INDEX_FILE_NAME);
        fs::remove_file(&index_path).unwrap();
        fs::create_dir(&index_path).unwrap();

        let err = plan_scope("global", &fx.global, "2026-03-01", RoundChains::default())
            .err()
            .expect("a directory index is refused");

        assert_eq!(
            err.to_string(),
            format!(
                "{} is not a regular file; migrate only rewrites a plain index",
                index_path.display()
            )
        );
    }

    #[test]
    fn apply_writes_private_files_and_leaves_no_temp_files() {
        let fx = Fixture::new("atomic");

        let report = fx.migrate(true);

        for dir in [&fx.global, &fx.workspace] {
            let temps: Vec<PathBuf> = snapshot(dir)
                .into_keys()
                .filter(|p| p.extension().is_some_and(|e| e == "tmp"))
                .collect();
            assert!(temps.is_empty(), "{temps:?}");
        }
        let written: Vec<&PathBuf> = ["global", "workspace"]
            .iter()
            .flat_map(|label| &scope(&report, label).written)
            .collect();
        assert_eq!(written.len(), 6, "{written:?}");
        #[cfg(unix)]
        for path in written {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_at_a_rewritten_path_is_refused_not_followed() {
        let fx = Fixture::new("symlink_swap");
        let mut plan = plan_scope(
            "workspace",
            &fx.workspace,
            "2026-03-01",
            RoundChains::default(),
        )
        .unwrap();
        let drill = fx.workspace.join("blank.md");
        assert!(plan.rewrites.iter().any(|r| r.file.path == drill));
        let planned = fs::read(&drill).unwrap();
        let outside = fx._root.0.join("outside.md");
        fs::write(&outside, &planned).unwrap();
        fs::remove_file(&drill).unwrap();
        std::os::unix::fs::symlink(&outside, &drill).unwrap();

        let err = apply_scope(&mut plan, &fx.backups, "20260301T123045")
            .expect_err("a symlinked rewrite target is refused");

        assert!(
            format!("{err:#}").contains(&format!(
                "{} changed since the migration was planned, so it was not rewritten",
                drill.display()
            )),
            "{err:#}"
        );
        assert!(fs::symlink_metadata(&drill).unwrap().is_symlink());
        assert_eq!(fs::read(&outside).unwrap(), planned);
        assert!(plan.written.is_empty());
    }

    #[test]
    fn round_chains_point_at_the_highest_round_and_untyped_members_become_run_state() {
        let fx = Fixture::new("chains");

        let report = fx.migrate(true);

        let r1 = load(&fx.workspace, "plan-r1").frontmatter;
        assert_eq!(r1.superseded_by.as_deref(), Some("plan-r3"));
        assert_eq!(r1.kind.kind(), MemoryKind::RunState);
        assert_eq!(r1.expires, None);
        let r2 = load(&fx.workspace, "plan-r2").frontmatter;
        assert_eq!(r2.superseded_by.as_deref(), Some("plan-r3"));
        assert_eq!(r2.kind.kind(), MemoryKind::Project);
        let r3 = load(&fx.workspace, "plan-r3").frontmatter;
        assert_eq!(r3.superseded_by, None);
        assert_eq!(r3.kind.kind(), MemoryKind::RunState);
        let gotcha = fs::read_to_string(fx.workspace.join("gotcha.md")).unwrap();
        assert!(gotcha.contains("\ntype: gotcha\n"), "{gotcha}");

        let workspace = scope(&report, "workspace");
        assert_eq!(
            workspace.superseded_stamps,
            vec![
                ("plan-r1".to_string(), "plan-r3".to_string()),
                ("plan-r2".to_string(), "plan-r3".to_string()),
            ]
        );
        assert_eq!(workspace.run_state_stamps, vec!["plan-r1", "plan-r3"]);
    }

    #[test]
    fn keep_leaves_older_rounds_unmarked_while_supersede_marks_them() {
        let marks = |round_chains| {
            let fx = Fixture::new("chains_setting");
            let report = migrate(
                &fx.targets(MigrateScope::All),
                &fx.backups,
                true,
                round_chains,
                now(),
            )
            .unwrap();
            let on_disk: Vec<Option<String>> = ["plan-r1", "plan-r2"]
                .iter()
                .map(|name| load(&fx.workspace, name).frontmatter.superseded_by)
                .collect();
            let workspace = scope(&report, "workspace");
            (
                on_disk,
                workspace.superseded_stamps.len(),
                workspace.run_state_stamps.clone(),
            )
        };

        let head = Some("plan-r3".to_string());
        assert_eq!(
            marks(RoundChains::Supersede),
            (
                vec![head.clone(), head],
                2,
                vec!["plan-r1".to_string(), "plan-r3".to_string()]
            )
        );
        assert_eq!(
            marks(RoundChains::Keep),
            (
                vec![None, None],
                0,
                vec!["plan-r1".to_string(), "plan-r3".to_string()]
            )
        );
    }

    #[test]
    fn an_unreadable_newest_round_still_heads_its_chain() {
        let fx = Fixture::new("unreadable_head");
        fs::write(
            fx.workspace.join("plan-r4.md"),
            "---\nname: [unclosed\n---\n\nr4\n",
        )
        .unwrap();

        let report = fx.migrate(true);

        for name in ["plan-r1", "plan-r2", "plan-r3"] {
            assert_eq!(
                load(&fx.workspace, name)
                    .frontmatter
                    .superseded_by
                    .as_deref(),
                Some("plan-r4"),
                "{name}"
            );
        }
        let workspace = scope(&report, "workspace");
        assert_eq!(
            workspace.superseded_stamps,
            vec![
                ("plan-r1".to_string(), "plan-r4".to_string()),
                ("plan-r2".to_string(), "plan-r4".to_string()),
                ("plan-r3".to_string(), "plan-r4".to_string()),
            ]
        );
        assert!(
            workspace
                .unreadable
                .iter()
                .any(|line| line.contains("plan-r4.md")),
            "{:?}",
            workspace.unreadable
        );
        assert!(
            !workspace
                .missing_targets
                .iter()
                .any(|(_, target)| target == "plan-r4"),
            "{:?}",
            workspace.missing_targets
        );
    }

    #[test]
    fn a_directory_named_like_a_newer_round_does_not_head_its_chain() {
        let fx = Fixture::new("dir_round");
        fs::create_dir(fx.workspace.join("plan-r5.md")).unwrap();

        let report = fx.migrate(false);

        assert_eq!(
            scope(&report, "workspace").superseded_stamps,
            vec![
                ("plan-r1".to_string(), "plan-r3".to_string()),
                ("plan-r2".to_string(), "plan-r3".to_string()),
            ]
        );
    }

    #[test]
    fn a_round_whose_front_matter_names_another_memory_is_left_out_and_reported() {
        let fx = Fixture::new("misnamed_round");
        fs::write(
            fx.workspace.join("plan-r4.md"),
            "---\nname: elsewhere\ntype: project\n---\n\nr4\n",
        )
        .unwrap();

        let report = fx.migrate(false);

        let workspace = scope(&report, "workspace");
        assert_eq!(
            workspace.superseded_stamps,
            vec![
                ("plan-r1".to_string(), "plan-r3".to_string()),
                ("plan-r2".to_string(), "plan-r3".to_string()),
            ]
        );
        assert!(
            workspace.unreadable.contains(&format!(
                "{}: front matter name 'elsewhere' does not match the file name",
                fx.workspace.join("plan-r4.md").display()
            )),
            "{:?}",
            workspace.unreadable
        );
    }

    #[test]
    fn only_an_unreadable_drill_file_excuses_a_missing_supersede_target() {
        let fx = Fixture::new("missing_targets");
        fs::create_dir(fx.global.join("a-dir.md")).unwrap();
        fs::write(fx.global.join("a-broken.md"), "---\nname: [unclosed\n---\n").unwrap();
        for (name, target) in [
            ("to-dir", "a-dir"),
            ("to-broken", "a-broken"),
            ("to-sub", "sub/a-broken"),
            ("to-parent", "../a-broken"),
        ] {
            write_file(
                &fx.global,
                name,
                &format!("description: d\ntype: user\nsuperseded_by: {target}\n"),
                "x",
            );
        }

        let report = fx.migrate(false);

        assert_eq!(
            scope(&report, "global").missing_targets,
            vec![
                ("to-dir".to_string(), "a-dir".to_string()),
                ("to-parent".to_string(), "../a-broken".to_string()),
                ("to-sub".to_string(), "sub/a-broken".to_string()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_drill_file_does_not_excuse_a_missing_supersede_target() {
        let fx = Fixture::new("missing_symlink_target");
        let outside = fx._root.0.join("outside.md");
        fs::write(&outside, "---\nname: a-link\n---\n\nl\n").unwrap();
        std::os::unix::fs::symlink(&outside, fx.global.join("a-link.md")).unwrap();
        write_file(
            &fx.global,
            "to-link",
            "description: d\ntype: user\nsuperseded_by: a-link\n",
            "x",
        );

        let report = fx.migrate(false);

        assert_eq!(
            scope(&report, "global").missing_targets,
            vec![("to-link".to_string(), "a-link".to_string())]
        );
    }

    #[test]
    fn an_index_edited_during_apply_is_not_overwritten() {
        let fx = Fixture::new("index_mid_apply");
        let scopes = plan_all(
            &fx.targets(MigrateScope::Workspace),
            "2026-03-01",
            RoundChains::default(),
        )
        .unwrap();
        let Some(ScopeOutcome::Migrated(mut report)) = scopes.into_iter().next() else {
            panic!("the workspace scope plans");
        };
        assert!(report.new_index.is_some());
        let index_path = fx.workspace.join(MEMORY_INDEX_FILE_NAME);
        let edited = format!("{}- [[late]]: added meanwhile\n", workspace_index());

        let (path, text) = (index_path.clone(), edited.clone());
        let err = apply_scope_with(&mut report, &fx.backups, "20260301T123045", move || {
            fs::write(&path, &text).unwrap();
        })
        .expect_err("an index edited during apply is refused");

        assert!(
            format!("{err:#}").contains(&format!(
                "{} changed since the migration was planned, so it was not rewritten",
                index_path.display()
            )),
            "{err:#}"
        );
        assert_eq!(fs::read_to_string(&index_path).unwrap(), edited);
        assert!(!report.written.contains(&index_path));
        assert!(!report.written.is_empty());
    }

    #[test]
    fn an_empty_description_or_type_is_dropped_quietly() {
        let fx = Fixture::new("empty_fields");
        fs::write(
            fx.global.join(MEMORY_INDEX_FILE_NAME),
            format!("{GLOBAL_INDEX}- [[hollow]]: from the index\n- [[hollow-r2]]: newer\n"),
        )
        .unwrap();
        write_file(&fx.global, "hollow", "description: ''\ntype: ''\n", "h");
        write_file(
            &fx.global,
            "hollow-r1",
            "description: '  '\ntype: user\n",
            "1",
        );
        write_file(
            &fx.global,
            "hollow-r2",
            "description: newer\ntype: user\n",
            "2",
        );

        let report = fx.migrate(true);

        let global = scope(&report, "global");
        assert!(
            global.unknown_types.is_empty(),
            "{:?}",
            global.unknown_types
        );
        assert!(global.long_descriptions.is_empty());
        assert!(global.drift.is_empty());
        assert!(global.run_state_stamps.is_empty());
        assert_eq!(global.backfilled, vec!["hollow"]);
        assert!(!report.to_string().contains("''"));
        let hollow = fs::read_to_string(fx.global.join("hollow.md")).unwrap();
        assert!(
            hollow.contains("\ndescription: from the index\n"),
            "{hollow}"
        );
        assert!(!hollow.contains("type:"), "{hollow}");
        let older = fs::read_to_string(fx.global.join("hollow-r1.md")).unwrap();
        assert!(older.contains("\nsuperseded_by: hollow-r2\n"), "{older}");
        assert!(!older.contains("description:"), "{older}");
    }

    #[test]
    fn report_only_sections_name_the_files_left_for_the_judgement_pass() {
        let fx = Fixture::new("sections");

        let report = fx.migrate(false);

        let workspace = scope(&report, "workspace");
        assert_eq!(
            workspace.oversized,
            vec![("huge".to_string(), MEMORY_FILE_HARD_CAP + 2)]
        );
        assert_eq!(
            workspace.long_descriptions,
            vec![("wordy".to_string(), 161)]
        );
        assert_eq!(
            workspace.unknown_types,
            vec![("gotcha".to_string(), "gotcha".to_string())]
        );
        assert_eq!(
            workspace.expired,
            vec![("bygone".to_string(), "2000-01-01".to_string())]
        );
        assert_eq!(
            workspace.missing_targets,
            vec![("bygone".to_string(), "nowhere".to_string())]
        );
        assert_eq!(workspace.orphans, vec!["bygone", "orphan", "wordy"]);
        assert!(
            report
                .to_string()
                .contains("  orphans (already searchable): 3\n    bygone\n    orphan\n    wordy\n")
        );
    }

    #[test]
    fn all_iterates_both_scopes_and_a_missing_dir_is_skipped() {
        let fx = Fixture::new("scopes");

        let labels = |which| {
            fx.targets(which)
                .iter()
                .map(|t| t.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(MigrateScope::All), vec!["global", "workspace"]);
        assert_eq!(labels(MigrateScope::Global), vec!["global"]);
        assert_eq!(labels(MigrateScope::Workspace), vec!["workspace"]);
        let report = fx.migrate(false);
        assert_eq!(report.scopes.len(), 2);
        scope(&report, "global");
        scope(&report, "workspace");

        let nowhere = fx.workspace_root.parent().unwrap().join("elsewhere");
        let missing = vec![
            Target {
                label: "global",
                dir: Some(nowhere.join("memory")),
            },
            Target {
                label: "workspace",
                dir: None,
            },
        ];
        let text = migrate(&missing, &fx.backups, true, RoundChains::default(), now())
            .unwrap()
            .to_string();
        assert_eq!(
            text,
            format!(
                "global: skipped, no memory dir at {}\n\
                 workspace: skipped, no workspace memory found from the current directory\n\
                 next: review the remaining sections by hand; migrate leaves them as they are\n",
                nowhere.join("memory").display()
            )
        );
        assert!(!fx.backups.exists());
    }

    #[test]
    fn a_backup_in_the_same_second_gets_its_own_dir() {
        let root = TempDir::new("backup_name");
        let first = unused_backup_dir(&root.0, "global", "20260301T123045");
        fs::create_dir_all(&first).unwrap();

        assert_eq!(
            unused_backup_dir(&root.0, "global", "20260301T123045"),
            root.0.join("global-20260301T123045-2")
        );
    }

    #[test]
    fn the_round_chains_setting_is_read_past_other_invalid_keys() {
        let root = TempDir::new("round_chains_setting");
        let config = root.0.join("config.yaml");
        assert_eq!(round_chains_in(&config).unwrap(), RoundChains::Supersede);

        fs::write(&config, "").unwrap();
        assert_eq!(round_chains_in(&config).unwrap(), RoundChains::Supersede);

        fs::write(&config, "model: [not, a, string]\n").unwrap();
        assert_eq!(round_chains_in(&config).unwrap(), RoundChains::Supersede);

        fs::write(
            &config,
            "model: [not, a, string]\nmemory_round_chains: keep\n",
        )
        .unwrap();
        assert_eq!(round_chains_in(&config).unwrap(), RoundChains::Keep);

        for broken in ["memory_round_chains: bogus\n", "model: [unclosed\n"] {
            fs::write(&config, broken).unwrap();
            let err = format!("{:#}", round_chains_in(&config).unwrap_err());
            assert!(
                err.starts_with(&format!(
                    "read memory_round_chains from {}",
                    config.display()
                )),
                "{err}"
            );
        }
    }
}
