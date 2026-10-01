//! The share set: which files under a workspace root a trusted peer may fetch. Two YAML
//! files make it up, the global `<config_dir>/mesh/shares.yaml` and the workspace
//! `<workspace_root>/<workspace config dir>/mesh-shares.yaml`. A mutation lands in the
//! workspace file when it exists and in the global one otherwise, unless the caller picks
//! a layer. Both files are the user's own, so one this build cannot read fails closed the
//! way the trust list does: the whole set serves nothing, nothing is written over it, and
//! the load says so once.
//!
//! Evaluation is deny-first. A candidate inside the workspace config directory is refused
//! before any list is consulted, and nothing lifts that; then a user deny from either
//! layer; then the built-in deny of secrets and `.git/` at any depth, which an `override`
//! lifts for one exact file; only then does an allow serve it. An override grants nothing
//! on its own: an allow must still match. Patterns are globs anchored at the share root
//! where `**` alone crosses a `/`. The root is the caller's, never the current directory.

use crate::config::paths;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::trust::same_hash;
use crate::mesh::wire_path::WirePath;
use crate::mesh::{canonical_hash, mesh_config_dir, redact_hashes, write_atomically};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const SHARES_FILE_VERSION: u64 = 1;
/// The share list is the human's own decisions, so a refusal says what starting fresh costs.
const SHARES_FILE_REMEDY: Remedy =
    Remedy::UserFile("is the share list, and a fresh one shares nothing");

/// What no allow reaches, at any depth, unless an `override` names the exact file: secrets
/// by their usual names, and `.git/`, so `**` never enters a repository's own store. The
/// workspace config directory joins this list at runtime under whatever name it has.
const BUILTIN_DENY: [&str; 6] = [".env", ".env.*", "*.pem", "*.key", "id_*", ".git/**"];
const GLOB_METACHARACTERS: [char; 6] = ['*', '?', '[', ']', '{', '}'];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharesFile {
    version: u64,
    #[serde(default)]
    allow: Vec<AllowEntry>,
    #[serde(default)]
    deny: Vec<DenyEntry>,
    #[serde(default, rename = "override")]
    overrides: Vec<OverrideEntry>,
}

