use super::install_remote::{
    canonical_source_url, owner_qualifier, repo_name_slug, validate_bundle_name,
};
use super::paths;
use crate::function::write_file_atomic;

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FileAction {
    New,
    Replaced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpAction {
    Added,
    Replaced,
    Renamed,
    Transferred,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileRecord {
    /// Relative to the config dir.
    pub(crate) path: String,
    pub(crate) category: String,
    /// Content hash at install time; a later mismatch means the user modified the file.
    pub(crate) sha256: String,
    pub(crate) action: FileAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct McpServerRecord {
    pub(crate) name: String,
    pub(crate) action: McpAction,
    pub(crate) renamed_to: Option<String>,
    /// Hash of the mcp.json entry as written; a later mismatch means the user
    /// modified it. Absent on records made before entry hashing existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
}

impl McpServerRecord {
    /// The key this entry actually occupies in mcp.json (the rename target, if any).
    pub(crate) fn effective_key(&self) -> &str {
        self.renamed_to.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct BundleRecord {
    pub(crate) source: String,
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub(crate) git_ref: Option<String>,
    pub(crate) commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) homepage: Option<String>,
    pub(crate) installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated_at: Option<String>,
    #[serde(default)]
    pub(crate) files: Vec<FileRecord>,
    #[serde(default)]
    pub(crate) mcp_servers: Vec<McpServerRecord>,
}

#[derive(Debug, Clone)]
pub(crate) struct InstallMetadata {
    pub(crate) source: String,
    pub(crate) git_ref: Option<String>,
    pub(crate) commit: String,
    pub(crate) version: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) homepage: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedBundleName {
    pub(crate) name: String,
    /// The unqualified name this install asked for, when it had to be owner-qualified.
    pub(crate) qualified_from: Option<String>,
    /// The record key this source was previously tracked under, when it changed.
    #[allow(dead_code)]
    pub(crate) migrated_from: Option<String>,
    /// Source URL of the different-source bundle that already holds the unqualified name.
    pub(crate) same_name_other_source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StoreContents {
    #[serde(default)]
    bundles: BTreeMap<String, BundleRecord>,
}

#[derive(Serialize)]
struct StoreContentsRef<'a> {
    bundles: &'a BTreeMap<String, BundleRecord>,
}

#[derive(Debug)]
pub(crate) struct BundleStore {
    path: PathBuf,
    bundles: BTreeMap<String, BundleRecord>,
}

impl BundleStore {
    pub(crate) fn load() -> Result<Self> {
        Self::load_from(paths::installed_bundles_file())
    }

    /// A corrupt store is an error, never an empty store: treating it as empty
    /// would let a reinstall re-acquire ownership over files the user may have
    /// modified since.
    pub(crate) fn load_from(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path,
                bundles: BTreeMap::new(),
            });
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let contents: StoreContents = serde_yaml::from_str(&content).with_context(|| {
            format!(
                "failed to parse {}; refusing to treat it as empty. \
                 Fix or remove the file to continue",
                path.display()
            )
        })?;
        Ok(Self {
            path,
            bundles: contents.bundles,
        })
    }

    pub(crate) fn save(&self) -> Result<()> {
        let content = serde_yaml::to_string(&StoreContentsRef {
            bundles: &self.bundles,
        })
        .context("failed to serialize the installed-bundles store")?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        write_file_atomic(&self.path, &content, None)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    pub(crate) fn get(&self, name: &str) -> Option<&BundleRecord> {
        self.bundles.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &BundleRecord)> {
        self.bundles
            .iter()
            .map(|(name, record)| (name.as_str(), record))
    }

    pub(crate) fn bundle_names(&self) -> Vec<&str> {
        self.bundles.keys().map(String::as_str).collect()
    }

    /// Look up the record installed from `url`, comparing canonical source URLs
    /// so https/scp/`.git` spellings of the same remote all match.
    pub(crate) fn find_by_source(&self, url: &str) -> Option<(&str, &BundleRecord)> {
        let canonical = canonical_source_url(url);
        self.bundles
            .iter()
            .find(|(_, record)| canonical_source_url(&record.source) == canonical)
            .map(|(name, record)| (name.as_str(), record))
    }

    /// Decide the record key for an install from `url`, matching by canonical
    /// source URL first and name second. If the URL is already tracked under a
    /// different key (manifest name added, renamed, or removed since install),
    /// the existing record is migrated to the new key — the same URL never gets
    /// a second record. A name held by a different-source bundle is
    /// owner-qualified instead. Both cases print a notice.
    pub(crate) fn resolve_bundle_name(
        &mut self,
        url: &str,
        manifest_name: Option<&str>,
    ) -> Result<ResolvedBundleName> {
        let canonical = canonical_source_url(url);
        let existing_key = self
            .bundles
            .iter()
            .find(|(_, record)| canonical_source_url(&record.source) == canonical)
            .map(|(name, _)| name.clone());

        let base = match manifest_name {
            Some(name) => {
                validate_bundle_name(name)?;
                name.to_string()
            }
            None => {
                let slug = sanitize_name_segment(&repo_name_slug(url));
                if slug.is_empty() {
                    bail!(
                        "cannot derive a bundle name from '{url}'; \
                         add a coyote-bundle.yaml manifest with a name"
                    );
                }
                slug
            }
        };

        let mut resolved = ResolvedBundleName {
            name: base.clone(),
            qualified_from: None,
            migrated_from: None,
            same_name_other_source: None,
        };

        if let Some(other_source) = self.source_of_other_bundle(&base, &canonical) {
            if base.contains('/') {
                bail!(
                    "bundle name '{base}' is already used by an install from \
                     '{other_source}' and cannot be qualified further; \
                     uninstall it or pick a different manifest name"
                );
            }
            let owner = owner_qualifier(url)
                .map(|owner| sanitize_name_segment(&owner))
                .filter(|owner| !owner.is_empty());
            let Some(owner) = owner else {
                bail!(
                    "bundle name '{base}' is already used by an install from \
                     '{other_source}', and no owner qualifier can be derived from '{url}'"
                );
            };
            let qualified = format!("{owner}/{base}");
            if let Some(source) = self.source_of_other_bundle(&qualified, &canonical) {
                bail!(
                    "bundle names '{base}' and '{qualified}' are both used by installs \
                     from other sources ('{other_source}', '{source}'); \
                     uninstall one or pick a different manifest name"
                );
            }
            resolved.name = qualified;
            resolved.qualified_from = Some(base);
            resolved.same_name_other_source = Some(other_source);
        }

        let already_recorded = existing_key.as_deref() == Some(resolved.name.as_str());
        if let Some(old_key) = existing_key
            && !already_recorded
        {
            let record = self
                .bundles
                .remove(&old_key)
                .expect("existing_key was found in the map");
            self.bundles.insert(resolved.name.clone(), record);
            println!(
                "Bundle '{old_key}' from {url} is now tracked as '{}'.",
                resolved.name
            );
            resolved.migrated_from = Some(old_key);
            self.save()?;
        }

        if let (Some(from), false) = (&resolved.qualified_from, already_recorded) {
            let other = resolved
                .same_name_other_source
                .as_deref()
                .unwrap_or_default();
            println!(
                "Bundle name '{from}' is already used by an install from '{other}'; \
                 tracking this install as '{}'.",
                resolved.name
            );
        }

        Ok(resolved)
    }

    /// Create or update the record's metadata and persist it. Repeated installs
    /// of the same bundle (e.g. with different filters) merge into one record:
    /// metadata is refreshed, files accumulate, and the original install time
    /// is kept.
    pub(crate) fn upsert_bundle(&mut self, name: &str, metadata: InstallMetadata) -> Result<()> {
        match self.bundles.get_mut(name) {
            Some(record) => {
                record.source = metadata.source;
                record.git_ref = metadata.git_ref;
                record.commit = metadata.commit;
                record.version = metadata.version;
                record.description = metadata.description;
                record.homepage = metadata.homepage;
            }
            None => {
                self.bundles.insert(
                    name.to_string(),
                    BundleRecord {
                        source: metadata.source,
                        git_ref: metadata.git_ref,
                        commit: metadata.commit,
                        version: metadata.version,
                        description: metadata.description,
                        homepage: metadata.homepage,
                        installed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                        updated_at: None,
                        files: Vec::new(),
                        mcp_servers: Vec::new(),
                    },
                );
            }
        }
        self.save()
    }

    /// Stamp the record with the time of its most recent update from source.
    pub(crate) fn mark_updated(&mut self, name: &str) -> Result<()> {
        self.ensure_bundle_exists(name)?;
        let record = self
            .bundles
            .get_mut(name)
            .expect("bundle existence checked above");
        record.updated_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
        self.save()
    }

    /// Drop one path from a bundle's owned files and persist. Used when the
    /// bundle no longer ships the file and it is gone (or deleted) locally.
    pub(crate) fn remove_file_record(&mut self, bundle: &str, path: &str) -> Result<()> {
        self.ensure_bundle_exists(bundle)?;
        let record = self
            .bundles
            .get_mut(bundle)
            .expect("bundle existence checked above");
        record.files.retain(|owned| owned.path != path);
        self.save()
    }

    /// Drop one mcp.json entry from a bundle's owned servers, matched by the
    /// key it occupies in mcp.json, and persist.
    pub(crate) fn remove_mcp_record(&mut self, bundle: &str, effective_key: &str) -> Result<()> {
        self.ensure_bundle_exists(bundle)?;
        let record = self
            .bundles
            .get_mut(bundle)
            .expect("bundle existence checked above");
        record
            .mcp_servers
            .retain(|owned| owned.effective_key() != effective_key);
        self.save()
    }

    /// Remove a bundle's record entirely and persist. Used once an uninstall
    /// has released everything the record owned.
    pub(crate) fn remove_bundle(&mut self, name: &str) -> Result<()> {
        self.ensure_bundle_exists(name)?;
        self.bundles.remove(name);
        self.save()
    }

    /// Record one written file and persist immediately, so an install aborted
    /// partway through still has provenance for everything already on disk.
    /// A path owned by another bundle transfers to `bundle`.
    pub(crate) fn record_file(&mut self, bundle: &str, file: FileRecord) -> Result<()> {
        self.ensure_bundle_exists(bundle)?;
        for (name, record) in self.bundles.iter_mut() {
            if name != bundle {
                record.files.retain(|owned| owned.path != file.path);
            }
        }
        let record = self
            .bundles
            .get_mut(bundle)
            .expect("bundle existence checked above");
        record.files.retain(|owned| owned.path != file.path);
        record.files.push(file);
        self.save()
    }

    /// Record the mcp.json entries an install wrote, in one persisted flush.
    /// An entry whose key any bundle already owns — including `bundle` itself
    /// on an update — transfers to `bundle`: the old owner drops it, and a
    /// `replaced` action is upgraded to `transferred` (removable at uninstall
    /// — plain `replaced` marks a pre-existing user entry that uninstall must
    /// never delete).
    pub(crate) fn record_mcp_servers(
        &mut self,
        bundle: &str,
        entries: Vec<McpServerRecord>,
    ) -> Result<()> {
        self.ensure_bundle_exists(bundle)?;
        for mut entry in entries {
            let key = entry.effective_key().to_string();
            let mut previously_owned = false;
            for record in self.bundles.values_mut() {
                let before = record.mcp_servers.len();
                record
                    .mcp_servers
                    .retain(|owned| owned.effective_key() != key);
                previously_owned |= record.mcp_servers.len() != before;
            }
            if previously_owned && entry.action == McpAction::Replaced {
                entry.action = McpAction::Transferred;
            }
            self.bundles
                .get_mut(bundle)
                .expect("bundle existence checked above")
                .mcp_servers
                .push(entry);
        }
        self.save()
    }

    fn ensure_bundle_exists(&self, bundle: &str) -> Result<()> {
        if !self.bundles.contains_key(bundle) {
            bail!(
                "no installed bundle named '{bundle}' (installed: {})",
                if self.bundles.is_empty() {
                    "none".to_string()
                } else {
                    self.bundle_names().join(", ")
                }
            );
        }
        Ok(())
    }

    fn source_of_other_bundle(&self, name: &str, canonical: &str) -> Option<String> {
        self.bundles
            .get(name)
            .filter(|record| canonical_source_url(&record.source) != canonical)
            .map(|record| record.source.clone())
    }
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn hash_file(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read {} for hashing", path.display()))?;
    Ok(hash_bytes(&bytes))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DriftSummary {
    pub(crate) intact: usize,
    pub(crate) modified: usize,
    pub(crate) missing: usize,
}

impl DriftSummary {
    pub(crate) fn display(&self) -> String {
        if self.intact + self.modified + self.missing == 0 {
            return "-".to_string();
        }
        let mut parts = Vec::new();
        if self.intact > 0 {
            parts.push(format!("{} intact", self.intact));
        }
        if self.modified > 0 {
            parts.push(format!("{} modified locally", self.modified));
        }
        if self.missing > 0 {
            parts.push(format!("{} missing", self.missing));
        }
        parts.join(", ")
    }
}

#[derive(Debug)]
pub(crate) struct BundleListRow {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) source: String,
    pub(crate) git_ref: String,
    pub(crate) installed_at: String,
    pub(crate) file_counts: String,
    pub(crate) drift: DriftSummary,
}

/// Build one listing row per installed bundle, hashing each owned file under
/// `config_dir` against its recorded checksum: a match is intact, a mismatch
/// (or unreadable file) counts as locally modified, and an absent file is
/// missing. Read-only: the store is never mutated by listing.
pub(crate) fn bundle_list_rows(store: &BundleStore, config_dir: &Path) -> Vec<BundleListRow> {
    store
        .iter()
        .map(|(name, record)| {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            let mut drift = DriftSummary::default();
            for file in &record.files {
                *counts.entry(file.category.as_str()).or_default() += 1;
                let path = config_dir.join(&file.path);
                if !path.exists() {
                    drift.missing += 1;
                } else {
                    match hash_file(&path) {
                        Ok(hash) if hash == file.sha256 => drift.intact += 1,
                        _ => drift.modified += 1,
                    }
                }
            }
            let file_counts = if counts.is_empty() {
                "-".to_string()
            } else {
                counts
                    .iter()
                    .map(|(category, count)| format!("{category}: {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            BundleListRow {
                name: name.to_string(),
                version: record
                    .version
                    .clone()
                    .unwrap_or_else(|| record.commit.chars().take(7).collect()),
                source: record.source.clone(),
                git_ref: record.git_ref.clone().unwrap_or_else(|| "-".to_string()),
                installed_at: record.installed_at.clone(),
                file_counts,
                drift,
            }
        })
        .collect()
}

pub fn list_installed_bundles() -> Result<()> {
    let store = BundleStore::load()?;
    let rows = bundle_list_rows(&store, &paths::config_dir());
    if rows.is_empty() {
        println!("No bundles installed. Install one with `coyote --install <git-url>`.");
        return Ok(());
    }

    let mut table = super::request_context::asset_table(&[
        "name",
        "version",
        "source",
        "ref",
        "installed",
        "files",
        "drift",
    ]);
    for row in rows {
        table.add_row(vec![
            row.name.as_str(),
            &row.version,
            &row.source,
            &row.git_ref,
            &row.installed_at,
            &row.file_counts,
            &row.drift.display(),
        ]);
    }

    println!("Bundles:");
    println!("{table}");
    Ok(())
}

fn sanitize_name_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::{get_env_name, temp_file};
    use serial_test::serial;
    use std::env;
    use std::ffi::OsString;

    struct TempStoreDir(PathBuf);

    impl TempStoreDir {
        fn new(label: &str) -> Self {
            let dir = temp_file(label, "");
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn store_path(&self) -> PathBuf {
            self.0.join("installed-bundles.yaml")
        }

        fn store(&self) -> BundleStore {
            BundleStore::load_from(self.store_path()).unwrap()
        }
    }

    impl Drop for TempStoreDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn metadata(source: &str, commit: &str) -> InstallMetadata {
        InstallMetadata {
            source: source.to_string(),
            git_ref: None,
            commit: commit.to_string(),
            version: None,
            description: None,
            homepage: None,
        }
    }

    fn file_record(path: &str, contents: &str) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            category: "macros".to_string(),
            sha256: hash_bytes(contents.as_bytes()),
            action: FileAction::New,
        }
    }

    fn mcp_record(name: &str, action: McpAction, renamed_to: Option<&str>) -> McpServerRecord {
        McpServerRecord {
            name: name.to_string(),
            action,
            renamed_to: renamed_to.map(str::to_string),
            sha256: None,
        }
    }

    #[test]
    fn load_missing_file_yields_empty_store() {
        let dir = TempStoreDir::new("bundles-empty");

        let store = dir.store();

        assert!(store.bundle_names().is_empty());
    }

    #[test]
    fn corrupt_store_fails_closed() {
        let dir = TempStoreDir::new("bundles-corrupt");
        fs::write(dir.store_path(), "bundles:\n  - this is not a map\n").unwrap();

        let result = BundleStore::load_from(dir.store_path());

        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("refusing to treat it as empty"),
            "{message}"
        );
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = TempStoreDir::new("bundles-roundtrip");
        let mut store = dir.store();
        store
            .upsert_bundle(
                "omc",
                InstallMetadata {
                    source: "https://github.com/x/omc".to_string(),
                    git_ref: Some("main".to_string()),
                    commit: "abc123".to_string(),
                    version: Some("1.4.0".to_string()),
                    description: Some("Opinionated roles and macros".to_string()),
                    homepage: Some("https://github.com/x/omc".to_string()),
                },
            )
            .unwrap();
        store
            .record_file("omc", file_record("macros/a.yaml", "a"))
            .unwrap();
        store
            .record_mcp_servers("omc", vec![mcp_record("srv", McpAction::Added, None)])
            .unwrap();

        let raw = fs::read_to_string(dir.store_path()).unwrap();
        let reloaded = dir.store();

        assert!(raw.contains("ref: main"), "{raw}");
        assert!(!raw.contains("git_ref"), "{raw}");
        assert!(raw.contains("action: added"), "{raw}");
        let record = reloaded.get("omc").unwrap();
        assert_eq!(record.git_ref.as_deref(), Some("main"));
        assert_eq!(record.version.as_deref(), Some("1.4.0"));
        assert_eq!(
            record.description.as_deref(),
            Some("Opinionated roles and macros")
        );
        assert_eq!(record.homepage.as_deref(), Some("https://github.com/x/omc"));
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.mcp_servers.len(), 1);
    }

    #[test]
    fn files_recorded_before_an_abort_survive_it() {
        let dir = TempStoreDir::new("bundles-abort");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();

        store
            .record_file("omc", file_record("macros/a.yaml", "a"))
            .unwrap();
        store
            .record_file("omc", file_record("skills/b.md", "b"))
            .unwrap();
        drop(store);

        let reloaded = dir.store();
        let paths: Vec<&str> = reloaded
            .get("omc")
            .unwrap()
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(paths, vec!["macros/a.yaml", "skills/b.md"]);
    }

    #[test]
    fn recording_the_same_path_twice_updates_in_place() {
        let dir = TempStoreDir::new("bundles-dedupe");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();

        store
            .record_file("omc", file_record("macros/a.yaml", "v1"))
            .unwrap();
        let mut updated = file_record("macros/a.yaml", "v2");
        updated.action = FileAction::Replaced;
        store.record_file("omc", updated).unwrap();

        let record = store.get("omc").unwrap();
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.files[0].sha256, hash_bytes(b"v2"));
        assert_eq!(record.files[0].action, FileAction::Replaced);
    }

    #[test]
    fn overwritten_file_transfers_ownership() {
        let dir = TempStoreDir::new("bundles-transfer");
        let mut store = dir.store();
        store
            .upsert_bundle("alpha", metadata("https://github.com/a/alpha", "abc123"))
            .unwrap();
        store
            .upsert_bundle("beta", metadata("https://github.com/b/beta", "def456"))
            .unwrap();
        store
            .record_file("alpha", file_record("macros/shared.yaml", "a"))
            .unwrap();

        let mut taken = file_record("macros/shared.yaml", "b");
        taken.action = FileAction::Replaced;
        store.record_file("beta", taken).unwrap();

        let reloaded = dir.store();
        assert!(reloaded.get("alpha").unwrap().files.is_empty());
        let beta_files = &reloaded.get("beta").unwrap().files;
        assert_eq!(beta_files.len(), 1);
        assert_eq!(beta_files[0].path, "macros/shared.yaml");
    }

    #[test]
    fn overwritten_mcp_entry_transfers_ownership_as_transferred() {
        let dir = TempStoreDir::new("bundles-mcp-transfer");
        let mut store = dir.store();
        store
            .upsert_bundle("alpha", metadata("https://github.com/a/alpha", "abc123"))
            .unwrap();
        store
            .upsert_bundle("beta", metadata("https://github.com/b/beta", "def456"))
            .unwrap();
        store
            .record_mcp_servers("alpha", vec![mcp_record("srv", McpAction::Added, None)])
            .unwrap();

        store
            .record_mcp_servers("beta", vec![mcp_record("srv", McpAction::Replaced, None)])
            .unwrap();

        let reloaded = dir.store();
        assert!(reloaded.get("alpha").unwrap().mcp_servers.is_empty());
        let beta_servers = &reloaded.get("beta").unwrap().mcp_servers;
        assert_eq!(beta_servers.len(), 1);
        assert_eq!(beta_servers[0].action, McpAction::Transferred);
    }

    #[test]
    fn mcp_replacement_of_unowned_entry_stays_replaced() {
        let dir = TempStoreDir::new("bundles-mcp-replaced");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();

        store
            .record_mcp_servers(
                "omc",
                vec![mcp_record("user-srv", McpAction::Replaced, None)],
            )
            .unwrap();

        assert_eq!(
            store.get("omc").unwrap().mcp_servers[0].action,
            McpAction::Replaced
        );
    }

    #[test]
    fn mcp_rerecord_of_own_entry_upgrades_replaced_to_transferred() {
        let dir = TempStoreDir::new("bundles-mcp-self-update");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        store
            .record_mcp_servers("omc", vec![mcp_record("srv", McpAction::Added, None)])
            .unwrap();

        let mut updated = mcp_record("srv", McpAction::Replaced, None);
        updated.sha256 = Some("deadbeef".to_string());
        store.record_mcp_servers("omc", vec![updated]).unwrap();

        let servers = &store.get("omc").unwrap().mcp_servers;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].action, McpAction::Transferred);
        assert_eq!(servers[0].sha256.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn mcp_transfer_matches_renamed_entries_by_effective_key() {
        let dir = TempStoreDir::new("bundles-mcp-renamed");
        let mut store = dir.store();
        store
            .upsert_bundle("alpha", metadata("https://github.com/a/alpha", "abc123"))
            .unwrap();
        store
            .upsert_bundle("beta", metadata("https://github.com/b/beta", "def456"))
            .unwrap();
        store
            .record_mcp_servers(
                "alpha",
                vec![mcp_record("srv", McpAction::Renamed, Some("srv-remote"))],
            )
            .unwrap();

        store
            .record_mcp_servers(
                "beta",
                vec![mcp_record("srv-remote", McpAction::Replaced, None)],
            )
            .unwrap();

        let reloaded = dir.store();
        assert!(reloaded.get("alpha").unwrap().mcp_servers.is_empty());
        assert_eq!(
            reloaded.get("beta").unwrap().mcp_servers[0].action,
            McpAction::Transferred
        );
    }

    #[test]
    fn resolve_same_url_same_name_is_an_update() {
        let dir = TempStoreDir::new("bundles-resolve-update");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();

        let resolved = store
            .resolve_bundle_name("git@github.com:X/omc.git", None)
            .unwrap();

        assert_eq!(resolved.name, "omc");
        assert_eq!(resolved.migrated_from, None);
        assert_eq!(resolved.qualified_from, None);
    }

    #[test]
    fn resolve_migrates_record_when_identity_changes() {
        let dir = TempStoreDir::new("bundles-migrate");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        store
            .record_file("omc", file_record("macros/a.yaml", "a"))
            .unwrap();

        let resolved = store
            .resolve_bundle_name("git@github.com:x/omc.git", Some("oh-my-coyote"))
            .unwrap();

        assert_eq!(resolved.name, "oh-my-coyote");
        assert_eq!(resolved.migrated_from.as_deref(), Some("omc"));
        let reloaded = dir.store();
        assert!(reloaded.get("omc").is_none());
        assert_eq!(reloaded.get("oh-my-coyote").unwrap().files.len(), 1);
    }

    #[test]
    fn resolve_qualifies_colliding_name_from_https_source() {
        let dir = TempStoreDir::new("bundles-qualify-https");
        let mut store = dir.store();
        store
            .upsert_bundle("repo", metadata("https://github.com/a/repo", "abc123"))
            .unwrap();

        let resolved = store
            .resolve_bundle_name("https://gitlab.com/b/repo.git", None)
            .unwrap();

        assert_eq!(resolved.name, "b/repo");
        assert_eq!(resolved.qualified_from.as_deref(), Some("repo"));
        assert_eq!(
            resolved.same_name_other_source.as_deref(),
            Some("https://github.com/a/repo")
        );
    }

    #[test]
    fn resolve_qualifies_colliding_name_from_scp_source() {
        let dir = TempStoreDir::new("bundles-qualify-scp");
        let mut store = dir.store();
        store
            .upsert_bundle("repo", metadata("https://github.com/a/repo", "abc123"))
            .unwrap();

        let resolved = store
            .resolve_bundle_name("git@bitbucket.org:c/repo.git", None)
            .unwrap();

        assert_eq!(resolved.name, "c/repo");
        assert_eq!(resolved.qualified_from.as_deref(), Some("repo"));
    }

    #[test]
    fn resolve_of_already_qualified_record_is_stable() {
        let dir = TempStoreDir::new("bundles-qualify-stable");
        let mut store = dir.store();
        store
            .upsert_bundle("repo", metadata("https://github.com/a/repo", "abc123"))
            .unwrap();
        store
            .upsert_bundle("b/repo", metadata("https://gitlab.com/b/repo", "def456"))
            .unwrap();

        let resolved = store
            .resolve_bundle_name("https://gitlab.com/b/repo.git", None)
            .unwrap();

        assert_eq!(resolved.name, "b/repo");
        assert_eq!(resolved.migrated_from, None);
    }

    #[test]
    fn resolve_sanitizes_derived_names() {
        let dir = TempStoreDir::new("bundles-sanitize");
        let mut store = dir.store();

        let resolved = store
            .resolve_bundle_name("https://github.com/vercel/next.js.git", None)
            .unwrap();

        assert_eq!(resolved.name, "next-js");
    }

    #[test]
    fn resolve_rejects_invalid_manifest_names() {
        let dir = TempStoreDir::new("bundles-invalid-name");
        let mut store = dir.store();

        let result = store.resolve_bundle_name("https://github.com/x/repo", Some("a/b/c"));

        assert!(result.is_err());
    }

    #[test]
    fn upsert_merges_metadata_and_preserves_files_and_install_time() {
        let dir = TempStoreDir::new("bundles-merge");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        store
            .record_file("omc", file_record("macros/a.yaml", "a"))
            .unwrap();
        let installed_at = store.get("omc").unwrap().installed_at.clone();

        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc.git", "def456"))
            .unwrap();

        let record = store.get("omc").unwrap();
        assert_eq!(record.commit, "def456");
        assert_eq!(record.source, "https://github.com/x/omc.git");
        assert_eq!(record.installed_at, installed_at);
        assert_eq!(record.files.len(), 1);
    }

    #[test]
    fn recording_against_unknown_bundle_fails() {
        let dir = TempStoreDir::new("bundles-unknown");
        let mut store = dir.store();

        let result = store.record_file("ghost", file_record("macros/a.yaml", "a"));

        assert!(result.unwrap_err().to_string().contains("ghost"));
    }

    #[test]
    fn mark_updated_stamps_and_round_trips() {
        let dir = TempStoreDir::new("bundles-mark-updated");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        let raw_before = fs::read_to_string(dir.store_path()).unwrap();
        assert!(!raw_before.contains("updated_at"), "{raw_before}");
        assert_eq!(store.get("omc").unwrap().updated_at, None);

        store.mark_updated("omc").unwrap();

        let raw_after = fs::read_to_string(dir.store_path()).unwrap();
        assert!(raw_after.contains("updated_at"), "{raw_after}");
        let reloaded = dir.store();
        assert!(reloaded.get("omc").unwrap().updated_at.is_some());
    }

    #[test]
    fn mark_updated_unknown_bundle_fails() {
        let dir = TempStoreDir::new("bundles-mark-unknown");
        let mut store = dir.store();

        let result = store.mark_updated("ghost");

        assert!(result.unwrap_err().to_string().contains("ghost"));
    }

    #[test]
    fn remove_file_record_drops_only_the_named_path() {
        let dir = TempStoreDir::new("bundles-remove-file");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        store
            .record_file("omc", file_record("macros/a.yaml", "a"))
            .unwrap();
        store
            .record_file("omc", file_record("macros/b.yaml", "b"))
            .unwrap();

        store.remove_file_record("omc", "macros/a.yaml").unwrap();

        let reloaded = dir.store();
        let paths: Vec<&str> = reloaded
            .get("omc")
            .unwrap()
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(paths, vec!["macros/b.yaml"]);
    }

    #[test]
    fn remove_file_record_unknown_bundle_fails() {
        let dir = TempStoreDir::new("bundles-remove-unknown");
        let mut store = dir.store();

        let result = store.remove_file_record("ghost", "macros/a.yaml");

        assert!(result.unwrap_err().to_string().contains("ghost"));
    }

    #[test]
    fn remove_mcp_record_drops_only_the_named_effective_key() {
        let dir = TempStoreDir::new("bundles-remove-mcp");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        store
            .record_mcp_servers(
                "omc",
                vec![
                    mcp_record("srv", McpAction::Renamed, Some("srv-remote")),
                    mcp_record("other", McpAction::Added, None),
                ],
            )
            .unwrap();

        store.remove_mcp_record("omc", "srv-remote").unwrap();

        let reloaded = dir.store();
        let keys: Vec<&str> = reloaded
            .get("omc")
            .unwrap()
            .mcp_servers
            .iter()
            .map(|s| s.effective_key())
            .collect();
        assert_eq!(keys, vec!["other"]);
    }

    #[test]
    fn remove_bundle_deletes_the_record_and_persists() {
        let dir = TempStoreDir::new("bundles-remove-bundle");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();

        store.remove_bundle("omc").unwrap();

        let reloaded = dir.store();
        assert!(reloaded.get("omc").is_none());
        assert!(reloaded.bundle_names().is_empty());
    }

    #[test]
    fn remove_bundle_unknown_name_fails() {
        let dir = TempStoreDir::new("bundles-remove-bundle-unknown");
        let mut store = dir.store();

        let result = store.remove_bundle("ghost");

        assert!(result.unwrap_err().to_string().contains("ghost"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_save_preserves_the_existing_store() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempStoreDir::new("bundles-atomic");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();
        let before = fs::read_to_string(dir.store_path()).unwrap();

        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = dir.0.join(".write-probe");
        if fs::write(&probe, "x").is_ok() {
            // A privileged user bypasses permission bits; the failure path
            // cannot be provoked this way.
            let _ = fs::remove_file(&probe);
            fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let result = store.record_file("omc", file_record("macros/a.yaml", "a"));
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(dir.store_path()).unwrap(), before);
        let reloaded = dir.store();
        assert!(reloaded.get("omc").unwrap().files.is_empty());
    }

    #[test]
    fn hash_helpers_are_stable() {
        let dir = TempStoreDir::new("bundles-hash");
        let path = dir.0.join("artifact.yaml");
        fs::write(&path, "hello").unwrap();

        assert_eq!(
            hash_bytes(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(hash_file(&path).unwrap(), hash_bytes(b"hello"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"hello!"));
    }

    #[test]
    #[serial]
    fn default_store_path_follows_the_config_dir() {
        let dir = TempStoreDir::new("bundles-env");
        let key = get_env_name("config_dir");
        let previous: Option<OsString> = env::var_os(&key);
        unsafe {
            env::set_var(&key, &dir.0);
        }

        let result = (|| -> Result<()> {
            let mut store = BundleStore::load()?;
            assert!(store.bundle_names().is_empty());
            store.upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))?;
            let reloaded = BundleStore::load()?;
            assert!(reloaded.get("omc").is_some());
            assert!(dir.store_path().is_file());
            Ok(())
        })();

        unsafe {
            match &previous {
                Some(value) => env::set_var(&key, value),
                None => env::remove_var(&key),
            }
        }
        result.unwrap();
    }

    #[test]
    fn bundle_rows_classify_drift_per_file() {
        let dir = TempStoreDir::new("bundles-list-drift");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123"))
            .unwrap();

        fs::create_dir_all(dir.0.join("macros")).unwrap();
        fs::write(dir.0.join("macros/intact.yaml"), "a").unwrap();
        store
            .record_file("omc", file_record("macros/intact.yaml", "a"))
            .unwrap();

        fs::create_dir_all(dir.0.join("skills")).unwrap();
        fs::write(dir.0.join("skills/modified.md"), "changed").unwrap();
        let mut modified = file_record("skills/modified.md", "original");
        modified.category = "skills".to_string();
        store.record_file("omc", modified).unwrap();

        let mut missing = file_record("roles/missing.md", "gone");
        missing.category = "roles".to_string();
        store.record_file("omc", missing).unwrap();

        let rows = bundle_list_rows(&store, &dir.0);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.name, "omc");
        assert_eq!(row.source, "https://github.com/x/omc");
        assert_eq!(
            row.drift,
            DriftSummary {
                intact: 1,
                modified: 1,
                missing: 1,
            }
        );
        assert_eq!(row.file_counts, "macros: 1, roles: 1, skills: 1");
        assert_eq!(
            row.drift.display(),
            "1 intact, 1 modified locally, 1 missing"
        );
    }

    #[test]
    fn bundle_rows_fall_back_to_the_short_commit_when_unversioned() {
        let dir = TempStoreDir::new("bundles-list-fallback");
        let mut store = dir.store();
        store
            .upsert_bundle("omc", metadata("https://github.com/x/omc", "abc123def456"))
            .unwrap();

        let rows = bundle_list_rows(&store, &dir.0);

        assert_eq!(rows[0].version, "abc123d");
        assert_eq!(rows[0].git_ref, "-");
        assert_eq!(rows[0].file_counts, "-");
        assert_eq!(rows[0].drift, DriftSummary::default());
        assert_eq!(rows[0].drift.display(), "-");
    }

    #[test]
    fn bundle_rows_show_manifest_version_and_pinned_ref() {
        let dir = TempStoreDir::new("bundles-list-versioned");
        let mut store = dir.store();
        store
            .upsert_bundle(
                "omc",
                InstallMetadata {
                    source: "git@github.com:x/omc.git".to_string(),
                    git_ref: Some("v1.4.0".to_string()),
                    commit: "abc123def456".to_string(),
                    version: Some("1.4.0".to_string()),
                    description: None,
                    homepage: None,
                },
            )
            .unwrap();

        let rows = bundle_list_rows(&store, &dir.0);

        assert_eq!(rows[0].version, "1.4.0");
        assert_eq!(rows[0].git_ref, "v1.4.0");
        assert_eq!(rows[0].source, "git@github.com:x/omc.git");
        assert!(!rows[0].installed_at.is_empty());
    }

    #[test]
    fn bundle_rows_are_empty_for_an_empty_store() {
        let dir = TempStoreDir::new("bundles-list-empty");

        let rows = bundle_list_rows(&dir.store(), &dir.0);

        assert!(rows.is_empty());
    }
}