impl Default for SharesFile {
    fn default() -> Self {
        Self {
            version: SHARES_FILE_VERSION,
            allow: Vec::new(),
            deny: Vec::new(),
            overrides: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AllowEntry {
    pub pattern: String,
    /// Canonical 32-hex identity or destination hash; absent means every trusted peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
}

impl AllowEntry {
    /// A `peer` that is not canonical 32-hex matches nobody rather than everybody.
    fn applies_to(&self, peer: &PeerRef<'_>) -> bool {
        match &self.peer {
            None => true,
            Some(scoped) => {
                is_canonical(scoped)
                    && (same_hash(scoped, peer.identity) || same_hash(scoped, peer.destination))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DenyEntry {
    pub pattern: String,
}

/// One exact relative file path in the wire grammar, never a pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OverrideEntry {
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layer {
    Global,
    Workspace,
}

/// An allow entry as it applies to one peer, with the layer it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub pattern: String,
    pub peer: Option<String>,
    pub layer: Layer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteScope {
    Auto,
    Global,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Mutation {
    Allow {
        pattern: String,
        peer: Option<String>,
    },
    Deny {
        pattern: String,
    },
    /// Removes every allow entry with exactly this pattern text from the target layer.
    Unshare {
        pattern: String,
    },
    Override {
        path: String,
    },
}

impl Mutation {
    fn kind(&self) -> &'static str {
        match self {
            Self::Allow { .. } => "allow",
            Self::Deny { .. } => "deny",
            Self::Unshare { .. } => "unshare",
            Self::Override { .. } => "override",
        }
    }
}

/// The peer a share is evaluated for, both hashes canonical lowercase 32-hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerRef<'a> {
    pub identity: &'a str,
    pub destination: &'a str,
}

/// Where the two files live for one share root. Path arithmetic only; nothing is read. The
/// workspace config directory's name is taken from the process once, here, so every rule
/// derived from it agrees with where the workspace file was looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShareLocations {
    pub global: PathBuf,
    pub workspace: PathBuf,
    workspace_config_dir: PathBuf,
    workspace_config_dir_name: String,
}

impl ShareLocations {
    pub(crate) fn new(config_dir: &Path, workspace_root: &Path) -> Self {
        Self::with_dir_name(
            config_dir,
            workspace_root,
            paths::workspace_config_dir_name(),
        )
    }

    fn with_dir_name(
        config_dir: &Path,
        workspace_root: &Path,
        workspace_config_dir_name: String,
    ) -> Self {
        let workspace_config_dir = workspace_root.join(&workspace_config_dir_name);
        Self {
            global: mesh_config_dir(config_dir).join("shares.yaml"),
            workspace: workspace_config_dir.join("mesh-shares.yaml"),
            workspace_config_dir,
            workspace_config_dir_name,
        }
    }
}

/// Both files as loaded. A missing file is an empty layer; a file that cannot be read as
/// this version poisons the whole set: `effective` is empty, every `permits` is false and
/// `apply` refuses, so a corrupt file is never served around nor written over.
pub(crate) struct ShareSet {
    locations: ShareLocations,
    global: SharesFile,
    workspace: SharesFile,
    workspace_exists: bool,
    poisoned: Option<String>,
}

impl ShareSet {
    /// Never fails and never creates a file; a refused file is warned about once here and
    /// its refusal kept for `apply` to repeat.
    pub(crate) fn load(locations: ShareLocations) -> Self {
        let mut refusals = Vec::new();
        let mut read = |path: &Path| match read_shares_file(path) {
            Ok(Some(file)) => (file, true),
            Ok(None) => (SharesFile::default(), false),
            Err(err) => {
                refusals.push(redact_hashes(&format!("{err:#}")));
                (SharesFile::default(), true)
            }
        };
        let (global, _) = read(&locations.global);
        let (workspace, workspace_exists) = read(&locations.workspace);
        let poisoned = (!refusals.is_empty()).then(|| refusals.join(" "));
        if let Some(refusal) = &poisoned {
            warn!("{refusal} Nothing is shared until then.");
        }
        Self {
            locations,
            global,
            workspace,
            workspace_exists,
            poisoned,
        }
    }

    /// The allow entries of both layers that apply to `peer`; empty while poisoned.
    pub(crate) fn effective(&self, peer: &PeerRef<'_>) -> Vec<Entry> {
        if self.poisoned.is_some() {
            return Vec::new();
        }
        [
            (Layer::Global, &self.global),
            (Layer::Workspace, &self.workspace),
        ]
        .into_iter()
        .flat_map(|(layer, file)| {
            file.allow
                .iter()
                .filter(|entry| entry.applies_to(peer))
                .map(move |entry| Entry {
                    pattern: entry.pattern.clone(),
                    peer: entry.peer.clone(),
                    layer,
                })
        })
        .collect()
    }

    /// The compiled policy for `peer`. A pattern on disk that is not a glob is an error
    /// here rather than a silently dropped rule.
    pub(crate) fn rules(&self, peer: &PeerRef<'_>, case_insensitive: bool) -> Result<ShareRules> {
        let allow = compile(
            self.effective(peer)
                .iter()
                .map(|entry| entry.pattern.as_str()),
            case_insensitive,
        )?;
        let deny = compile(
            self.global
                .deny
                .iter()
                .chain(&self.workspace.deny)
                .map(|entry| entry.pattern.as_str()),
            case_insensitive,
        )?;
        let builtin = compile(
            builtin_deny_patterns(&self.locations.workspace_config_dir_name)
                .iter()
                .map(String::as_str),
            case_insensitive,
        )?;
        let overrides = self
            .global
            .overrides
            .iter()
            .chain(&self.workspace.overrides)
            .map(|entry| entry.path.clone())
            .collect();
        Ok(ShareRules {
            allow,
            deny,
            builtin,
            overrides,
            case_insensitive,
            protected_dirs: protected_dirs(&self.locations.workspace_config_dir),
        })
    }

    pub(crate) fn write_target(&self, scope: WriteScope) -> Layer {
        write_target(self.workspace_exists, scope)
    }

    /// Validates `mutation`, writes the target file atomically and only then updates
    /// memory, returning the file written. A mutation the file already holds is `Ok`
    /// without a write. Refused while poisoned, with the load's refusal, so a corrupt
    /// file is never replaced by a fresh one.
    pub(crate) fn apply(&mut self, mutation: Mutation, scope: WriteScope) -> Result<PathBuf> {
        if let Some(refusal) = &self.poisoned {
            bail!("{refusal} Nothing was written.");
        }
        let layer = self.write_target(scope);
        let (path, file) = match layer {
            Layer::Global => (&self.locations.global, &mut self.global),
            Layer::Workspace => (&self.locations.workspace, &mut self.workspace),
        };
        let mut next = file.clone();
        if mutate(&mut next, path, &mutation)? {
            let yaml =
                serde_yaml::to_string(&next).context("Failed to serialize the mesh share list")?;
            write_atomically(path, yaml.as_bytes())?;
            *file = next;
            if layer == Layer::Workspace {
                self.workspace_exists = true;
            }
            debug!(
                "Mesh share list '{}' updated: {}",
                path.display(),
                mutation.kind()
            );
        }
        Ok(path.clone())
    }
}

/// The layer a mutation lands in: under `Auto` the workspace file when it exists, else the
/// global one; `Global` and `Workspace` pick outright, and `Workspace` creates the file.
pub(crate) fn write_target(workspace_exists: bool, scope: WriteScope) -> Layer {
    match scope {
        WriteScope::Auto if workspace_exists => Layer::Workspace,
        WriteScope::Auto | WriteScope::Global => Layer::Global,
        WriteScope::Workspace => Layer::Workspace,
    }
}

/// The compiled policy, applied after the caller has parsed the wire path, resolved
/// `root/path` and checked that it lies under the canonical share root. Deny always wins:
/// a protected directory first, which nothing lifts; then a user deny from either layer;
/// then the built-in deny unless an override names this exact file; and only then does an
/// allow serve the file. An override lifts the built-in layer alone and grants nothing.
pub(crate) struct ShareRules {
    allow: GlobSet,
    deny: GlobSet,
    builtin: GlobSet,
    overrides: Vec<String>,
    case_insensitive: bool,
    protected_dirs: Vec<PathBuf>,
}

impl ShareRules {
    /// `relative` is the `/`-separated path under the share root as the wire carries it;
    /// `canonical` is where it resolved to on disk.
    pub(crate) fn permits(&self, relative: &str, canonical: &Path) -> bool {
        if self
            .protected_dirs
            .iter()
            .any(|dir| canonical.starts_with(dir))
        {
            return false;
        }
        if self.deny.is_match(relative) {
            return false;
        }
        if self.builtin.is_match(relative) && !self.overridden(relative) {
            return false;
        }
        self.allow.is_match(relative)
    }

    fn overridden(&self, relative: &str) -> bool {
        self.overrides.iter().any(|path| {
            if self.case_insensitive {
                path.to_lowercase() == relative.to_lowercase()
            } else {
                path == relative
            }
        })
    }
}

/// Every built-in pattern at the share root and at any depth below it, plus the workspace
/// config directory under `workspace_config_dir_name`.
fn builtin_deny_patterns(workspace_config_dir_name: &str) -> Vec<String> {
    let workspace_config = format!("{workspace_config_dir_name}/**");
    BUILTIN_DENY
        .iter()
        .map(|pattern| (*pattern).to_string())
        .chain([workspace_config])
        .flat_map(|pattern| [format!("**/{pattern}"), pattern])
        .collect()
}

/// The workspace config directories as they resolve on disk: the one under the share root
/// and the one the process has, which an absolute env override can place anywhere a name
/// glob would miss. A directory that does not resolve holds nothing to protect.
fn protected_dirs(workspace_config_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        workspace_config_dir.to_path_buf(),
        paths::workspace_config_dir(),
    ]
    .iter()
    .filter_map(|dir| dunce::canonicalize(dir).ok())
    .collect();
    dirs.dedup();
    dirs
}

fn glob(pattern: &str, case_insensitive: bool) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(case_insensitive)
        .build()
        .with_context(|| format!("Share pattern `{pattern}` is not a valid glob"))
}

fn compile<'a>(
    patterns: impl IntoIterator<Item = &'a str>,
    case_insensitive: bool,
) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for pattern in patterns {
        set.add(glob(pattern, case_insensitive)?);
    }
    set.build().context("Failed to compile the share patterns")
}

fn is_canonical(hash: &str) -> bool {
    canonical_hash(hash).as_deref() == Some(hash)
}

/// Applies `mutation` to `file`, which lives at `path`; `Ok(false)` means the file already
/// says so and nothing needs writing.
fn mutate(file: &mut SharesFile, path: &Path, mutation: &Mutation) -> Result<bool> {
    match mutation {
        Mutation::Allow { pattern, peer } => {
            validate_pattern(pattern)?;
            let peer = peer.as_deref().map(canonical_peer).transpose()?;
            let entry = AllowEntry {
                pattern: pattern.clone(),
                peer,
            };
            Ok(push_unless_present(&mut file.allow, entry))
        }
        Mutation::Deny { pattern } => {
            validate_pattern(pattern)?;
            let entry = DenyEntry {
                pattern: pattern.clone(),
            };
            Ok(push_unless_present(&mut file.deny, entry))
        }
        Mutation::Unshare { pattern } => {
            let before = file.allow.len();
            file.allow.retain(|entry| entry.pattern != *pattern);
            if file.allow.len() == before {
                bail!(
                    "No allow entry in mesh share list '{}' has the pattern `{pattern}`; nothing was changed.",
                    path.display()
                );
            }
            Ok(true)
        }
        Mutation::Override { path: file_path } => {
            validate_override(file_path)?;
            let entry = OverrideEntry {
                path: file_path.clone(),
            };
            Ok(push_unless_present(&mut file.overrides, entry))
        }
    }
}

fn push_unless_present<T: PartialEq>(entries: &mut Vec<T>, entry: T) -> bool {
    if entries.contains(&entry) {
        return false;
    }
    entries.push(entry);
    true
}

fn canonical_peer(peer: &str) -> Result<String> {
    canonical_hash(peer).ok_or_else(|| {
        anyhow!(
            "`{peer}` is not a peer hash; scope a share to a peer by the 32-hex identity or destination hash `.mesh peers` shows."
        )
    })
}

fn has_drive_prefix(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphabetic) && bytes.get(1) == Some(&b':')
}

fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.contains('\\') {
        bail!(
            "Share patterns use `/` between segments on every platform; `{pattern}` carries a `\\`. Write it with `/`."
        );
    }
    if pattern.starts_with('/') || has_drive_prefix(pattern) {
        bail!(
            "Share patterns are relative to the workspace root; `docs/**` means docs under whichever workspace is open. `{pattern}` starts at the filesystem root: drop that prefix."
        );
    }
    if pattern
        .split('/')
        .any(|segment| matches!(segment, "" | "." | ".."))
    {
        bail!(
            "Share pattern `{pattern}` has an empty, `.` or `..` segment; name the path from the workspace root down, like `docs/**`."
        );
    }
    glob(pattern, false)?;
    Ok(())
}

fn validate_override(path: &str) -> Result<()> {
    if path.contains(GLOB_METACHARACTERS) {
        bail!(
            "An override lifts the built-in deny for one exact file; `{path}` is a pattern. Name the file."
        );
    }
    WirePath::parse(path).with_context(|| {
        format!(
            "An override names one exact file by its `/`-separated path from the workspace root; `{path}` is not one"
        )
    })?;
    Ok(())
}

fn read_shares_file(path: &Path) -> Result<Option<SharesFile>> {
    match fs::read_to_string(path) {
        Ok(text) => parse_shares_file(path, &text).map(Some),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to read mesh share list '{}'", path.display()))
        }
    }
}

/// Reads the version alone first so a file from a newer Coyote gets a precise message
/// rather than an unknown-field error from whatever the newer layout added.
fn parse_shares_file(path: &Path, text: &str) -> Result<SharesFile> {
    let probe: VersionProbe = serde_yaml::from_str(text).with_context(|| {
        unversioned_refusal(
            "share list",
            path,
            None,
            SHARES_FILE_VERSION,
            SHARES_FILE_REMEDY,
        )
    })?;
    if probe.version != SHARES_FILE_VERSION {
        bail!(version_refusal(
            "share list",
            path,
            None,
            probe.version,
            SHARES_FILE_VERSION,
            SHARES_FILE_REMEDY
        ));
    }
    serde_yaml::from_str(text).with_context(|| {
        format!(
            "Mesh share list '{}' could not be parsed as version {SHARES_FILE_VERSION}. {}",
            path.display(),
            SHARES_FILE_REMEDY.sentence()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WORKSPACE_COYOTE_DIR_NAME;
    use crate::mesh::hex_lower;
    use crate::mesh::test_support::TempDir;
    use crate::testing::{EnvVarGuard, install_log_collector, warn_snapshot};
    use crate::utils::get_env_name;

    struct Fixture {
        _tmp: TempDir,
        config_dir: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let tmp = TempDir::new(tag);
            let config_dir = tmp.path.join("config");
            let root = tmp.path.join("workspace");
            fs::create_dir_all(&root).unwrap();
            Self {
                _tmp: tmp,
                config_dir,
                root,
            }
        }

        /// Pins the default directory name so these tests neither read nor depend on the
        /// env override another test may be holding; the override itself is exercised
        /// under `serial` below.
        fn locations(&self) -> ShareLocations {
            ShareLocations::with_dir_name(
                &self.config_dir,
                &self.root,
                WORKSPACE_COYOTE_DIR_NAME.to_string(),
            )
        }

        fn load(&self) -> ShareSet {
            ShareSet::load(self.locations())
        }

        fn write(&self, layer: Layer, text: &str) -> PathBuf {
            let path = match layer {
                Layer::Global => self.locations().global,
                Layer::Workspace => self.locations().workspace,
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }

        /// Creates `relative` under the share root and returns where it resolved to.
        fn file(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, relative).unwrap();
            dunce::canonicalize(path).unwrap()
        }
    }

    fn fake_hash(fill: u8) -> String {
        hex_lower(&[fill; 16])
    }

    fn allow(pattern: &str) -> Mutation {
        Mutation::Allow {
            pattern: pattern.to_string(),
            peer: None,
        }
    }

    fn deny(pattern: &str) -> Mutation {
        Mutation::Deny {
            pattern: pattern.to_string(),
        }
    }

    fn lift(path: &str) -> Mutation {
        Mutation::Override {
            path: path.to_string(),
        }
    }

    fn patterns(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.pattern.as_str()).collect()
    }

    /// Whether `relative` is served to a peer nothing is scoped to; the resolved path is
    /// the plain join, which no protected directory contains.
    fn served(set: &ShareSet, fx: &Fixture, relative: &str) -> bool {
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        set.rules(&peer, false)
            .unwrap()
            .permits(relative, &fx.root.join(relative))
    }

    #[test]
    fn locations_put_the_global_file_under_mesh_and_the_workspace_file_in_its_config_dir() {
        let locations = ShareLocations::with_dir_name(
            Path::new("cfg"),
            Path::new("ws"),
            WORKSPACE_COYOTE_DIR_NAME.to_string(),
        );

        assert_eq!(locations.global, Path::new("cfg/mesh/shares.yaml"));
        assert_eq!(
            locations.workspace,
            Path::new("ws/.coyote/mesh-shares.yaml")
        );
    }

    #[test]
    fn load_of_missing_files_is_empty_and_creates_nothing() {
        let fx = Fixture::new("shares-missing");
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        let set = fx.load();

        assert!(set.effective(&peer).is_empty());
        assert!(set.poisoned.is_none());
        assert!(!set.workspace_exists);
        assert!(!served(&set, &fx, "README.md"));
        assert!(!fx.config_dir.exists());
        assert!(!fx.locations().workspace.parent().unwrap().exists());
    }

    #[test]
    fn a_corrupt_file_poisons_the_whole_set_and_is_warned_about_once() {
        install_log_collector();
        let fx = Fixture::new("shares-corrupt");
        let corrupt = fx.write(Layer::Global, "version: 1\nallow: [\n");
        fx.write(Layer::Workspace, "version: 1\nallow:\n- pattern: '**'\n");
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        let mut set = fx.load();

        assert!(set.effective(&peer).is_empty());
        assert!(!served(&set, &fx, "README.md"));
        let err = set
            .apply(allow("docs/**"), WriteScope::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&corrupt.display().to_string()), "{err}");
        assert!(err.contains("Nothing was written"), "{err}");
        assert_eq!(
            fs::read_to_string(&corrupt).unwrap(),
            "version: 1\nallow: [\n"
        );
        let warned: Vec<String> = warn_snapshot()
            .into_iter()
            .filter(|line| line.contains(&corrupt.display().to_string()))
            .collect();
        assert_eq!(warned.len(), 1, "{warned:#?}");
        assert!(
            warned[0].contains("Nothing is shared until then"),
            "{warned:#?}"
        );
        assert!(warned[0].contains("move the file aside"), "{warned:#?}");
    }

    #[test]
    fn a_newer_file_version_is_refused_asking_for_an_upgrade() {
        let fx = Fixture::new("shares-newer");
        let newer = SHARES_FILE_VERSION + 1;
        let path = fx.write(
            Layer::Workspace,
            &format!("version: {newer}\nallow: []\nfuture_section: {{}}\n"),
        );

        let set = fx.load();

        let refusal = set.poisoned.clone().unwrap();
        assert!(refusal.contains(&path.display().to_string()), "{refusal}");
        assert!(refusal.contains(&format!("version {newer}")), "{refusal}");
        assert!(refusal.contains("upgrade Coyote"), "{refusal}");
        assert!(refusal.contains("move the file aside"), "{refusal}");
        assert!(refusal.contains("shares nothing"), "{refusal}");
        assert!(set.workspace_exists);
    }

    #[test]
    fn a_pre_baseline_file_version_is_refused_as_having_no_migration() {
        let fx = Fixture::new("shares-older");
        fx.write(Layer::Global, "version: 0\nallow: []\n");

        let refusal = fx.load().poisoned.unwrap();

        assert!(refusal.contains("version 0"), "{refusal}");
        assert!(refusal.contains("no migration"), "{refusal}");
        assert!(!refusal.contains("upgrade Coyote"), "{refusal}");
    }

    #[test]
    fn a_file_without_a_version_is_refused_as_an_unknown_shape() {
        let fx = Fixture::new("shares-unversioned");
        let path = fx.write(Layer::Global, "allow:\n- pattern: '**'\n");

        let set = fx.load();

        let refusal = set.poisoned.clone().unwrap();
        assert!(refusal.contains(&path.display().to_string()), "{refusal}");
        assert!(refusal.contains("no readable `version` field"), "{refusal}");
        assert!(!served(&set, &fx, "README.md"));
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_partially_loaded() {
        let fx = Fixture::new("shares-unknown-field");
        fx.write(
            Layer::Global,
            "version: 1\nallow:\n- pattern: docs/**\n  peers: everyone\n",
        );

        let set = fx.load();

        let refusal = set.poisoned.clone().unwrap();
        assert!(
            refusal.contains("could not be parsed as version 1"),
            "{refusal}"
        );
        assert!(!served(&set, &fx, "docs/a.md"));
    }

    #[test]
    fn write_target_prefers_an_existing_workspace_file_unless_a_layer_is_named() {
        let table = [
            (false, WriteScope::Auto, Layer::Global),
            (true, WriteScope::Auto, Layer::Workspace),
            (false, WriteScope::Global, Layer::Global),
            (true, WriteScope::Global, Layer::Global),
            (false, WriteScope::Workspace, Layer::Workspace),
            (true, WriteScope::Workspace, Layer::Workspace),
        ];
        for (workspace_exists, scope, expected) in table {
            assert_eq!(
                write_target(workspace_exists, scope),
                expected,
                "workspace_exists={workspace_exists} scope={scope:?}"
            );
        }
    }

    #[test]
    fn apply_writes_the_file_write_target_names_and_leaves_the_other_alone() {
        let fx = Fixture::new("shares-apply-auto-global");
        let locations = fx.locations();
        let written = fx.load().apply(allow("docs/**"), WriteScope::Auto).unwrap();
        assert_eq!(written, locations.global);
        assert!(locations.global.exists());
        assert!(!locations.workspace.exists());
        assert!(!locations.global.with_added_extension("tmp").exists());

        let fx = Fixture::new("shares-apply-auto-workspace");
        let locations = fx.locations();
        fx.write(Layer::Workspace, "version: 1\n");
        let written = fx.load().apply(allow("docs/**"), WriteScope::Auto).unwrap();
        assert_eq!(written, locations.workspace);
        assert!(!locations.global.exists());

        let fx = Fixture::new("shares-apply-forced-global");
        let locations = fx.locations();
        fx.write(Layer::Workspace, "version: 1\n");
        let written = fx
            .load()
            .apply(allow("docs/**"), WriteScope::Global)
            .unwrap();
        assert_eq!(written, locations.global);
        assert_eq!(
            fs::read_to_string(&locations.workspace).unwrap(),
            "version: 1\n"
        );

        let fx = Fixture::new("shares-apply-forced-workspace");
        let locations = fx.locations();
        let mut set = fx.load();
        let written = set.apply(allow("docs/**"), WriteScope::Workspace).unwrap();
        assert_eq!(written, locations.workspace);
        assert!(!locations.global.exists());
        assert!(set.workspace_exists);
        assert_eq!(set.write_target(WriteScope::Auto), Layer::Workspace);
    }

    #[test]
    fn a_written_file_round_trips_with_its_version_and_every_section() {
        let fx = Fixture::new("shares-roundtrip");
        let peer = fake_hash(0x1a);
        let mut set = fx.load();
        set.apply(
            Mutation::Allow {
                pattern: "docs/**".into(),
                peer: Some(peer.to_ascii_uppercase()),
            },
            WriteScope::Auto,
        )
        .unwrap();
        set.apply(deny("docs/private/**"), WriteScope::Auto)
            .unwrap();
        set.apply(lift(".env.example"), WriteScope::Auto).unwrap();

        let text = fs::read_to_string(&fx.locations().global).unwrap();

        assert!(text.starts_with("version: 1\n"), "{text}");
        assert!(text.contains(&format!("  peer: {peer}\n")), "{text}");
        assert!(text.contains("override:\n- path: .env.example\n"), "{text}");
        let reloaded = fx.load();
        assert_eq!(reloaded.global, set.global);
        assert!(reloaded.poisoned.is_none());
    }

    #[test]
    fn deny_wins_across_layers_and_an_override_lifts_only_the_builtin_deny() {
        let fx = Fixture::new("shares-precedence-global-allow");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(deny("docs/**"), WriteScope::Workspace).unwrap();
        assert!(!served(&set, &fx, "docs/a.md"));
        assert!(served(&set, &fx, "src/a.rs"));

        let fx = Fixture::new("shares-precedence-workspace-allow");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Workspace).unwrap();
        set.apply(deny("docs/**"), WriteScope::Global).unwrap();
        assert!(!served(&set, &fx, "docs/a.md"));
        assert!(served(&set, &fx, "src/a.rs"));

        let fx = Fixture::new("shares-precedence-override");
        let mut set = fx.load();
        set.apply(lift(".env.example"), WriteScope::Auto).unwrap();
        assert!(
            !served(&set, &fx, ".env.example"),
            "an override alone grants nothing"
        );
        set.apply(allow("**"), WriteScope::Auto).unwrap();
        assert!(served(&set, &fx, ".env.example"));
        assert!(!served(&set, &fx, ".env"), "the override is for one file");
        set.apply(deny(".env.*"), WriteScope::Auto).unwrap();
        assert!(
            !served(&set, &fx, ".env.example"),
            "an override never lifts a user deny"
        );
    }

    #[test]
    fn an_override_on_a_pattern_is_refused_at_write_time() {
        let fx = Fixture::new("shares-override-glob");
        let mut set = fx.load();

        for pattern in ["*.pem", "keys/?.key", "[a].env", "{a,b}.pem"] {
            let err = set
                .apply(lift(pattern), WriteScope::Auto)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("one exact file") && err.contains("is a pattern"),
                "{err}"
            );
        }
        let err = format!(
            "{:#}",
            set.apply(lift("/etc/.env"), WriteScope::Auto).unwrap_err()
        );
        assert!(err.contains("leading_slash"), "{err}");
        assert!(!fx.locations().global.exists());
    }

    #[test]
    fn an_absolute_or_malformed_pattern_is_refused_with_a_teaching_error() {
        let fx = Fixture::new("shares-absolute-pattern");
        let mut set = fx.load();

        for pattern in ["/docs/**", "C:/docs/**"] {
            let err = set
                .apply(allow(pattern), WriteScope::Auto)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("relative to the workspace root"),
                "{pattern}: {err}"
            );
            assert!(err.contains("docs/**"), "{pattern}: {err}");
        }
        let err = set
            .apply(deny("docs\\**"), WriteScope::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains("use `/`"), "{err}");
        for pattern in ["../secrets/**", "docs//**", "./docs", "docs/", ""] {
            let err = set
                .apply(allow(pattern), WriteScope::Auto)
                .unwrap_err()
                .to_string();
            assert!(err.contains("segment"), "{pattern}: {err}");
        }
        let err = set
            .apply(allow("docs/[a-"), WriteScope::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid glob"), "{err}");
        assert!(!fx.locations().global.exists());
    }

    #[test]
    fn double_star_never_enters_a_git_directory_nor_reaches_a_secret_at_any_depth() {
        let fx = Fixture::new("shares-builtin");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Auto).unwrap();

        for denied in [
            ".git/config",
            ".git/objects/ab/cdef",
            "vendor/lib/.git/HEAD",
            ".env",
            "deploy/.env",
            ".env.local",
            "deploy/.env.production",
            "server.pem",
            "certs/server.pem",
            "tls.key",
            "certs/tls.key",
            "id_rsa",
            ".ssh/id_ed25519",
        ] {
            assert!(!served(&set, &fx, denied), "{denied} must not be served");
        }
        for allowed in [
            "README.md",
            "src/git/config.rs",
            "docs/environment.md",
            "notes/pem-formats.md",
            "keys.md",
            "src/ids/id.rs",
        ] {
            assert!(served(&set, &fx, allowed), "{allowed} must be served");
        }
    }

    #[test]
    fn a_single_star_does_not_cross_a_separator_but_double_star_does() {
        let fx = Fixture::new("shares-star");
        let mut set = fx.load();
        set.apply(allow("docs/*.md"), WriteScope::Auto).unwrap();
        assert!(served(&set, &fx, "docs/a.md"));
        assert!(!served(&set, &fx, "docs/sub/a.md"));
        assert!(!served(&set, &fx, "a.md"));

        set.apply(allow("src/**"), WriteScope::Auto).unwrap();
        assert!(served(&set, &fx, "src/a.rs"));
        assert!(served(&set, &fx, "src/deep/er/a.rs"));
        assert!(!served(&set, &fx, "srcs/a.rs"));
    }

    #[test]
    fn a_peer_scoped_allow_matches_the_identity_or_the_destination_and_nobody_else() {
        let fx = Fixture::new("shares-peer");
        let (identity, destination, other) = (fake_hash(0x1a), fake_hash(0x2b), fake_hash(0x3c));
        let mut set = fx.load();
        set.apply(
            Mutation::Allow {
                pattern: "by-identity/**".into(),
                peer: Some(identity.clone()),
            },
            WriteScope::Auto,
        )
        .unwrap();
        set.apply(
            Mutation::Allow {
                pattern: "by-destination/**".into(),
                peer: Some(destination.clone()),
            },
            WriteScope::Auto,
        )
        .unwrap();
        set.apply(allow("public/**"), WriteScope::Auto).unwrap();

        let both = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        assert_eq!(
            patterns(&set.effective(&both)),
            ["by-identity/**", "by-destination/**", "public/**"]
        );
        let same_instance_other_identity = PeerRef {
            identity: &other,
            destination: &destination,
        };
        assert_eq!(
            patterns(&set.effective(&same_instance_other_identity)),
            ["by-destination/**", "public/**"]
        );
        let stranger = PeerRef {
            identity: &other,
            destination: &other,
        };
        assert_eq!(patterns(&set.effective(&stranger)), ["public/**"]);
        let rules = set.rules(&stranger, false).unwrap();
        assert!(!rules.permits("by-identity/a.md", &fx.root.join("by-identity/a.md")));
        assert!(rules.permits("public/a.md", &fx.root.join("public/a.md")));
    }

    #[test]
    fn a_peer_that_is_not_a_canonical_hash_is_refused_at_write_and_ignored_on_disk() {
        let fx = Fixture::new("shares-peer-noncanonical");
        let identity = fake_hash(0x1a);
        let mut set = fx.load();
        let err = set
            .apply(
                Mutation::Allow {
                    pattern: "docs/**".into(),
                    peer: Some("bob".into()),
                },
                WriteScope::Auto,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a peer hash"), "{err}");

        fx.write(
            Layer::Global,
            &format!(
                "version: 1\nallow:\n- pattern: upper/**\n  peer: {}\n- pattern: named/**\n  peer: bob\n- pattern: open/**\n",
                identity.to_ascii_uppercase()
            ),
        );
        let set = fx.load();
        let peer = PeerRef {
            identity: &identity,
            destination: &identity,
        };
        assert!(set.poisoned.is_none());
        assert_eq!(patterns(&set.effective(&peer)), ["open/**"]);
    }

    #[test]
    fn effective_carries_the_layer_each_allow_came_from() {
        let fx = Fixture::new("shares-layers");
        let mut set = fx.load();
        set.apply(allow("global/**"), WriteScope::Global).unwrap();
        set.apply(allow("workspace/**"), WriteScope::Workspace)
            .unwrap();
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));

        let entries = set.effective(&PeerRef {
            identity: &identity,
            destination: &destination,
        });

        assert_eq!(
            entries,
            [
                Entry {
                    pattern: "global/**".into(),
                    peer: None,
                    layer: Layer::Global,
                },
                Entry {
                    pattern: "workspace/**".into(),
                    peer: None,
                    layer: Layer::Workspace,
                },
            ]
        );
    }

    #[test]
    fn repeating_a_mutation_is_a_no_op_that_writes_nothing() {
        let fx = Fixture::new("shares-idempotent");
        let mut set = fx.load();
        let path = set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        set.apply(deny("docs/private/**"), WriteScope::Auto)
            .unwrap();
        set.apply(lift(".env.example"), WriteScope::Auto).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{before}# untouched\n")).unwrap();

        set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        set.apply(deny("docs/private/**"), WriteScope::Auto)
            .unwrap();
        set.apply(lift(".env.example"), WriteScope::Auto).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("{before}# untouched\n")
        );
        assert_eq!(set.global.allow.len(), 1);
        assert_eq!(set.global.deny.len(), 1);
        assert_eq!(set.global.overrides.len(), 1);
    }

    #[test]
    fn unshare_removes_every_allow_with_that_pattern_and_refuses_an_unknown_one() {
        let fx = Fixture::new("shares-unshare");
        let peer = fake_hash(0x1a);
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        set.apply(
            Mutation::Allow {
                pattern: "docs/**".into(),
                peer: Some(peer.clone()),
            },
            WriteScope::Auto,
        )
        .unwrap();
        set.apply(allow("src/**"), WriteScope::Auto).unwrap();

        let path = set
            .apply(
                Mutation::Unshare {
                    pattern: "docs/**".into(),
                },
                WriteScope::Auto,
            )
            .unwrap();

        assert_eq!(
            set.global.allow,
            [AllowEntry {
                pattern: "src/**".into(),
                peer: None,
            }]
        );
        assert_eq!(fx.load().global, set.global);
        let err = set
            .apply(
                Mutation::Unshare {
                    pattern: "docs/**".into(),
                },
                WriteScope::Auto,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("docs/**"), "{err}");
    }

    #[test]
    fn case_insensitive_rules_match_across_case_and_so_does_the_override() {
        let fx = Fixture::new("shares-case");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        set.apply(lift("docs/.Env.Example"), WriteScope::Auto)
            .unwrap();
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        let exact = set.rules(&peer, false).unwrap();
        let folded = set.rules(&peer, true).unwrap();

        assert!(exact.permits("docs/x.md", &fx.root.join("docs/x.md")));
        assert!(!exact.permits("Docs/x.md", &fx.root.join("Docs/x.md")));
        assert!(folded.permits("Docs/x.md", &fx.root.join("Docs/x.md")));
        assert!(!exact.permits("docs/.env.example", &fx.root.join("docs/.env.example")));
        assert!(folded.permits("docs/.env.example", &fx.root.join("docs/.env.example")));
        assert!(!folded.permits("docs/.env.sample", &fx.root.join("docs/.env.sample")));
    }

    #[test]
    #[serial_test::serial]
    fn the_workspace_config_dir_is_never_served_under_allow_everything() {
        let env_name = get_env_name("workspace_config_dir");
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        // The real constructor reads the process's name for the directory; the fixture's
        // pinned one would hide what this test is about.
        let live = |fx: &Fixture| ShareSet::load(ShareLocations::new(&fx.config_dir, &fx.root));

        let _default_name = EnvVarGuard::unset(&env_name);
        let fx = Fixture::new("shares-protected-default");
        let mut set = live(&fx);
        set.apply(allow("**"), WriteScope::Global).unwrap();
        let relative = format!("{WORKSPACE_COYOTE_DIR_NAME}/sessions/notes.md");
        let inside = fx.file(&relative);
        let readme = fx.file("README.md");
        let rules = set.rules(&peer, false).unwrap();
        assert!(!rules.permits(&relative, &inside));
        assert!(rules.permits("README.md", &readme));
        let patterns = builtin_deny_patterns(&paths::workspace_config_dir_name());
        assert!(
            patterns.contains(&format!("{WORKSPACE_COYOTE_DIR_NAME}/**")),
            "{patterns:?}"
        );
        assert!(
            patterns.contains(&format!("**/{WORKSPACE_COYOTE_DIR_NAME}/**")),
            "{patterns:?}"
        );

        let fx = Fixture::new("shares-protected-override");
        let hidden = fx.root.join("hidden-cfg");
        let _absolute_name = EnvVarGuard::set(&env_name, &hidden);
        let mut set = live(&fx);
        set.apply(allow("**"), WriteScope::Global).unwrap();
        let inside = fx.file("hidden-cfg/sessions/notes.md");
        let readme = fx.file("README.md");
        let rules = set.rules(&peer, false).unwrap();
        assert_eq!(
            ShareLocations::new(&fx.config_dir, &fx.root).workspace,
            hidden.join("mesh-shares.yaml")
        );
        assert!(
            !rules.permits("hidden-cfg/sessions/notes.md", &inside),
            "the resolved path, not the name, is what protects an overridden config dir"
        );
        assert!(rules.permits("README.md", &readme));
        assert!(
            !rules.builtin.is_match("hidden-cfg/sessions/notes.md"),
            "the name glob cannot see an absolute override; the prefix check must"
        );
    }
}
