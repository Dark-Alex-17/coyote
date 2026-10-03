//! The share set: which files under a workspace root a trusted peer may fetch. Two YAML
//! files make it up, the global `<config_dir>/mesh/shares.yaml` and the workspace
//! `<workspace_root>/<workspace config dir>/mesh-shares.yaml`. A mutation lands in the
//! workspace file when it exists and in the global one otherwise, unless the caller picks
//! a layer. Both files are the user's own, so one this build cannot read fails closed the
//! way the trust list does: the whole set serves nothing, nothing is written over it, and
//! the load says so once.
//!
//! Evaluation is deny-first and judged on the file that is actually on disk. A candidate
//! inside either config directory, the workspace's or the global one, is refused before
//! any list is consulted, and nothing lifts that; then a user deny from either layer; then
//! the built-in deny of secrets and `.git` at any depth, which an `override` lifts for one
//! exact resolved file; only then does an allow serve it. Deny and built-in deny are judged
//! on both the name the peer sent, a parsed `WirePath`, and the path it resolved to under
//! the share root; allow and override are judged on the resolved path alone. An override
//! grants nothing on its own: an allow must still match, and only the global file's
//! overrides count, since the workspace file arrives with a cloned repository and must not
//! be able to lift the deny on `.env` by itself. Patterns are globs anchored at the
//! share root where `**` alone crosses a `/`. The root is the caller's, never the current
//! directory. A grant from the `GrantStore` lets one peer fetch a named file no allow
//! reaches, after every deny has had its say; `is_served` is the one path a fetch takes
//! through all of this, and `list` walks only what the allows name.

use crate::config::{WORKSPACE_COYOTE_DIR_NAME, paths};
use crate::mesh::grants::GrantStore;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::trust::same_hash;
use crate::mesh::wire_path::WirePath;
use crate::mesh::write_atomically;
use crate::mesh::{
    canonical_hash, hex_lower, mesh_cache_dir, mesh_config_dir, redact_hashes, short,
};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const SHARES_FILE_VERSION: u64 = 1;
/// The share list is the human's own decisions, so a refusal says what starting fresh costs.
const SHARES_FILE_REMEDY: Remedy =
    Remedy::UserFile("is the share list, and a fresh one shares nothing");
/// How many entries, files and directories alike, one listing visits before it stops
/// and says it was cut short; a workspace with a `node_modules` under `**` would otherwise
/// hash for minutes on a peer's request.
pub(crate) const DEFAULT_LIST_WALK_BOUND: usize = 100_000;
/// Entries per listing page, the wire's cap.
pub(crate) const LIST_PAGE_SIZE: usize = 1_000;

/// What no allow reaches, at any depth, unless an `override` names the exact file: secrets
/// by their usual names, and `.git` as a directory or as the file a worktree or submodule
/// has, so `**` never enters a repository's own store. The workspace config directory
/// joins this list at runtime under whatever name it has.
const BUILTIN_DENY: [&str; 7] = [
    ".env", ".env.*", "*.pem", "*.key", "id_*", ".git", ".git/**",
];
pub(crate) const GLOB_METACHARACTERS: [char; 6] = ['*', '?', '[', ']', '{', '}'];

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
                is_canonical_peer(scoped)
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
    /// Removes every allow and deny entry with exactly this pattern text, and every
    /// override naming exactly this path, from the target layer.
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

/// What `apply` did: the file it targeted, and whether that file changed or already
/// said so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub path: PathBuf,
    pub changed: bool,
}

/// The peer a share is evaluated for, both hashes canonical lowercase 32-hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerRef<'a> {
    pub identity: &'a str,
    pub destination: &'a str,
}

impl PeerRef<'static> {
    /// The reference no scoped allow applies to, so `effective` and the walks answer for
    /// every trusted peer at once: a scoped entry compares against a canonical 32-hex
    /// hash, which an empty string never equals, so only peer-less entries match it.
    pub(crate) const fn unscoped() -> Self {
        Self {
            identity: "",
            destination: "",
        }
    }
}

/// The verdict on one fetch. `NotShared` covers a file that does not exist, one outside
/// every allow and one a deny refuses alike, so a peer learns nothing about the tree from
/// the answer; only a name the grammar refuses is told which rule it broke, and only a
/// file the peer could otherwise fetch is told it is too large, so a size never leaks
/// for a file the rules would not serve.
#[derive(Debug)]
pub(crate) enum Served {
    File(ServedFile),
    NotShared,
    InvalidPath { rule: &'static str },
    TooLarge { size: u64 },
}

/// The open handle the verdict was reached on, so the reader reads the very file that was
/// judged and not whatever the path names by then. `via` says which layer let it through.
#[derive(Debug)]
pub(crate) struct ServedFile {
    pub file: fs::File,
    #[cfg(test)]
    pub canonical: PathBuf,
    pub size: u64,
    pub via: Via,
}

/// What let a served file through: an allow in the share set, or a one-shot grant whose
/// use the serve has spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    Allow,
    Grant,
}

/// Where a candidate stands against the policy: `Denied` is what nothing lifts, `Allowed`
/// is served, and `NotAllowed` is a file no rule names, which only a grant lets through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Judgement {
    Denied,
    Allowed,
    NotAllowed,
}

/// `Judgement` with the reason a denial hides, for the human's own listing rather than a
/// peer's answer. `Protected` is a file under a directory nothing lifts, and also one
/// that did not resolve to plain segments under the share root, since no rule can serve
/// either; `Denied` is a user deny; `BuiltinDenied` the built-in deny no override lifts.
/// `Shared` and `NotAllowed` are `Allowed` and `NotAllowed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Shared,
    Protected,
    Denied,
    BuiltinDenied,
    NotAllowed,
}

/// One file in a listing, named by the wire path a peer fetches it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub path: String,
    pub size: u64,
    pub sha256: [u8; 32],
    pub mtime: SystemTime,
}

/// One page of a listing. `truncated` says the walk hit its bound, so files may be
/// missing from every page; `next` is the cursor for the page after this one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Listing {
    pub entries: Vec<Listed>,
    pub truncated: bool,
    pub next: Option<String>,
}

/// The human's view of what `list` serves a peer and what it holds back: every regular
/// file an allow names, each with its verdict, so a deny that swallows an allow is
/// visible. `truncated` says the walk hit its bound; `capped` that more than `cap`
/// entries were found and the rest cut.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Resolved {
    pub entries: Vec<ResolvedEntry>,
    pub truncated: bool,
    pub capped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedEntry {
    pub path: String,
    pub verdict: Verdict,
}

/// How many regular files one pattern alone reaches under the share root, counted up to
/// `cap`: `capped` says the count stopped there, `truncated` that the walk hit its bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MatchCount {
    pub files: usize,
    pub capped: bool,
    pub truncated: bool,
}

/// One entry as a share file holds it, with the layer it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawEntry {
    pub layer: Layer,
    pub kind: RawKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawKind {
    Allow {
        pattern: String,
        peer: Option<String>,
    },
    Deny {
        pattern: String,
    },
    Override {
        path: String,
    },
}

/// Where the two files live for one share root, and the directories nothing may serve out
/// of. Path arithmetic only; nothing is read. The workspace config directory's name is
/// taken from the process once, here, so every rule derived from it agrees with where the
/// workspace file was looked up. `with_cache_dir` adds the mesh cache directory, where
/// grants, the inbox and pending questions live, to what is protected; `with_protected`
/// adds any other directory, such as a configured inbox that lies outside the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShareLocations {
    pub global: PathBuf,
    pub workspace: PathBuf,
    config_dir: PathBuf,
    workspace_root: PathBuf,
    workspace_config_dir_name: String,
    mesh_cache_dir: Option<PathBuf>,
    protected: Vec<PathBuf>,
}

impl ShareLocations {
    pub(crate) fn new(config_dir: &Path, workspace_root: &Path) -> Self {
        Self::with_dir_name(
            config_dir,
            workspace_root,
            paths::workspace_config_dir_name(),
        )
    }

    pub(crate) fn with_dir_name(
        config_dir: &Path,
        workspace_root: &Path,
        workspace_config_dir_name: String,
    ) -> Self {
        Self {
            global: mesh_config_dir(config_dir).join("shares.yaml"),
            workspace: workspace_root
                .join(&workspace_config_dir_name)
                .join("mesh-shares.yaml"),
            config_dir: config_dir.to_path_buf(),
            workspace_root: workspace_root.to_path_buf(),
            workspace_config_dir_name,
            mesh_cache_dir: None,
            protected: Vec::new(),
        }
    }

    pub(crate) fn with_cache_dir(mut self, cache_dir: &Path) -> Self {
        self.mesh_cache_dir = Some(mesh_cache_dir(cache_dir));
        self
    }

    pub(crate) fn with_protected(mut self, dir: &Path) -> Self {
        self.protected.push(dir.to_path_buf());
        self
    }
}

/// Both files as loaded. A missing file is an empty layer; a file that cannot be read as
/// this version poisons the whole set: `effective` is empty, `rules` refuses, and so
/// does `apply`, so a corrupt file is never served around nor written over. Refusing
/// the rules outright, rather than serving on empty allows, is what keeps a grant from
/// slipping past a deny the refused file carried.
pub(crate) struct ShareSet {
    locations: ShareLocations,
    global: SharesFile,
    workspace: SharesFile,
    workspace_exists: bool,
    poisoned: Option<String>,
}

impl ShareSet {
    /// Never fails and never creates a file; a refused file is warned about once here and
    /// its refusal kept for `apply` to repeat. The warning is the outer context alone, the
    /// file and the remedy; the offending entry is the user's own text and stays in the
    /// refusal. Nothing in production loads this way yet: the wire path and the REPL's
    /// share verbs use `load_quietly` and own the warning.
    #[cfg(test)]
    pub(crate) fn load(locations: ShareLocations) -> Self {
        let (set, warning) = Self::load_quietly(locations);
        if let Some(warning) = warning {
            warn!("{warning}");
        }
        set
    }

    /// `load` with the warning returned instead of logged, for a caller that loads per
    /// request and must not let a peer make the operator's log repeat itself.
    pub(crate) fn load_quietly(locations: ShareLocations) -> (Self, Option<String>) {
        let mut refusals = Vec::new();
        let mut warnings = Vec::new();
        let mut read = |path: &Path| match read_shares_file(path) {
            Ok(Some(file)) => (file, true),
            Ok(None) => (SharesFile::default(), false),
            Err(err) => {
                refusals.push(redact_hashes(&format!("{err:#}")));
                warnings.push(redact_hashes(&format!("{err}")));
                (SharesFile::default(), true)
            }
        };
        let (global, _) = read(&locations.global);
        let workspace = read(&locations.workspace);
        let poisoned = (!refusals.is_empty()).then(|| refusals.join(" "));
        let warning = (!warnings.is_empty())
            .then(|| format!("{} Nothing is shared until then.", warnings.join(" ")));
        (
            Self {
                locations,
                global,
                workspace: workspace.0,
                workspace_exists: workspace.1,
                poisoned,
            },
            warning,
        )
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

    /// The override entries the workspace file carries, which lift nothing: a repository
    /// can ship that file, so an override there is read and kept but never applied. The
    /// REPL shows them so the human knows to move one to the global file.
    #[cfg(test)]
    pub(crate) fn inert_overrides(&self) -> &[OverrideEntry] {
        if self.poisoned.is_some() {
            return &[];
        }
        &self.workspace.overrides
    }

    /// The compiled policy for `peer`. `case_insensitive` must be the probe result for the
    /// share root's filesystem: `false` is only safe on a case-sensitive root, since a
    /// folding one serves `DOCS/x` past a deny on `docs/**`. A pattern on disk that is not
    /// a glob is an error here rather than a silently dropped rule, and so is a share root
    /// that does not resolve, since every allow is judged under it. Nothing in this type
    /// caches: a `ShareSet` is loaded per evaluation or REPL verb, and a caller that holds
    /// one must reload to see a repaired file.
    pub(crate) fn rules(&self, peer: &PeerRef<'_>, case_insensitive: bool) -> Result<ShareRules> {
        if self.poisoned.is_some() {
            // The refusal itself quotes the hand-edited entry, which `apply` may show the
            // human but a per-request log line must not carry; `load` already warned.
            bail!(
                "A mesh share list was refused when it was loaded, so nothing is shared until it is fixed."
            );
        }
        let canonical_root = self.canonical_root()?;
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
        let builtin = self.builtin(case_insensitive)?;
        let overrides = self
            .global
            .overrides
            .iter()
            .map(|entry| entry.path.clone())
            .collect();
        Ok(ShareRules {
            allow,
            deny,
            builtin,
            overrides,
            case_insensitive,
            protected_dirs: protected_dirs(&self.locations),
            canonical_root,
        })
    }

    fn canonical_root(&self) -> Result<PathBuf> {
        dunce::canonicalize(&self.locations.workspace_root)
            .context("Failed to resolve the share root")
    }

    fn builtin(&self, case_insensitive: bool) -> Result<GlobSet> {
        compile(
            builtin_deny_patterns(&self.locations.workspace_config_dir_name)
                .iter()
                .map(String::as_str),
            case_insensitive,
        )
    }

    /// Whether `peer` may fetch `wire_text`, judged on the file as it is on disk and
    /// before it is opened. The grammar is checked before anything is resolved, so a
    /// traversal never reaches the filesystem; the path is then resolved under the share
    /// root, refused when it resolved elsewhere, and judged against the rules. A file no
    /// rule names is served only on a grant, and a grant never lifts a deny; the grant is
    /// previewed here, not spent. The file is then stat'ed: one that is not a regular
    /// file is `NotShared`, and one over `max_bytes` is `TooLarge`, both before any use
    /// is spent, so a grant for a file too large to carry keeps its use. Only then is the
    /// grant's use reserved, before the open, so two fetches racing for one use cannot
    /// both be served; it goes back when the open fails or the handle turns out larger
    /// than the stat said. Only a verdict of served opens the file, since opening a FIFO
    /// waits for a writer that never comes. Every error on the way is `NotShared`, the
    /// same answer an unshared file gets.
    pub(crate) fn is_served(
        &self,
        peer: &PeerRef<'_>,
        wire_text: &str,
        case_insensitive: bool,
        max_bytes: u64,
        grants: Option<(&GrantStore, SystemTime)>,
    ) -> Served {
        let wire = match WirePath::parse(wire_text) {
            Ok(wire) => wire,
            Err(invalid) => return Served::InvalidPath { rule: invalid.rule },
        };
        let candidate = self.locations.workspace_root.join(wire.to_relative_path());
        let Ok(canonical) = dunce::canonicalize(candidate) else {
            return Served::NotShared;
        };
        let rules = match self.rules(peer, case_insensitive) {
            Ok(rules) => rules,
            Err(err) => {
                debug!(
                    "Mesh share rules could not be built, so nothing is served: {}",
                    redact_hashes(&format!("{err}"))
                );
                return Served::NotShared;
            }
        };
        if rules.resolved(&canonical).is_none() {
            return Served::NotShared;
        }
        let grant = match rules.judge(&wire, &canonical) {
            Judgement::Denied => return Served::NotShared,
            Judgement::Allowed => None,
            Judgement::NotAllowed => {
                let Some((store, now)) = grants else {
                    return Served::NotShared;
                };
                match store.is_granted(peer, wire_text, now) {
                    Ok(true) => Some((store, now)),
                    Ok(false) => return Served::NotShared,
                    Err(err) => {
                        debug!(
                            "Mesh grant store could not be read, so no grant applies: {}",
                            redact_hashes(&format!("{err}"))
                        );
                        return Served::NotShared;
                    }
                }
            }
        };
        let Ok(metadata) = fs::metadata(&canonical) else {
            return Served::NotShared;
        };
        if !metadata.is_file() {
            return Served::NotShared;
        }
        if metadata.len() > max_bytes {
            return Served::TooLarge {
                size: metadata.len(),
            };
        }
        if let Some((store, now)) = grant {
            match store.consume(peer, wire_text, now) {
                Ok(true) => {}
                Ok(false) => return Served::NotShared,
                Err(err) => {
                    debug!(
                        "Mesh grant store could not be read, so no grant applies: {}",
                        redact_hashes(&format!("{err}"))
                    );
                    return Served::NotShared;
                }
            }
        }
        let refund = || {
            if let Some((store, now)) = grant
                && let Err(err) = store.refund(peer, wire_text, now)
            {
                debug!(
                    "Mesh grant use could not be refunded: {}",
                    redact_hashes(&format!("{err}"))
                );
            }
        };
        let Some((file, metadata)) = open_regular(&canonical) else {
            refund();
            return Served::NotShared;
        };
        let size = metadata.len();
        if size > max_bytes {
            refund();
            return Served::TooLarge { size };
        }
        let via = if grant.is_some() {
            Via::Grant
        } else {
            Via::Allow
        };
        debug!(
            "Mesh share served: {} ({} bytes)",
            short(&hex_lower(&Sha256::digest(
                canonical.as_os_str().as_encoded_bytes()
            ))),
            size
        );
        Served::File(ServedFile {
            file,
            #[cfg(test)]
            canonical,
            size,
            via,
        })
    }

    /// Whether the share rules alone already let `peer` fetch `wire_text`: the resolve,
    /// judge and regular-file half of `is_served`, without the grant store, the size
    /// rule or the open, so an access request for a path the rules serve can be answered
    /// without spending or touching anything. A directory an allow matches is not
    /// allowed, since a fetch of it would not be served. Every error reads as not
    /// allowed.
    pub(crate) fn is_allowed(
        &self,
        peer: &PeerRef<'_>,
        wire_text: &str,
        case_insensitive: bool,
    ) -> bool {
        let Ok(wire) = WirePath::parse(wire_text) else {
            return false;
        };
        let candidate = self.locations.workspace_root.join(wire.to_relative_path());
        let Ok(canonical) = dunce::canonicalize(candidate) else {
            return false;
        };
        if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file()) {
            return false;
        }
        let Ok(rules) = self.rules(peer, case_insensitive) else {
            return false;
        };
        rules.resolved(&canonical).is_some() && rules.judge(&wire, &canonical) == Judgement::Allowed
    }

    /// The files `peer` may fetch, one page at a time, sorted by wire path. The walk
    /// starts at the literal head of each allow pattern rather than at the root, so a
    /// share of `docs/**` never reads the rest of the tree, and it visits at most
    /// `walk_bound` entries before it stops and says so. Only the page returned is opened
    /// and hashed, so a listing of a large tree costs one walk, not one read of every
    /// file. Grants never appear: a listing is what the share set says, not what one peer
    /// was handed once. A set whose rules cannot be built lists nothing. `next` names the
    /// last entry returned, so a client resumes after what it saw; only when every
    /// candidate on a page fell off during hydration does it name the last candidate
    /// instead, so the client can still move past them.
    pub(crate) fn list(
        &self,
        peer: &PeerRef<'_>,
        prefix: Option<&str>,
        cursor: Option<&str>,
        case_insensitive: bool,
        walk_bound: usize,
    ) -> Listing {
        let rules = match self.rules(peer, case_insensitive) {
            Ok(rules) => rules,
            Err(err) => {
                debug!(
                    "Mesh share rules could not be built, so nothing is listed: {}",
                    redact_hashes(&format!("{err}"))
                );
                return Listing::default();
            }
        };
        let walk = self.walk_allows(&rules, peer, walk_bound);
        let truncated = walk.truncated;
        let mut candidates = walk.sorted();
        candidates.retain(|candidate| candidate.verdict == Verdict::Shared);
        if let Some(prefix) = prefix {
            candidates.retain(|candidate| candidate.path.starts_with(prefix));
        }
        let (page, next) = paginate(candidates, cursor, LIST_PAGE_SIZE);
        let entries = hydrate(page);
        let next = next.map(|cursor| {
            entries
                .last()
                .map_or(cursor, |last| list_cursor(&last.path))
        });
        debug!(
            "Mesh share listed: {} entries on this page, truncated: {truncated}",
            entries.len()
        );
        Listing {
            entries,
            truncated,
            next,
        }
    }

    /// The walk behind `list` and `resolve`: every regular file an allow for `peer`
    /// names, with its verdict, from the literal head of each allow pattern.
    fn walk_allows<'r>(
        &self,
        rules: &'r ShareRules,
        peer: &PeerRef<'_>,
        walk_bound: usize,
    ) -> Walk<'r> {
        let mut walk = Walk::new(rules, walk_bound, usize::MAX);
        walk.run_from(
            self.effective(peer)
                .iter()
                .map(|entry| entry.pattern.as_str()),
        );
        walk
    }

    pub(crate) fn write_target(&self, scope: WriteScope) -> Layer {
        write_target(self.workspace_exists, scope)
    }

    /// Validates `mutation`, writes the target file atomically and only then updates
    /// memory. A mutation the file already holds is `Ok` without a write, and `Applied`
    /// says which. Refused while poisoned, with the load's refusal, so a corrupt file is
    /// never replaced by a fresh one.
    pub(crate) fn apply(&mut self, mutation: Mutation, scope: WriteScope) -> Result<Applied> {
        if let Some(refusal) = &self.poisoned {
            bail!("{refusal} Nothing was written.");
        }
        let layer = self.write_target(scope);
        let (path, file) = match layer {
            Layer::Global => (&self.locations.global, &mut self.global),
            Layer::Workspace => (&self.locations.workspace, &mut self.workspace),
        };
        let mut next = file.clone();
        let changed = mutate(&mut next, path, &mutation)?;
        if changed {
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
        Ok(Applied {
            path: path.clone(),
            changed,
        })
    }
}

impl ShareSet {
    /// Every entry of both files as written, global first, in file order; empty while
    /// poisoned, like `effective`.
    pub(crate) fn entries(&self) -> Vec<RawEntry> {
        if self.poisoned.is_some() {
            return Vec::new();
        }
        [
            (Layer::Global, &self.global),
            (Layer::Workspace, &self.workspace),
        ]
        .into_iter()
        .flat_map(|(layer, file)| {
            let allow = file.allow.iter().map(|entry| RawKind::Allow {
                pattern: entry.pattern.clone(),
                peer: entry.peer.clone(),
            });
            let deny = file.deny.iter().map(|entry| RawKind::Deny {
                pattern: entry.pattern.clone(),
            });
            let overrides = file.overrides.iter().map(|entry| RawKind::Override {
                path: entry.path.clone(),
            });
            allow
                .chain(deny)
                .chain(overrides)
                .map(move |kind| RawEntry { layer, kind })
        })
        .collect()
    }

    pub(crate) fn locations(&self) -> &ShareLocations {
        &self.locations
    }

    /// Whether the workspace file was on disk at load or has been written since.
    pub(crate) fn workspace_exists(&self) -> bool {
        self.workspace_exists
    }

    /// The refusal a file earned at load, for the verbs to show; `None` when both read.
    pub(crate) fn refusal(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    /// `list` with the reasons: every regular file an allow for `peer` names, with the
    /// verdict a fetch of it would get, so the human sees which of their allows a deny or
    /// the built-in list holds back. A file no allow names is no entry at all. Nothing is
    /// opened or hashed. Sorted by wire path and cut at `cap`; a set whose rules cannot
    /// be built resolves nothing, and `refusal` says why when a file was refused.
    pub(crate) fn resolve(
        &self,
        peer: &PeerRef<'_>,
        case_insensitive: bool,
        walk_bound: usize,
        cap: usize,
    ) -> Resolved {
        let Ok(rules) = self.rules(peer, case_insensitive) else {
            return Resolved::default();
        };
        let walk = self.walk_allows(&rules, peer, walk_bound);
        let truncated = walk.truncated;
        let mut entries: Vec<ResolvedEntry> = walk
            .sorted()
            .into_iter()
            .map(|candidate| ResolvedEntry {
                path: candidate.path,
                verdict: candidate.verdict,
            })
            .collect();
        let capped = entries.len() > cap;
        entries.truncate(cap);
        Resolved {
            entries,
            truncated,
            capped,
        }
    }

    /// How many regular files under the share root `pattern` alone reaches, counting up
    /// to `cap` and no further, so a verb can ask whether the pattern a human is about to
    /// write is broad before it writes it. Independent of every allow and deny on disk;
    /// `.git` and the protected directories are never entered and a symlinked directory
    /// never followed, as in `list`. Fails only for a pattern the files would refuse or a
    /// root that does not resolve.
    pub(crate) fn count_matches(
        &self,
        pattern: &str,
        case_insensitive: bool,
        walk_bound: usize,
        cap: usize,
    ) -> Result<MatchCount> {
        validate_pattern(pattern)?;
        let rules = ShareRules {
            allow: compile([pattern], case_insensitive)?,
            deny: GlobSet::empty(),
            builtin: GlobSet::empty(),
            overrides: Vec::new(),
            case_insensitive,
            protected_dirs: protected_dirs(&self.locations),
            canonical_root: self.canonical_root()?,
        };
        let mut walk = Walk::new(&rules, walk_bound, cap);
        walk.run_from([pattern]);
        Ok(MatchCount {
            files: walk.candidates.len(),
            capped: walk.capped,
            truncated: walk.truncated,
        })
    }

    /// The first segment of `pattern` when it names a directory nothing may serve out of,
    /// so `allow` and `deny` refuse it before anything is walked or written: `.git`, or
    /// the workspace config directory under either name it goes by. The global config
    /// directory is not addressable by a relative pattern, so it is not checked here.
    pub(crate) fn protected_head(&self, pattern: &str) -> Option<String> {
        let head = pattern.split('/').next()?;
        let protected = head == ".git"
            || workspace_config_dir_names(&self.locations.workspace_config_dir_name)
                .contains(&head);
        protected.then(|| head.to_string())
    }

    /// Whether the built-in deny, with the workspace config directory, names this one
    /// relative file: judged on the text, and on what it resolves to under the root when
    /// it exists, so a link to a secret is caught as a fetch of it would be. This is what
    /// a forced allow of such a file has to be told about.
    pub(crate) fn builtin_denies(&self, path: &str, case_insensitive: bool) -> Result<bool> {
        let builtin = self.builtin(case_insensitive)?;
        if builtin.is_match(path) {
            return Ok(true);
        }
        let Ok(root) = self.canonical_root() else {
            return Ok(false);
        };
        let Ok(canonical) = dunce::canonicalize(root.join(path)) else {
            return Ok(false);
        };
        Ok(segments_under(&root, &canonical)
            .is_some_and(|segments| builtin.is_match(segments.join("/"))))
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

/// The compiled policy, applied after the caller has resolved `root/path` on disk. Deny
/// always wins: a protected directory first, which nothing lifts; then a user deny from
/// either layer; then the built-in deny unless a global override names this exact file;
/// and only then does an allow serve the file. Deny is judged on both the name the peer sent
/// and the path it resolved to under the share root, so no alias dodges one; allow and
/// override are judged on the resolved path alone, so a symlink serves only what an allow
/// names on disk. An override lifts the built-in layer alone and grants nothing.
pub(crate) struct ShareRules {
    allow: GlobSet,
    deny: GlobSet,
    builtin: GlobSet,
    overrides: Vec<String>,
    case_insensitive: bool,
    protected_dirs: Vec<PathBuf>,
    canonical_root: PathBuf,
}

impl ShareRules {
    #[cfg(test)]
    pub(crate) fn permits(&self, wire: &WirePath, canonical: &Path) -> bool {
        self.judge(wire, canonical) == Judgement::Allowed
    }

    /// `wire` is the path as the peer sent it; `canonical` is `dunce::canonicalize` of
    /// `root/wire`, which a symlink may have taken anywhere. A `std::fs::canonicalize`
    /// `\\?\` path on Windows fails the root prefix check. A candidate outside the share
    /// root, the root itself, or one whose resolved path is not UTF-8, is `Denied` like
    /// anything a deny names, so no grant reaches it either.
    pub(crate) fn judge(&self, wire: &WirePath, canonical: &Path) -> Judgement {
        match self.verdict(wire, canonical) {
            Verdict::Shared => Judgement::Allowed,
            Verdict::NotAllowed => Judgement::NotAllowed,
            Verdict::Protected | Verdict::Denied | Verdict::BuiltinDenied => Judgement::Denied,
        }
    }

    /// `judge` with the reason kept; the one place the order of the rules is written.
    pub(crate) fn verdict(&self, wire: &WirePath, canonical: &Path) -> Verdict {
        if self.is_protected(canonical) {
            return Verdict::Protected;
        }
        let Some(resolved) = self.resolved(canonical) else {
            return Verdict::Protected;
        };
        let alias = wire.segments().collect::<Vec<_>>().join("/");
        if self.deny.is_match(&alias) || self.deny.is_match(&resolved) {
            return Verdict::Denied;
        }
        if (self.builtin.is_match(&alias) || self.builtin.is_match(&resolved))
            && !self.overridden(&resolved)
        {
            return Verdict::BuiltinDenied;
        }
        if self.allow.is_match(&resolved) {
            Verdict::Shared
        } else {
            Verdict::NotAllowed
        }
    }

    /// Whether an allow names what `canonical` resolved to, whatever the denies say.
    fn allow_names(&self, canonical: &Path) -> bool {
        self.resolved(canonical)
            .is_some_and(|resolved| self.allow.is_match(&resolved))
    }

    fn is_protected(&self, canonical: &Path) -> bool {
        self.protected_dirs
            .iter()
            .any(|dir| canonical.starts_with(dir))
    }

    /// The `/`-separated path of `canonical` under the share root; `None` for a path that
    /// is not plain segments below it: the root itself is no file to serve, and `..` or
    /// a root here means the caller did not resolve it.
    fn resolved(&self, canonical: &Path) -> Option<String> {
        let segments = segments_under(&self.canonical_root, canonical)?;
        (!segments.is_empty()).then(|| segments.join("/"))
    }

    fn overridden(&self, resolved: &str) -> bool {
        self.overrides.iter().any(|path| {
            if self.case_insensitive {
                path.to_lowercase() == resolved.to_lowercase()
            } else {
                path == resolved
            }
        })
    }
}

/// Whether `root`'s filesystem folds case, learned by creating a file there and looking
/// for it under the other case. The answer is what `rules` and `is_served` take as
/// `case_insensitive`, so a caller that cannot learn it fails closed rather than guessing
/// `false`. The probe is the one file this module ever removes, and it refuses to touch a
/// file that was already there under its name.
pub(crate) fn probe_case_insensitive(root: &Path) -> Result<bool> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!(".coyote-case-probe-{}-{nanos}", std::process::id());
    let probe = root.join(&name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .context("Failed to create a case probe file under the share root")?;
    let folds = fs::symlink_metadata(root.join(name.to_ascii_uppercase())).is_ok();
    let _ = fs::remove_file(&probe);
    Ok(folds)
}

/// The directory walk behind `list`, `resolve` and `count_matches`: an explicit stack, a
/// budget of entries to visit, a cap on the files kept, and every regular file an allow
/// names, unopened and with its verdict, so one traversal serves a peer's listing and
/// the human's view of what the denies hold back. Symlinked directories are never
/// entered, since a link can take the walk anywhere and `**` is the user's promise about
/// this tree alone; a symlinked file is judged on what it resolves to, as a fetch of it
/// would be.
struct Walk<'a> {
    rules: &'a ShareRules,
    remaining: usize,
    truncated: bool,
    /// Files kept before the walk stops; `usize::MAX` keeps them all.
    cap: usize,
    capped: bool,
    candidates: Vec<Candidate>,
}

/// A file an allow names, by its wire path and where it resolved to, kept unopened until
/// `hydrate` reads the page it lands on, with the verdict a fetch of it would get.
struct Candidate {
    path: String,
    canonical: PathBuf,
    verdict: Verdict,
}

impl<'a> Walk<'a> {
    fn new(rules: &'a ShareRules, walk_bound: usize, cap: usize) -> Self {
        Self {
            rules,
            remaining: walk_bound,
            truncated: false,
            cap,
            capped: false,
            candidates: Vec::new(),
        }
    }

    /// Walks from the literal head of each pattern rather than from the root, so a share
    /// of `docs/**` never reads the rest of the tree.
    fn run_from<'p>(&mut self, patterns: impl IntoIterator<Item = &'p str>) {
        for start in start_dirs(patterns) {
            let start = start
                .split('/')
                .filter(|segment| !segment.is_empty())
                .fold(self.rules.canonical_root.clone(), |path, segment| {
                    path.join(segment)
                });
            // A pattern's literal head may itself pass through a link, and the walk only
            // refuses links it meets on the way down; so the start is resolved and must
            // still lie in the root, or there is nothing under it an allow could name.
            let Ok(start) = dunce::canonicalize(start) else {
                continue;
            };
            if !start.starts_with(&self.rules.canonical_root) || self.rules.is_protected(&start) {
                continue;
            }
            self.run(start);
        }
    }

    fn run(&mut self, start: PathBuf) {
        let mut stack = vec![start];
        while let Some(path) = stack.pop() {
            if self.capped {
                return;
            }
            if self.remaining == 0 {
                self.truncated = true;
                return;
            }
            self.remaining -= 1;
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if path.file_name().is_some_and(|name| name == ".git")
                    || self.rules.is_protected(&path)
                {
                    continue;
                }
                let Ok(children) = fs::read_dir(&path) else {
                    continue;
                };
                stack.extend(children.flatten().map(|child| child.path()));
            } else if metadata.is_file() || metadata.file_type().is_symlink() {
                self.file(&path);
            }
        }
    }

    /// Keeps `path` if its name under the root is a wire path and an allow names what it
    /// resolves to, a regular file. A name the grammar refuses is left out, since no peer
    /// could fetch it.
    fn file(&mut self, path: &Path) {
        let Some(wire_text) = self.wire_text(path) else {
            return;
        };
        let Ok(wire) = WirePath::parse(&wire_text) else {
            return;
        };
        let Ok(canonical) = dunce::canonicalize(path) else {
            return;
        };
        if !self.rules.allow_names(&canonical) {
            return;
        }
        if !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file()) {
            return;
        }
        let verdict = self.rules.verdict(&wire, &canonical);
        self.candidates.push(Candidate {
            path: wire_text,
            canonical,
            verdict,
        });
        self.capped = self.candidates.len() >= self.cap;
    }

    fn wire_text(&self, path: &Path) -> Option<String> {
        Some(segments_under(&self.rules.canonical_root, path)?.join("/"))
    }

    /// The files kept, by wire path, a path reached twice kept once.
    fn sorted(self) -> Vec<Candidate> {
        let mut candidates = self.candidates;
        candidates.sort_by(|left, right| left.path.cmp(&right.path));
        candidates.dedup_by(|left, right| left.path == right.path);
        candidates
    }
}

/// The plain segments of `path` below `root`; `None` when `path` is not under `root` or
/// has anything but UTF-8 normal segments there, since `..` or a root prefix means the
/// caller did not resolve it.
fn segments_under<'a>(root: &Path, path: &'a Path) -> Option<Vec<&'a str>> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .map(|component| match component {
            Component::Normal(segment) => segment.to_str(),
            _ => None,
        })
        .collect()
}

/// `canonical` opened, with the metadata of that very handle, or `None` for anything but
/// a regular file. The path is checked before the open because opening a FIFO waits for a
/// writer that never comes, and the handle after it so a swap between the two changes
/// nothing.
fn open_regular(canonical: &Path) -> Option<(fs::File, fs::Metadata)> {
    if !fs::metadata(canonical).ok()?.is_file() {
        return None;
    }
    let file = fs::File::open(canonical).ok()?;
    let metadata = file.metadata().ok()?;
    metadata.is_file().then_some((file, metadata))
}

/// Opens and hashes the one page a peer will see. A candidate that is no longer a regular
/// file, or cannot be read, is left off the page rather than failing it.
fn hydrate(page: Vec<Candidate>) -> Vec<Listed> {
    page.into_iter()
        .filter_map(|candidate| {
            let (file, metadata) = open_regular(&candidate.canonical)?;
            let sha256 = sha256_of(file).ok()?;
            Some(Listed {
                path: candidate.path,
                size: metadata.len(),
                sha256,
                mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
            })
        })
        .collect()
}

/// Streams `file` through the hasher rather than reading it whole, since a listing may
/// meet files far larger than anything the wire would carry.
fn sha256_of(file: fs::File) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    io::copy(&mut io::BufReader::new(file), &mut hasher)?;
    Ok(hasher.finalize().into())
}

/// Where the walk for each pattern begins: the longest run of leading segments with no
/// glob metacharacter, so `docs/**` starts at `docs` and `**` at the root. A start under
/// another start is dropped, since the outer walk reaches it.
fn start_dirs<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut starts: Vec<String> = patterns
        .into_iter()
        .map(|pattern| {
            pattern
                .split('/')
                .take_while(|segment| !segment.contains(GLOB_METACHARACTERS))
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    starts.sort();
    starts.dedup();
    let all = starts.clone();
    starts.retain(|start| {
        !all.iter().any(|outer| {
            outer != start
                && (outer.is_empty()
                    || start
                        .strip_prefix(outer.as_str())
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    });
    starts
}

/// The page of `candidates` after the one `cursor` ends, or the first page when there is no
/// cursor or none matches: a cursor from a listing that has since changed starts over
/// rather than skipping an unknown amount.
fn paginate(
    candidates: Vec<Candidate>,
    cursor: Option<&str>,
    page_size: usize,
) -> (Vec<Candidate>, Option<String>) {
    let from = cursor
        .and_then(|cursor| {
            candidates
                .iter()
                .position(|candidate| list_cursor(&candidate.path) == cursor)
        })
        .map_or(0, |at| at + 1);
    let mut page: Vec<Candidate> = candidates.into_iter().skip(from).collect();
    let next = if page.len() > page_size {
        page.truncate(page_size);
        page.last().map(|candidate| list_cursor(&candidate.path))
    } else {
        None
    };
    (page, next)
}

/// The cursor that names the page ending at `path`: a hash rather than the path itself,
/// so a cursor on the wire never carries a file name.
pub(crate) fn list_cursor(path: &str) -> String {
    hex_lower(&Sha256::digest(path.as_bytes()))[..32].to_string()
}

/// Every built-in pattern at the share root and at any depth below it, plus the workspace
/// config directory under each name it goes by, escaped so a name is matched literally.
fn builtin_deny_patterns(workspace_config_dir_name: &str) -> Vec<String> {
    BUILTIN_DENY
        .iter()
        .map(|pattern| (*pattern).to_string())
        .chain(
            workspace_config_dir_names(workspace_config_dir_name)
                .into_iter()
                .map(|name| format!("{}/**", globset::escape(name))),
        )
        .flat_map(|pattern| [format!("**/{pattern}"), pattern])
        .collect()
}

/// The runtime name and the literal default, which memory and the sbx mixin keep using
/// under an env override; one entry when they agree.
fn workspace_config_dir_names(workspace_config_dir_name: &str) -> Vec<&str> {
    let mut names = vec![workspace_config_dir_name, WORKSPACE_COYOTE_DIR_NAME];
    names.dedup();
    names
}

/// The directories nothing lifts, as they resolve on disk: the workspace config directory
/// under each name it goes by, since an absolute env override lands where a name glob
/// would miss, and the global config directory, which a share root above it would
/// otherwise serve, the mesh cache directory when the caller named one, and whatever else
/// `with_protected` added. A directory that does not resolve holds nothing to protect.
fn protected_dirs(locations: &ShareLocations) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = workspace_config_dir_names(&locations.workspace_config_dir_name)
        .into_iter()
        .map(|name| locations.workspace_root.join(name))
        .chain([locations.config_dir.clone()])
        .chain(locations.mesh_cache_dir.clone())
        .chain(locations.protected.iter().cloned())
        .filter_map(|dir| dunce::canonicalize(dir).ok())
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

fn glob(pattern: &str, case_insensitive: bool) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
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

/// Whether `peer` is the canonical lowercase 32-hex an allow entry's `peer` must be to
/// scope it to anyone; any other text scopes the entry to nobody.
pub(crate) fn is_canonical_peer(peer: &str) -> bool {
    canonical_hash(peer).as_deref() == Some(peer)
}

/// Whether `pattern` reaches the whole tree: its first segment is `**`, so `**`,
/// `**/*.md` and `**/x` are broad where `docs/**` is not.
pub(crate) fn is_broad_pattern(pattern: &str) -> bool {
    pattern.split('/').next() == Some("**")
}

/// Whether a root-relative name must not be offered as a completion for `allow` or
/// `deny`: `.git`, the workspace config directory under either name it goes by, or
/// anything the built-in deny matches by name. No I/O. The built-in set is this module's
/// own constants, so one that fails to compile is a bug and hides everything.
#[cfg(test)]
pub(crate) fn hidden_from_completion(relative: &str, workspace_config_dir_name: &str) -> bool {
    workspace_config_dir_names(workspace_config_dir_name).contains(&relative)
        || compile(
            builtin_deny_patterns(workspace_config_dir_name)
                .iter()
                .map(String::as_str),
            false,
        )
        .map_or(true, |builtin| builtin.is_match(relative))
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
            let before = (file.allow.len(), file.deny.len(), file.overrides.len());
            file.allow.retain(|entry| entry.pattern != *pattern);
            file.deny.retain(|entry| entry.pattern != *pattern);
            file.overrides.retain(|entry| entry.path != *pattern);
            if (file.allow.len(), file.deny.len(), file.overrides.len()) == before {
                bail!(
                    "No share entry in '{}' has the pattern `{pattern}`; nothing was changed.",
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

/// The teaching refusals for a pattern as the share files take it; the verbs run them
/// before anything is walked so the human hears the rule, not a glob error.
pub(crate) fn validate_pattern(pattern: &str) -> Result<()> {
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

pub(crate) fn validate_override(path: &str) -> Result<()> {
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
/// rather than an unknown-field error from whatever the newer layout added. A hand-edited
/// pattern or override path the write path would have refused is refused here too, so
/// neither is ever inert; a `peer` that is not canonical can only narrow an allow, so it
/// loads and matches nobody.
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
    let file: SharesFile = serde_yaml::from_str(text).with_context(|| {
        format!(
            "Mesh share list '{}' could not be parsed as version {SHARES_FILE_VERSION}. {}",
            path.display(),
            SHARES_FILE_REMEDY.sentence()
        )
    })?;
    validate_entries(&file).with_context(|| {
        format!(
            "Mesh share list '{}' has an entry to fix or remove. {}",
            path.display(),
            SHARES_FILE_REMEDY.sentence()
        )
    })?;
    Ok(file)
}

fn validate_entries(file: &SharesFile) -> Result<()> {
    let allow = file.allow.iter().map(|entry| entry.pattern.as_str());
    let deny = file.deny.iter().map(|entry| entry.pattern.as_str());
    allow.chain(deny).try_for_each(validate_pattern)?;
    file.overrides
        .iter()
        .try_for_each(|entry| validate_override(&entry.path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mesh_config::MAX_FETCH_FILE_BYTES;
    use crate::mesh::hex_lower;
    use crate::mesh::test_support::TempDir;
    use crate::testing::{EnvVarGuard, debug_snapshot, install_log_collector, warn_snapshot};
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
            Self::at(tmp, config_dir, root)
        }

        /// The global config dir inside the share root, as a REPL started from `$HOME`
        /// has it.
        fn enclosing(tag: &str) -> Self {
            let tmp = TempDir::new(tag);
            let root = tmp.path.join("workspace");
            Self::at(tmp, root.join("config"), root)
        }

        fn at(tmp: TempDir, config_dir: PathBuf, root: PathBuf) -> Self {
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

        /// Creates a symlink at `relative` under the share root pointing at `target` as
        /// written, and returns where the link resolved to.
        #[cfg(unix)]
        fn link(&self, relative: &str, target: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, &path).unwrap();
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

    fn wire(relative: &str) -> WirePath {
        WirePath::parse(relative).unwrap()
    }

    fn rules_for_anyone(set: &ShareSet) -> ShareRules {
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        set.rules(&peer, false).unwrap()
    }

    /// Whether `relative`, created as a plain file under the share root, is served to a
    /// peer nothing is scoped to.
    fn served(set: &ShareSet, fx: &Fixture, relative: &str) -> bool {
        rules_for_anyone(set).permits(&wire(relative), &fx.file(relative))
    }

    /// The destination every grant in these tests is for.
    fn anyone() -> (String, String) {
        (fake_hash(0x1a), fake_hash(0x2b))
    }

    fn fetch(set: &ShareSet, wire_text: &str) -> Served {
        fetch_with(set, wire_text, false, None)
    }

    fn fetch_with(
        set: &ShareSet,
        wire_text: &str,
        case_insensitive: bool,
        grants: Option<(&GrantStore, SystemTime)>,
    ) -> Served {
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        set.is_served(
            &peer,
            wire_text,
            case_insensitive,
            MAX_FETCH_FILE_BYTES,
            grants,
        )
    }

    fn is_not_shared(verdict: &Served) -> bool {
        matches!(verdict, Served::NotShared)
    }

    fn listing(set: &ShareSet, prefix: Option<&str>, walk_bound: usize) -> Listing {
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        set.list(&peer, prefix, None, false, walk_bound)
    }

    fn listed_paths(listing: &Listing) -> Vec<&str> {
        listing
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect()
    }

    fn candidate(path: &str) -> Candidate {
        Candidate {
            path: path.to_string(),
            canonical: PathBuf::new(),
            verdict: Verdict::Shared,
        }
    }

    fn grant_store(fx: &Fixture) -> GrantStore {
        GrantStore::new(&fx._tmp.path.join("cache"), "inst")
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)
    }

    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(made.success(), "{made}");
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
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
        fx.file("README.md");
        assert!(is_not_shared(&fetch(&set, "README.md")));
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
        fx.file("README.md");
        assert!(is_not_shared(&fetch(&set, "README.md")));
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
        fx.file("docs/a.md");
        assert!(is_not_shared(&fetch(&set, "docs/a.md")));
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
        let written = fx
            .load()
            .apply(allow("docs/**"), WriteScope::Auto)
            .unwrap()
            .path;
        assert_eq!(written, locations.global);
        assert!(locations.global.exists());
        assert!(!locations.workspace.exists());
        assert!(!locations.global.with_added_extension("tmp").exists());

        let fx = Fixture::new("shares-apply-auto-workspace");
        let locations = fx.locations();
        fx.write(Layer::Workspace, "version: 1\n");
        let written = fx
            .load()
            .apply(allow("docs/**"), WriteScope::Auto)
            .unwrap()
            .path;
        assert_eq!(written, locations.workspace);
        assert!(!locations.global.exists());

        let fx = Fixture::new("shares-apply-forced-global");
        let locations = fx.locations();
        fx.write(Layer::Workspace, "version: 1\n");
        let written = fx
            .load()
            .apply(allow("docs/**"), WriteScope::Global)
            .unwrap()
            .path;
        assert_eq!(written, locations.global);
        assert_eq!(
            fs::read_to_string(&locations.workspace).unwrap(),
            "version: 1\n"
        );

        let fx = Fixture::new("shares-apply-forced-workspace");
        let locations = fx.locations();
        let mut set = fx.load();
        let written = set
            .apply(allow("docs/**"), WriteScope::Workspace)
            .unwrap()
            .path;
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

    /// The workspace file travels with a cloned repository, so an override there must
    /// not be what lifts the deny on a secret.
    #[test]
    fn a_workspace_override_is_inert_and_only_a_global_one_lifts_the_builtin_deny() {
        let fx = Fixture::new("shares-override-workspace-inert");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(lift(".env.example"), WriteScope::Workspace)
            .unwrap();
        assert!(
            !served(&set, &fx, ".env.example"),
            "a workspace override lifts nothing"
        );

        set.apply(lift(".env.example"), WriteScope::Global).unwrap();
        assert!(
            served(&set, &fx, ".env.example"),
            "the same override in the global file lifts the built-in deny"
        );
        assert!(!served(&set, &fx, ".env"), "the override is for one file");
    }

    #[test]
    fn inert_overrides_lists_the_workspace_entries_and_nothing_while_poisoned() {
        let fx = Fixture::new("shares-inert-overrides");
        let mut set = fx.load();
        assert!(set.inert_overrides().is_empty());
        set.apply(lift(".env.example"), WriteScope::Global).unwrap();
        assert!(set.inert_overrides().is_empty(), "global overrides apply");
        set.apply(lift("id_rsa.pub"), WriteScope::Workspace)
            .unwrap();
        assert_eq!(
            set.inert_overrides(),
            [OverrideEntry {
                path: "id_rsa.pub".to_string(),
            }]
        );

        fx.write(Layer::Workspace, "version: 1\nallow: [\n");
        assert!(fx.load().inert_overrides().is_empty());
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

        let fx = Fixture::new("shares-builtin-git-file");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Auto).unwrap();
        for denied in [".git", "vendor/lib/.git"] {
            assert!(
                !served(&set, &fx, denied),
                "{denied} as a worktree or submodule file must not be served"
            );
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
        assert!(!rules.permits(&wire("by-identity/a.md"), &fx.file("by-identity/a.md")));
        assert!(rules.permits(&wire("public/a.md"), &fx.file("public/a.md")));
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
        let path = set.apply(allow("docs/**"), WriteScope::Auto).unwrap().path;
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
            .unwrap()
            .path;

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
    fn unshare_removes_a_deny_and_an_override_with_that_text_too() {
        let fx = Fixture::new("shares-unshare-any-kind");
        let mut set = fx.load();
        set.apply(deny("src/vault/*"), WriteScope::Auto).unwrap();
        set.apply(lift(".env.example"), WriteScope::Auto).unwrap();
        set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        let unshare = |pattern: &str| Mutation::Unshare {
            pattern: pattern.into(),
        };

        assert!(
            set.apply(unshare("src/vault/*"), WriteScope::Auto)
                .unwrap()
                .changed
        );
        assert!(
            set.apply(unshare(".env.example"), WriteScope::Auto)
                .unwrap()
                .changed
        );

        assert!(set.global.deny.is_empty());
        assert!(set.global.overrides.is_empty());
        assert_eq!(set.global.allow.len(), 1, "the allow is untouched");
        assert_eq!(fx.load().global, set.global);
        let err = set
            .apply(unshare("src/vault/*"), WriteScope::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("No share entry in '"), "{err}");
        assert!(
            err.ends_with("has the pattern `src/vault/*`; nothing was changed."),
            "{err}"
        );
    }

    #[test]
    fn apply_says_whether_the_file_changed() {
        let fx = Fixture::new("shares-applied");
        let mut set = fx.load();

        let first = set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        let again = set.apply(allow("docs/**"), WriteScope::Auto).unwrap();

        assert_eq!(first.path, fx.locations().global);
        assert!(first.changed);
        assert_eq!(again.path, first.path);
        assert!(!again.changed);
    }

    #[test]
    fn entries_lists_every_entry_of_both_files_global_first_and_nothing_while_poisoned() {
        let fx = Fixture::new("shares-entries");
        let peer = fake_hash(0x1a);
        let mut set = fx.load();
        set.apply(lift(".env.example"), WriteScope::Global).unwrap();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        set.apply(deny("docs/private/**"), WriteScope::Global)
            .unwrap();
        set.apply(
            Mutation::Allow {
                pattern: "src/**".into(),
                peer: Some(peer.clone()),
            },
            WriteScope::Workspace,
        )
        .unwrap();

        assert_eq!(
            set.entries(),
            [
                RawEntry {
                    layer: Layer::Global,
                    kind: RawKind::Allow {
                        pattern: "docs/**".into(),
                        peer: None,
                    },
                },
                RawEntry {
                    layer: Layer::Global,
                    kind: RawKind::Deny {
                        pattern: "docs/private/**".into(),
                    },
                },
                RawEntry {
                    layer: Layer::Global,
                    kind: RawKind::Override {
                        path: ".env.example".into(),
                    },
                },
                RawEntry {
                    layer: Layer::Workspace,
                    kind: RawKind::Allow {
                        pattern: "src/**".into(),
                        peer: Some(peer),
                    },
                },
            ]
        );
        fx.write(Layer::Global, "version: 1\nallow: [\n");
        assert!(fx.load().entries().is_empty());
    }

    #[test]
    fn the_read_accessors_say_where_the_files_are_and_what_the_load_found() {
        let fx = Fixture::new("shares-accessors");
        let set = fx.load();
        assert_eq!(*set.locations(), fx.locations());
        assert!(!set.workspace_exists());
        assert_eq!(set.refusal(), None);

        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Workspace).unwrap();
        assert!(set.workspace_exists(), "a write creates the workspace file");

        let corrupt = fx.write(Layer::Workspace, "version: 1\nallow: [\n");
        let set = fx.load();
        assert!(set.workspace_exists());
        let refusal = set.refusal().unwrap();
        assert!(
            refusal.contains(&corrupt.display().to_string()),
            "{refusal}"
        );
    }

    #[test]
    fn is_canonical_peer_accepts_lowercase_32_hex_alone() {
        assert!(is_canonical_peer(&fake_hash(0xab)));
        assert!(!is_canonical_peer(&fake_hash(0xab).to_uppercase()));
        assert!(!is_canonical_peer(&fake_hash(0xab)[..31]));
        assert!(!is_canonical_peer("alice"));
        assert!(!is_canonical_peer(""));
    }

    #[test]
    fn an_unscoped_peer_sees_only_the_peer_less_allow_entries() {
        let fx = Fixture::new("shares-unscoped");
        let mut set = fx.load();
        set.apply(
            Mutation::Allow {
                pattern: "private/**".into(),
                peer: Some(fake_hash(0x1a)),
            },
            WriteScope::Auto,
        )
        .unwrap();
        set.apply(allow("public/**"), WriteScope::Auto).unwrap();

        assert!(!same_hash(&fake_hash(0x1a), ""));
        assert_eq!(
            patterns(&set.effective(&PeerRef::unscoped())),
            ["public/**"]
        );
    }

    #[test]
    fn verdict_keeps_the_reason_judge_folds_into_denied() {
        let fx = Fixture::new("shares-verdict");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(deny("src/vault/*"), WriteScope::Global).unwrap();
        set.apply(lift(".env.example"), WriteScope::Global).unwrap();
        let table = [
            ("docs/a.md", Verdict::Shared, Judgement::Allowed),
            (".coyote/notes.md", Verdict::Protected, Judgement::Denied),
            ("src/vault/k", Verdict::Denied, Judgement::Denied),
            (".env", Verdict::BuiltinDenied, Judgement::Denied),
            (".env.example", Verdict::Shared, Judgement::Allowed),
        ];
        let files: Vec<PathBuf> = table
            .iter()
            .map(|(relative, ..)| fx.file(relative))
            .collect();
        // Built after the files, so the workspace config dir exists to be protected.
        let rules = rules_for_anyone(&set);

        for ((relative, verdict, judgement), canonical) in table.into_iter().zip(&files) {
            let canonical = canonical.as_path();
            assert_eq!(
                rules.verdict(&wire(relative), canonical),
                verdict,
                "{relative}"
            );
            assert_eq!(
                rules.judge(&wire(relative), canonical),
                judgement,
                "{relative}"
            );
        }
        let outside = fx.file("docs/b.md");
        let narrow = {
            let mut set = fx.load();
            set.apply(
                Mutation::Unshare {
                    pattern: "**".into(),
                },
                WriteScope::Global,
            )
            .unwrap();
            set.apply(allow("docs/a.md"), WriteScope::Global).unwrap();
            rules_for_anyone(&set)
        };
        assert_eq!(
            narrow.verdict(&wire("docs/b.md"), &outside),
            Verdict::NotAllowed
        );
        assert_eq!(
            narrow.judge(&wire("docs/b.md"), &outside),
            Judgement::NotAllowed
        );
    }

    /// `allow **` with a workspace deny on the vault: the human's view names the shared
    /// file and both held-back files with their reasons, while the directories the walk
    /// never enters contribute nothing.
    fn resolve_fixture(tag: &str) -> (Fixture, ShareSet) {
        let fx = Fixture::new(tag);
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(deny("src/vault/*"), WriteScope::Workspace)
            .unwrap();
        for path in ["docs/a.md", ".env", "src/vault/k", ".git/config"] {
            fx.file(path);
        }
        (fx, set)
    }

    #[test]
    fn resolve_marks_a_built_in_denied_and_a_denied_file_under_an_allow_everything_pattern() {
        let (_fx, set) = resolve_fixture("resolve-reasons");

        let resolved = set.resolve(
            &PeerRef::unscoped(),
            false,
            DEFAULT_LIST_WALK_BOUND,
            LIST_PAGE_SIZE,
        );

        assert_eq!(
            resolved.entries,
            [
                ResolvedEntry {
                    path: ".env".into(),
                    verdict: Verdict::BuiltinDenied,
                },
                ResolvedEntry {
                    path: "docs/a.md".into(),
                    verdict: Verdict::Shared,
                },
                ResolvedEntry {
                    path: "src/vault/k".into(),
                    verdict: Verdict::Denied,
                },
            ],
            "nothing under `.git/` or the workspace config dir"
        );
        assert!(!resolved.truncated);
        assert!(!resolved.capped);
    }

    #[test]
    fn resolve_cuts_at_the_cap_and_says_so() {
        let (_fx, set) = resolve_fixture("resolve-cap");

        let resolved = set.resolve(&PeerRef::unscoped(), false, DEFAULT_LIST_WALK_BOUND, 2);

        assert_eq!(
            resolved
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            [".env", "docs/a.md"]
        );
        assert!(resolved.capped);
    }

    #[test]
    fn resolve_reports_a_walk_cut_short_and_resolves_nothing_while_poisoned() {
        let (fx, set) = resolve_fixture("resolve-bound");

        assert!(
            set.resolve(&PeerRef::unscoped(), false, 2, LIST_PAGE_SIZE)
                .truncated
        );
        fx.write(Layer::Global, "version: 1\nallow: [\n");
        assert_eq!(
            fx.load().resolve(
                &PeerRef::unscoped(),
                false,
                DEFAULT_LIST_WALK_BOUND,
                LIST_PAGE_SIZE
            ),
            Resolved::default()
        );
    }

    #[test]
    fn listing_the_resolve_fixture_serves_only_the_shared_file() {
        let (_fx, set) = resolve_fixture("resolve-vs-list");

        let listing = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert_eq!(listed_paths(&listing), ["docs/a.md"]);
        assert!(!listing.truncated);
    }

    #[test]
    fn count_matches_counts_the_files_one_pattern_reaches_and_stops_at_the_cap() {
        let fx = Fixture::new("count-matches");
        let set = fx.load();
        for path in [
            "docs/a.md",
            "docs/b.md",
            "docs/deep/c.md",
            "src/x.rs",
            ".env",
        ] {
            fx.file(path);
        }

        let docs = set
            .count_matches("docs/**", false, DEFAULT_LIST_WALK_BOUND, LIST_PAGE_SIZE)
            .unwrap();
        let capped = set
            .count_matches("docs/**", false, DEFAULT_LIST_WALK_BOUND, 2)
            .unwrap();
        let everything = set
            .count_matches("**", false, DEFAULT_LIST_WALK_BOUND, LIST_PAGE_SIZE)
            .unwrap();

        assert_eq!(
            docs,
            MatchCount {
                files: 3,
                capped: false,
                truncated: false,
            }
        );
        assert_eq!(
            capped,
            MatchCount {
                files: 2,
                capped: true,
                truncated: false,
            }
        );
        assert_eq!(
            everything.files, 5,
            "the count is about the pattern alone, not the deny lists"
        );
        assert!(
            set.count_matches("docs/**", false, 2, LIST_PAGE_SIZE)
                .unwrap()
                .truncated
        );
        let err = set
            .count_matches("/docs/**", false, DEFAULT_LIST_WALK_BOUND, LIST_PAGE_SIZE)
            .unwrap_err()
            .to_string();
        assert!(err.contains("relative to the workspace root"), "{err}");
    }

    #[test]
    fn is_broad_pattern_is_true_only_when_the_first_segment_is_a_double_star() {
        for (pattern, broad) in [
            ("**", true),
            ("**/*.md", true),
            ("**/x", true),
            ("docs/**", false),
            ("*", false),
            ("*.md", false),
            ("docs/**/x.md", false),
        ] {
            assert_eq!(is_broad_pattern(pattern), broad, "{pattern}");
        }
    }

    #[test]
    fn protected_head_names_git_and_the_workspace_config_dir_at_the_first_segment_only() {
        let fx = Fixture::new("protected-head");
        let set = fx.load();
        let dir = WORKSPACE_COYOTE_DIR_NAME;

        assert_eq!(set.protected_head(".git"), Some(".git".into()));
        assert_eq!(set.protected_head(".git/**"), Some(".git".into()));
        assert_eq!(set.protected_head(dir), Some(dir.into()));
        assert_eq!(
            set.protected_head(&format!("{dir}/mesh-shares.yaml")),
            Some(dir.into())
        );
        assert_eq!(set.protected_head("docs/**"), None);
        assert_eq!(set.protected_head("**"), None);
        assert_eq!(
            set.protected_head("docs/.git/**"),
            None,
            "the built-in deny, not this check, covers a nested `.git`"
        );
    }

    #[test]
    fn builtin_denies_the_usual_secrets_git_and_the_workspace_config_dir_but_not_a_doc() {
        let fx = Fixture::new("builtin-denies");
        let set = fx.load();
        let in_config_dir = format!("{WORKSPACE_COYOTE_DIR_NAME}/x");

        for path in [
            ".env",
            "docs/.env.local",
            "a/b.pem",
            "id_rsa",
            ".git/config",
            in_config_dir.as_str(),
        ] {
            assert!(set.builtin_denies(path, false).unwrap(), "{path}");
        }
        assert!(!set.builtin_denies("docs/a.md", false).unwrap());
        assert!(set.builtin_denies("docs/.ENV", true).unwrap());
        assert!(!set.builtin_denies("docs/.ENV", false).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn builtin_denies_a_plain_name_that_resolves_to_a_secret() {
        let fx = Fixture::new("builtin-denies-link");
        let set = fx.load();
        fx.file(".env");
        fx.link("docs/settings", "../.env");

        assert!(set.builtin_denies("docs/settings", false).unwrap());
        assert!(!set.builtin_denies("docs/missing", false).unwrap());
    }

    #[test]
    fn hidden_from_completion_hides_git_the_config_dir_and_built_in_denied_names() {
        for (name, hidden) in [
            (".git", true),
            (".coyote", true),
            (".coyote-custom", true),
            (".env", true),
            (".env.local", true),
            ("x.pem", true),
            ("id_rsa", true),
            ("docs", false),
            ("README.md", false),
        ] {
            assert_eq!(
                hidden_from_completion(name, ".coyote-custom"),
                hidden,
                "{name}"
            );
        }
    }

    #[test]
    fn case_insensitive_rules_match_across_case_and_so_does_the_override() {
        let policy = |fx: &Fixture| {
            let mut set = fx.load();
            set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
            set.apply(lift("docs/.Env.Example"), WriteScope::Auto)
                .unwrap();
            set
        };
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        let fx = Fixture::new("shares-case-lower");
        let set = policy(&fx);
        let exact = set.rules(&peer, false).unwrap();
        let folded = set.rules(&peer, true).unwrap();
        let x = fx.file("docs/x.md");
        assert!(exact.permits(&wire("docs/x.md"), &x));
        let example = fx.file("docs/.env.example");
        assert!(!exact.permits(&wire("docs/.env.example"), &example));
        assert!(folded.permits(&wire("docs/.env.example"), &example));
        let sample = fx.file("docs/.env.sample");
        assert!(!folded.permits(&wire("docs/.env.sample"), &sample));

        // `Docs` gets a root of its own so a case-folding filesystem cannot resolve it to
        // the `docs` above.
        let fx = Fixture::new("shares-case-upper");
        let set = policy(&fx);
        let exact = set.rules(&peer, false).unwrap();
        let folded = set.rules(&peer, true).unwrap();
        let x = fx.file("Docs/x.md");
        assert!(!exact.permits(&wire("Docs/x.md"), &x));
        assert!(folded.permits(&wire("Docs/x.md"), &x));
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
        assert!(!rules.permits(&wire(&relative), &inside));
        assert!(rules.permits(&wire("README.md"), &readme));
        assert!(
            rules.builtin.is_match(".coyote/sessions/notes.md"),
            "the name glob denies the directory on its own, before the prefix check"
        );
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
        let memory = fx.file(".coyote/memory/MEMORY.md");
        let readme = fx.file("README.md");
        let rules = set.rules(&peer, false).unwrap();
        assert_eq!(
            ShareLocations::new(&fx.config_dir, &fx.root).workspace,
            hidden.join("mesh-shares.yaml")
        );
        assert!(
            !rules.permits(&wire("hidden-cfg/sessions/notes.md"), &inside),
            "the resolved path, not the name, is what protects an overridden config dir"
        );
        assert!(
            !rules.permits(&wire(".coyote/memory/MEMORY.md"), &memory),
            "memory stays under the literal name whatever the override says"
        );
        assert!(rules.permits(&wire("README.md"), &readme));
        assert!(
            !rules.builtin.is_match("hidden-cfg/sessions/notes.md"),
            "the name glob cannot see an absolute override; the prefix check must"
        );
    }

    #[test]
    fn the_global_config_dir_is_never_served_when_the_share_root_encloses_it() {
        let fx = Fixture::enclosing("shares-protected-global");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        let shares = dunce::canonicalize(fx.locations().global).unwrap();
        let trust = fx.file("config/mesh/trust.yaml");
        let readme = fx.file("README.md");

        let rules = rules_for_anyone(&set);

        assert!(
            !rules.permits(&wire("config/mesh/shares.yaml"), &shares),
            "the share list itself must not be served"
        );
        assert!(
            !rules.permits(&wire("config/mesh/trust.yaml"), &trust),
            "the trust list must not be served"
        );
        assert!(rules.permits(&wire("README.md"), &readme));
        assert!(
            !rules.builtin.is_match("config/mesh/trust.yaml"),
            "no name glob knows the global config dir; the prefix check must"
        );
    }

    /// A share root above the cache dir, as a REPL started from `$HOME` has it: the grants
    /// file and a staged inbox file under `<cache>/mesh` are neither served nor listed,
    /// while the same `allow **` still serves the rest of the tree.
    #[test]
    fn a_file_under_the_mesh_cache_dir_is_never_served_or_listed() {
        let fx = Fixture::new("shares-protected-cache");
        let cache_dir = fx.root.join("cache");
        fx.write(Layer::Global, "version: 1\nallow:\n- pattern: '**'\n");
        let set = ShareSet::load(fx.locations().with_cache_dir(&cache_dir));
        fx.file("cache/mesh/grants-inst.jsonl");
        fx.file("cache/mesh/inbox/inst/0123abcd/docs/a.md");
        fx.file("README.md");
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        for hidden in [
            "cache/mesh/grants-inst.jsonl",
            "cache/mesh/inbox/inst/0123abcd/docs/a.md",
        ] {
            assert!(
                is_not_shared(&set.is_served(&peer, hidden, false, MAX_FETCH_FILE_BYTES, None)),
                "{hidden}"
            );
        }
        assert!(matches!(
            set.is_served(&peer, "README.md", false, MAX_FETCH_FILE_BYTES, None),
            Served::File(_)
        ));
        let listing = set.list(&peer, None, None, false, DEFAULT_LIST_WALK_BOUND);
        assert_eq!(listed_paths(&listing), ["README.md"]);
        assert!(!listing.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_inside_the_root_cannot_reach_a_built_in_denied_file() {
        for (tag, pattern) in [
            ("shares-alias-builtin-all", "**"),
            ("shares-alias-builtin-public", "public/**"),
        ] {
            let fx = Fixture::new(tag);
            fx.file(".env");
            let link = fx.link("public/link", "../.env");
            let mut set = fx.load();
            set.apply(allow(pattern), WriteScope::Auto).unwrap();
            assert!(
                !rules_for_anyone(&set).permits(&wire("public/link"), &link),
                "{tag}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_allow_is_judged_on_the_resolved_file_not_the_alias() {
        let fx = Fixture::new("shares-alias-allow");
        fx.file("src/x.rs");
        let link = fx.link("docs/link", "../src/x.rs");

        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        assert!(
            !rules_for_anyone(&set).permits(&wire("docs/link"), &link),
            "the alias is under docs/ but the file is not"
        );

        let fx = Fixture::new("shares-alias-allow-resolved");
        fx.file("src/x.rs");
        let link = fx.link("docs/link", "../src/x.rs");
        let mut set = fx.load();
        set.apply(allow("src/**"), WriteScope::Auto).unwrap();
        assert!(
            rules_for_anyone(&set).permits(&wire("docs/link"), &link),
            "the file is under src/, so the alias serves it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_user_deny_on_the_resolved_file_holds_through_an_alias() {
        let fx = Fixture::new("shares-alias-deny");
        fx.file("src/secret.md");
        let link = fx.link("pub/s", "../src/secret.md");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Auto).unwrap();
        set.apply(deny("src/secret.md"), WriteScope::Auto).unwrap();

        assert!(!rules_for_anyone(&set).permits(&wire("pub/s"), &link));
        assert!(served(&set, &fx, "src/other.md"));
    }

    #[test]
    fn a_candidate_outside_the_canonical_root_is_never_served() {
        let fx = Fixture::new("shares-outside-root");
        let outside = fx._tmp.path.join("outside.md");
        fs::write(&outside, "outside").unwrap();
        let outside = dunce::canonicalize(outside).unwrap();
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Auto).unwrap();

        let rules = rules_for_anyone(&set);

        assert!(!rules.permits(&wire("outside.md"), &outside));
        assert!(rules.permits(&wire("README.md"), &fx.file("README.md")));
    }

    #[test]
    fn a_share_root_that_does_not_resolve_fails_closed_without_naming_it() {
        let fx = Fixture::new("shares-no-root");
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Auto).unwrap();
        fs::remove_dir_all(&fx.root).unwrap();

        let err = set
            .rules(&peer, false)
            .err()
            .expect("a share root that is gone must fail closed");
        let err = format!("{err:#}");

        assert!(err.contains("the share root"), "{err}");
        assert!(!err.contains(&fx.root.display().to_string()), "{err}");
    }

    #[test]
    fn the_module_never_reads_the_current_directory() {
        let source = include_str!("shares.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];

        for needle in [
            ["current", "_dir"].concat(),
            ["workspace_config", "_dir()"].concat(),
        ] {
            assert!(
                !production.contains(&needle),
                "production code uses {needle}"
            );
        }
    }

    #[test]
    fn a_hand_edited_absolute_or_dotted_pattern_poisons_the_set_at_load() {
        install_log_collector();
        let table = [
            (
                "shares-load-absolute",
                "/secrets/**",
                "version: 1\ndeny:\n- pattern: /secrets/**\n",
                "relative to the workspace root",
            ),
            (
                "shares-load-dotted",
                "docs/../**",
                "version: 1\nallow:\n- pattern: docs/../**\n",
                "`..` segment",
            ),
            (
                "shares-load-override-glob",
                "docs/*",
                "version: 1\noverride:\n- path: docs/*\n",
                "is a pattern",
            ),
            (
                "shares-load-override-absolute",
                "/etc/.env",
                "version: 1\noverride:\n- path: /etc/.env\n",
                "names one exact file",
            ),
        ];
        for (tag, offending, text, teaching) in table {
            let fx = Fixture::new(tag);
            let path = fx.write(Layer::Global, text);
            fx.write(Layer::Workspace, "version: 1\nallow:\n- pattern: '**'\n");

            let mut set = fx.load();

            let refusal = set.poisoned.clone().unwrap();
            assert!(
                refusal.starts_with(&format!("Mesh share list '{}'", path.display())),
                "{tag}: {refusal}"
            );
            assert!(refusal.contains(teaching), "{tag}: {refusal}");
            fx.file("README.md");
            assert!(is_not_shared(&fetch(&set, "README.md")), "{tag}");
            let err = set
                .apply(allow("src/**"), WriteScope::Auto)
                .unwrap_err()
                .to_string();
            assert!(err.contains(teaching), "{tag}: {err}");
            assert!(err.contains(offending), "{tag}: {err}");
            let warned: Vec<String> = warn_snapshot()
                .into_iter()
                .filter(|line| line.contains(&path.display().to_string()))
                .collect();
            assert_eq!(warned.len(), 1, "{tag}: {warned:#?}");
            assert!(
                warned[0].contains("entry to fix or remove"),
                "{tag}: {warned:#?}"
            );
            assert!(
                warned[0].contains("move the file aside"),
                "{tag}: {warned:#?}"
            );
            assert!(
                !warned[0].contains(offending),
                "{tag}: the log must not carry the hand-edited text: {warned:#?}"
            );
            let fetch_lines: Vec<String> = debug_snapshot()
                .into_iter()
                .filter(|line| line.contains("nothing is served"))
                .collect();
            assert!(
                !fetch_lines.is_empty(),
                "{tag}: the refused fetch is logged"
            );
            for line in &fetch_lines {
                assert!(!line.contains(offending), "{tag}: {line}");
            }
        }
    }

    #[test]
    fn a_config_dir_name_with_glob_metacharacters_is_still_denied_by_name() {
        let fx = Fixture::new("shares-protected-metacharacters");
        let mut set = ShareSet::load(ShareLocations::with_dir_name(
            &fx.config_dir,
            &fx.root,
            "cfg[dev]".to_string(),
        ));
        set.apply(allow("**"), WriteScope::Global).unwrap();
        let inside = fx.file("cfg[dev]/sessions/x.md");

        let rules = rules_for_anyone(&set);

        assert!(!rules.permits(&wire("cfg[dev]/sessions/x.md"), &inside));
        assert!(rules.builtin.is_match("cfg[dev]/sessions/x.md"));
        assert!(rules.permits(&wire("README.md"), &fx.file("README.md")));
        compile(
            builtin_deny_patterns("cfg[").iter().map(String::as_str),
            false,
        )
        .expect("an unclosed bracket in the name must not break every share");
    }

    #[test]
    fn a_candidate_that_is_not_canonical_is_refused_rather_than_matched() {
        let fx = Fixture::new("shares-dotted-candidate");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Auto).unwrap();
        let readme = fx.file("README.md");
        // Built on the canonical root so the `..` alone is what refuses it.
        let dotted = readme
            .parent()
            .unwrap()
            .join("docs")
            .join("..")
            .join("README.md");

        let rules = rules_for_anyone(&set);

        assert!(!rules.permits(&wire("README.md"), &dotted));
        let root = readme.parent().unwrap();
        assert!(
            !rules.permits(&wire("docs"), root),
            "the share root itself is no file to serve"
        );
        assert!(rules.permits(&wire("README.md"), &readme));
    }

    #[test]
    fn mutation_logs_name_the_share_file_but_never_a_pattern_or_override_path() {
        install_log_collector();
        let fx = Fixture::new("shares-mutation-logs");
        let mut set = fx.load();

        set.apply(allow("docs/**"), WriteScope::Auto).unwrap();
        set.apply(deny("src/vault/*"), WriteScope::Auto).unwrap();
        set.apply(lift(".env.example"), WriteScope::Auto).unwrap();

        let path = fx.locations().global.display().to_string();
        let logged: Vec<String> = debug_snapshot()
            .into_iter()
            .chain(warn_snapshot())
            .filter(|line| line.contains(&path))
            .collect();
        assert_eq!(logged.len(), 3, "{logged:#?}");
        for line in &logged {
            for text in ["docs/**", "src/vault/*", ".env.example"] {
                assert!(!line.contains(text), "{line}");
            }
        }
    }

    #[test]
    fn an_invalid_wire_path_is_refused_before_the_filesystem_is_touched() {
        let fx = Fixture::new("serve-invalid-first");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fs::remove_dir_all(&fx.root).unwrap();

        for (text, rule) in [
            ("../../.bashrc", "segment"),
            ("docs\\a.md", "backslash"),
            ("/etc/passwd", "leading_slash"),
            ("", "empty"),
        ] {
            let verdict = fetch(&set, text);
            assert!(
                matches!(verdict, Served::InvalidPath { rule: broken } if broken == rule),
                "{text:?}: {verdict:?}"
            );
        }
        assert!(!fx.root.exists(), "the refusal must not create the root");
    }

    #[test]
    fn a_path_with_a_nul_byte_is_an_invalid_path() {
        let fx = Fixture::new("serve-nul");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");

        let verdict = fetch(&set, "docs/a\0.md");

        assert!(
            matches!(verdict, Served::InvalidPath { rule: "control" }),
            "{verdict:?}"
        );
    }

    #[test]
    fn a_decomposed_name_is_an_invalid_path() {
        let fx = Fixture::new("serve-nfc");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("docs/\u{e9}.md");

        let verdict = fetch(&set, "docs/e\u{301}.md");

        assert!(
            matches!(verdict, Served::InvalidPath { rule: "nfc" }),
            "{verdict:?}"
        );
        assert!(matches!(fetch(&set, "docs/\u{e9}.md"), Served::File(_)));
    }

    #[test]
    fn a_nonexistent_file_and_an_unshared_file_are_both_not_shared() {
        let fx = Fixture::new("serve-not-shared");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        let canonical = fx.file("docs/a.md");
        fx.file("src/x.rs");

        let missing = fetch(&set, "docs/missing.md");
        let unshared = fetch(&set, "src/x.rs");
        let served = fetch(&set, "docs/a.md");

        assert!(is_not_shared(&missing), "{missing:?}");
        assert!(is_not_shared(&unshared), "{unshared:?}");
        assert_eq!(format!("{missing:?}"), format!("{unshared:?}"));
        let Served::File(file) = served else {
            panic!("{served:?}");
        };
        assert_eq!(file.canonical, canonical);
        assert_eq!(file.size, "docs/a.md".len() as u64);
        assert_eq!(file.file.metadata().unwrap().len(), file.size);
        assert_eq!(file.via, Via::Allow);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_leaves_the_root_is_not_shared() {
        let fx = Fixture::new("serve-escape");
        let outside = fx._tmp.path.join("outside.md");
        fs::write(&outside, "outside").unwrap();
        fx.link("docs/escape", "../../outside.md");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();

        let verdict = fetch(&set, "docs/escape");

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    #[test]
    fn a_directory_under_an_allow_is_not_shared() {
        let fx = Fixture::new("serve-directory");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");

        let verdict = fetch(&set, "docs");

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    fn allowed_to_anyone(set: &ShareSet, wire_text: &str) -> bool {
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        set.is_allowed(&peer, wire_text, false)
    }

    #[test]
    fn is_allowed_answers_yes_only_for_a_regular_file_the_rules_serve() {
        let fx = Fixture::new("allowed-file");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(deny("docs/private.md"), WriteScope::Global)
            .unwrap();
        fx.file("docs/a.md");
        fx.file("docs/private.md");

        assert!(allowed_to_anyone(&set, "docs/a.md"));
        assert!(!allowed_to_anyone(&set, "docs/private.md"));
        assert!(!allowed_to_anyone(&set, "docs/missing.md"));
        assert!(
            !allowed_to_anyone(&set, "docs"),
            "a directory is never allowed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn is_allowed_refuses_a_path_that_leaves_the_root_through_a_symlink() {
        let fx = Fixture::new("allowed-escape");
        let outside = fx._tmp.path.join("outside.md");
        fs::write(&outside, "outside").unwrap();
        fx.link("docs/escape", "../../outside.md");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();

        assert!(!allowed_to_anyone(&set, "docs/escape"));
    }

    #[test]
    fn is_allowed_holds_a_peer_scoped_allow_to_that_peer() {
        let fx = Fixture::new("allowed-scoped");
        let (identity, destination, other) = (fake_hash(0x1a), fake_hash(0x2b), fake_hash(0x3c));
        let mut set = fx.load();
        set.apply(
            Mutation::Allow {
                pattern: "docs/a.md".into(),
                peer: Some(other.clone()),
            },
            WriteScope::Global,
        )
        .unwrap();
        fx.file("docs/a.md");
        let requester = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        let scoped = PeerRef {
            identity: &other,
            destination: &destination,
        };

        assert!(!set.is_allowed(&requester, "docs/a.md", false));
        assert!(set.is_allowed(&scoped, "docs/a.md", false));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_reached_through_an_escaping_link_is_refused_at_the_root_boundary() {
        let fx = Fixture::new("serve-fifo-escape");
        let fifo = fx._tmp.path.join("outside.fifo");
        mkfifo(&fifo);
        fx.link("docs/pipe", "../../outside.fifo");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();

        let verdict = fetch(&set, "docs/pipe");

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    /// Opening a FIFO waits for a writer, so a fetch that opened before it judged the
    /// kind of file would hang here; the test finishing is the proof of the order.
    #[cfg(unix)]
    #[test]
    fn a_fifo_inside_the_root_under_an_allow_is_refused_before_it_is_opened() {
        let fx = Fixture::new("serve-fifo-inside");
        fs::create_dir_all(fx.root.join("docs")).unwrap();
        mkfifo(&fx.root.join("docs/pipe"));
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();

        let verdict = fetch(&set, "docs/pipe");

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_inside_the_root_is_left_out_of_a_listing() {
        let fx = Fixture::new("list-fifo-inside");
        fx.file("docs/a.md");
        mkfifo(&fx.root.join("docs/pipe"));
        fx.link("docs/link", "pipe");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();

        let listing = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert_eq!(listed_paths(&listing), ["docs/a.md"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_under_an_allow_is_neither_served_nor_listed() {
        let fx = Fixture::new("serve-socket");
        let _listener = std::os::unix::net::UnixListener::bind(fx.root.join("sock")).unwrap();
        fx.file("docs/a.md");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();

        let verdict = fetch(&set, "sock");
        let listed = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert!(is_not_shared(&verdict), "{verdict:?}");
        assert_eq!(
            listed
                .entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["docs/a.md"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_share_root_itself_is_not_shared_even_through_a_link() {
        let fx = Fixture::new("serve-root");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");
        fx.link("docs/root", "..");

        let verdict = fetch(&set, "docs/root");

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    /// The deny is spelled in lower case and the file lives under `Vault`, so the request
    /// matches the on-disk name on every filesystem and only the fold flag decides.
    #[test]
    fn a_case_flipped_name_cannot_dodge_a_deny_under_either_fold_flag() {
        let fx = Fixture::new("serve-case-flip");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(deny("src/vault/*"), WriteScope::Global).unwrap();
        fx.file("src/Vault/seed.txt");

        let folded = fetch_with(&set, "src/Vault/seed.txt", true, None);
        let exact = fetch_with(&set, "src/Vault/seed.txt", false, None);

        assert!(is_not_shared(&folded), "{folded:?}");
        assert!(matches!(exact, Served::File(_)), "{exact:?}");
    }

    #[test]
    fn a_file_whose_real_name_is_upper_case_is_served_until_a_deny_names_its_directory() {
        let fx = Fixture::new("serve-case-control");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("src/vault/Seed.txt");

        assert!(matches!(
            fetch_with(&set, "src/vault/Seed.txt", false, None),
            Served::File(_)
        ));
        set.apply(deny("src/vault/*"), WriteScope::Global).unwrap();
        let verdict = fetch_with(&set, "src/vault/Seed.txt", false, None);

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    #[test]
    fn a_granted_path_outside_every_allow_is_served() {
        let fx = Fixture::new("serve-grant-outside");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("src/x.rs");
        let store = grant_store(&fx);
        let now = now();

        assert!(is_not_shared(&fetch_with(
            &set,
            "src/x.rs",
            false,
            Some((&store, now))
        )));
        store
            .grant(
                "req",
                &fake_hash(0x2b),
                &["src/x.rs".to_string()],
                None,
                now,
            )
            .unwrap();

        let verdict = fetch_with(&set, "src/x.rs", false, Some((&store, now)));

        let Served::File(file) = verdict else {
            panic!("{verdict:?}");
        };
        assert_eq!(file.via, Via::Grant);
        assert!(
            is_not_shared(&fetch(&set, "src/x.rs")),
            "without the store the grant is not consulted"
        );
        let other = (fake_hash(0x3c), fake_hash(0x4d));
        let stranger = PeerRef {
            identity: &other.0,
            destination: &other.1,
        };
        assert!(is_not_shared(&set.is_served(
            &stranger,
            "src/x.rs",
            false,
            MAX_FETCH_FILE_BYTES,
            Some((&store, now))
        )));
    }

    #[test]
    fn a_granted_path_under_a_user_deny_is_not_served() {
        let fx = Fixture::new("serve-grant-denied");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        set.apply(deny("src/vault/**"), WriteScope::Global).unwrap();
        fx.file("src/vault/seed.txt");
        let store = grant_store(&fx);
        let now = now();
        store
            .grant(
                "req",
                &fake_hash(0x2b),
                &["src/vault/seed.txt".to_string()],
                None,
                now,
            )
            .unwrap();

        let verdict = fetch_with(&set, "src/vault/seed.txt", false, Some((&store, now)));

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    #[test]
    fn a_granted_builtin_denied_file_is_not_served() {
        let fx = Fixture::new("serve-grant-builtin");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file(".env");
        let store = grant_store(&fx);
        let now = now();
        store
            .grant("req", &fake_hash(0x2b), &[".env".to_string()], None, now)
            .unwrap();

        let verdict = fetch_with(&set, ".env", false, Some((&store, now)));

        assert!(is_not_shared(&verdict), "{verdict:?}");
    }

    #[test]
    fn an_unreadable_grant_store_grants_nothing() {
        install_log_collector();
        let fx = Fixture::new("serve-grant-unreadable");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("src/x.rs");
        let store = grant_store(&fx);
        let now = now();
        store
            .grant(
                "req",
                &fake_hash(0x2b),
                &["src/x.rs".to_string()],
                None,
                now,
            )
            .unwrap();
        fs::write(store.path(), "not json\n").unwrap();

        let verdict = fetch_with(&set, "src/x.rs", false, Some((&store, now)));

        assert!(is_not_shared(&verdict), "{verdict:?}");
        assert!(
            debug_snapshot()
                .iter()
                .any(|line| line.contains("no grant applies")),
            "{:#?}",
            debug_snapshot()
        );
        assert!(
            !warn_snapshot()
                .iter()
                .any(|line| line.contains("no grant applies")),
            "a peer's fetch is not a warning: {:#?}",
            warn_snapshot()
        );
    }

    #[test]
    fn three_granted_files_outside_every_allow_are_served_once_each_and_a_fourth_fetch_is_not() {
        let fx = Fixture::new("serve-grant-three");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        let granted = ["src/a.rs", "src/b.rs", "notes/c.txt"];
        for path in granted {
            fx.file(path);
        }
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        store
            .grant("req", &destination, &granted.map(str::to_string), None, now)
            .unwrap();

        for path in granted {
            let verdict = set.is_served(
                &peer,
                path,
                false,
                MAX_FETCH_FILE_BYTES,
                Some((&store, now)),
            );
            let Served::File(file) = verdict else {
                panic!("{path}: {verdict:?}");
            };
            assert_eq!(file.via, Via::Grant, "{path}");
        }

        for path in granted {
            let verdict = set.is_served(
                &peer,
                path,
                false,
                MAX_FETCH_FILE_BYTES,
                Some((&store, now)),
            );
            assert!(is_not_shared(&verdict), "{path}: {verdict:?}");
        }
    }

    #[test]
    fn concurrent_fetches_of_a_single_use_path_serve_exactly_one() {
        let fx = Fixture::new("serve-grant-race");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("src/x.rs");
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        store
            .grant("req", &destination, &["src/x.rs".to_string()], None, now)
            .unwrap();

        let verdicts: Vec<Served> = std::thread::scope(|scope| {
            let fetches: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        set.is_served(
                            &peer,
                            "src/x.rs",
                            false,
                            MAX_FETCH_FILE_BYTES,
                            Some((&store, now)),
                        )
                    })
                })
                .collect();
            fetches
                .into_iter()
                .map(|fetch| fetch.join().unwrap())
                .collect()
        });

        let served: Vec<&Served> = verdicts
            .iter()
            .filter(|verdict| matches!(verdict, Served::File(_)))
            .collect();
        assert_eq!(served.len(), 1, "{verdicts:?}");
        assert!(matches!(served[0], Served::File(file) if file.via == Via::Grant));
        assert_eq!(
            verdicts
                .iter()
                .filter(|verdict| is_not_shared(verdict))
                .count(),
            7
        );
        let records = store.list().unwrap();
        assert_eq!(records.len(), 1, "{records:#?}");
        assert_eq!(records[0].paths[0].path, "src/x.rs");
        assert_eq!(
            records[0].paths[0].uses_left, 0,
            "the one serve spent the one use and no loser refunded it"
        );
    }

    #[test]
    fn a_granted_path_that_is_not_a_regular_file_is_not_served_and_keeps_its_use() {
        let fx = Fixture::new("serve-grant-directory");
        let mut set = fx.load();
        set.apply(allow("src/**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        store
            .grant("req", &destination, &["docs".to_string()], None, now)
            .unwrap();

        let verdict = set.is_served(
            &peer,
            "docs",
            false,
            MAX_FETCH_FILE_BYTES,
            Some((&store, now)),
        );

        assert!(is_not_shared(&verdict), "{verdict:?}");
        assert!(
            store.is_granted(&peer, "docs", now).unwrap(),
            "nothing was spent on a path that is not a regular file"
        );
    }

    /// The use is spent before the open, so an open that then fails must give it back;
    /// a mode of `000` is the one way to make a regular file stat but not open. Root
    /// opens anything, so under root the test proves the serve instead.
    #[cfg(unix)]
    #[test]
    fn a_granted_file_that_fails_to_open_after_the_spend_gets_its_use_back() {
        let fx = Fixture::new("serve-grant-refund-on-open");
        let set = fx.load();
        let locked = fx.file("src/x.rs");
        set_mode(&locked, 0o000);
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        store
            .grant("req", &destination, &["src/x.rs".to_string()], None, now)
            .unwrap();

        let verdict = set.is_served(
            &peer,
            "src/x.rs",
            false,
            MAX_FETCH_FILE_BYTES,
            Some((&store, now)),
        );
        set_mode(&locked, 0o600);

        let records = store.list().unwrap();
        let uses_left = records[0].paths[0].uses_left;
        match verdict {
            Served::File(_) => assert_eq!(uses_left, 0, "root opened it, so the use is spent"),
            Served::NotShared => {
                assert_eq!(uses_left, 1, "the failed open gave the use back");
                assert!(store.is_granted(&peer, "src/x.rs", now).unwrap());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_file_over_the_limit_is_too_large_and_a_granted_one_keeps_its_use() {
        let fx = Fixture::new("serve-too-large");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");
        fx.file("src/x.rs");
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        store
            .grant("req", &destination, &["src/x.rs".to_string()], None, now)
            .unwrap();

        let allowed = set.is_served(&peer, "docs/a.md", false, 5, None);
        let granted = set.is_served(&peer, "src/x.rs", false, 5, Some((&store, now)));

        let Served::TooLarge { size } = allowed else {
            panic!("{allowed:?}");
        };
        assert_eq!(size, "docs/a.md".len() as u64);
        let Served::TooLarge { size } = granted else {
            panic!("{granted:?}");
        };
        assert_eq!(size, "src/x.rs".len() as u64);
        assert!(
            store.is_granted(&peer, "src/x.rs", now).unwrap(),
            "a fetch refused for size spends nothing"
        );
        assert!(matches!(
            set.is_served(&peer, "src/x.rs", false, size, Some((&store, now))),
            Served::File(ServedFile {
                via: Via::Grant,
                ..
            })
        ));
    }

    #[test]
    fn a_too_large_verdict_is_only_reached_for_a_file_the_peer_may_fetch() {
        let fx = Fixture::new("serve-too-large-unshared");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        set.apply(deny("docs/vault/**"), WriteScope::Global)
            .unwrap();
        fx.file("src/x.rs");
        fx.file("docs/vault/seed.txt");
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        for path in ["src/x.rs", "docs/vault/seed.txt"] {
            let verdict = set.is_served(&peer, path, false, 5, Some((&store, now)));
            assert!(is_not_shared(&verdict), "{path}: {verdict:?}");
        }
    }

    #[test]
    fn a_poisoned_set_serves_nothing_even_on_a_grant() {
        let fx = Fixture::new("serve-poisoned-grant");
        fx.write(Layer::Workspace, "version: 1\nallow: [\n");
        fx.file("src/x.rs");
        let set = fx.load();
        assert!(set.poisoned.is_some());
        let store = grant_store(&fx);
        let now = now();
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        store
            .grant("req", &destination, &["src/x.rs".to_string()], None, now)
            .unwrap();

        let verdict = set.is_served(
            &peer,
            "src/x.rs",
            false,
            MAX_FETCH_FILE_BYTES,
            Some((&store, now)),
        );

        assert!(is_not_shared(&verdict), "{verdict:?}");
        assert!(
            store.is_granted(&peer, "src/x.rs", now).unwrap(),
            "a refused set spends nothing"
        );
    }

    #[test]
    fn serving_a_file_logs_a_hash_prefix_and_size_but_never_the_path() {
        install_log_collector();
        let fx = Fixture::new("serve-log");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        // The size is the only thing that tells this serve apart from every other test's
        // in the shared log, so the name is long enough to make it unique.
        let relative = "docs/served-once-under-a-name-no-other-test-uses.md";
        let canonical = fx.file(relative);

        assert!(matches!(fetch(&set, relative), Served::File(_)));

        let size = format!("({} bytes)", relative.len());
        let logged: Vec<String> = debug_snapshot()
            .into_iter()
            .filter(|line| line.contains("Mesh share served") && line.ends_with(&size))
            .collect();
        assert_eq!(logged.len(), 1, "{logged:#?}");
        assert!(!logged[0].contains("served-once"), "{logged:#?}");
        assert!(
            !logged[0].contains(&canonical.display().to_string()),
            "{logged:#?}"
        );
    }

    #[test]
    fn the_case_probe_leaves_no_file_behind() {
        let fx = Fixture::new("probe-clean");
        fx.file("README.md");

        probe_case_insensitive(&fx.root).unwrap();

        let names: Vec<String> = fs::read_dir(&fx.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["README.md"]);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn the_case_probe_reports_a_folding_filesystem() {
        let fx = Fixture::new("probe-folding");
        assert!(probe_case_insensitive(&fx.root).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_case_probe_reports_a_case_sensitive_filesystem() {
        let fx = Fixture::new("probe-sensitive");
        assert!(!probe_case_insensitive(&fx.root).unwrap());
    }

    #[test]
    fn the_case_probe_fails_closed_when_the_root_cannot_take_a_file() {
        let fx = Fixture::new("probe-missing");
        fs::remove_dir_all(&fx.root).unwrap();

        let err = probe_case_insensitive(&fx.root).unwrap_err().to_string();

        assert!(err.contains("case probe"), "{err}");
        assert!(!fx.root.exists());
    }

    /// Ten files sit outside the allow and the bound admits four visits; a walk that
    /// began at the root would spend them there and report itself cut short.
    #[test]
    fn listing_walks_only_under_the_allow_patterns() {
        let fx = Fixture::new("list-start-dirs");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");
        for n in 0..10 {
            fx.file(&format!("secrets-elsewhere/{n}.md"));
        }

        let listing = listing(&set, None, 4);

        assert!(!listing.truncated);
        assert_eq!(listed_paths(&listing), ["docs/a.md"]);
        assert!(listing.next.is_none());
        assert_eq!(
            start_dirs(["docs/**", "docs/*.md", "docs/sub/**", "**", "src/a.rs"]),
            [""],
            "the root swallows every other start"
        );
        assert_eq!(
            start_dirs(["docs/**", "docs/sub/**", "docs2/*", "src/[ab]/**"]),
            ["docs", "docs2", "src"]
        );
    }

    #[test]
    fn listing_everything_visits_the_whole_tree_except_git_and_secrets() {
        let fx = Fixture::new("list-everything");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        for path in [
            "b.md",
            "a.md",
            "docs/c.md",
            ".git/config",
            ".env",
            "deep/er/d.rs",
        ] {
            fx.file(path);
        }

        let listing = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert_eq!(
            listed_paths(&listing),
            ["a.md", "b.md", "deep/er/d.rs", "docs/c.md"],
            "sorted bytewise, `.git` never entered, `.env` never listed"
        );
        assert!(!listing.truncated);
    }

    /// Absence from the listing cannot tell a walk that never enters `.git` and the
    /// workspace config dir from one that enters them and denies each file; the bound
    /// can. The legitimate tree costs six visits (the root, `a.md`, `docs`, `.git`,
    /// `.coyote`, `docs/c.md`); either hidden directory holds ten more, so a bound of
    /// eight is only met by a walk that descends neither.
    #[test]
    fn listing_never_descends_into_git_or_the_workspace_config_dir() {
        let fx = Fixture::new("list-no-descent");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("a.md");
        fx.file("docs/c.md");
        for n in 0..10 {
            fx.file(&format!(".git/objects/{n}"));
            fx.file(&format!("{WORKSPACE_COYOTE_DIR_NAME}/sessions/{n}.md"));
        }

        let listing = listing(&set, None, 8);

        assert!(
            !listing.truncated,
            "a walk that entered `.git` or `{WORKSPACE_COYOTE_DIR_NAME}` would have spent the bound there"
        );
        assert_eq!(listed_paths(&listing), ["a.md", "docs/c.md"]);
        assert!(listing.next.is_none());
    }

    /// A share root above the global config dir, as a REPL started from `$HOME` has it:
    /// nothing under the config dir is listed, and the bound shows the walk never went in.
    /// The legitimate tree costs three visits (the root, `README.md`, `config`); the config
    /// dir holds the share list, the trust list and ten more files below that.
    #[test]
    fn listing_never_descends_into_an_enclosed_global_config_dir() {
        let fx = Fixture::enclosing("list-protected-global");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("README.md");
        fx.file("config/mesh/trust.yaml");
        for n in 0..10 {
            fx.file(&format!("config/agents/{n}/config.yaml"));
        }
        assert!(
            fx.locations().global.starts_with(&fx.root),
            "the fixture must put the share list inside the root"
        );

        let listing = listing(&set, None, 5);

        assert!(
            !listing.truncated,
            "a walk that entered the global config dir would have spent the bound there"
        );
        assert_eq!(listed_paths(&listing), ["README.md"]);
        assert!(
            listed_paths(&listing)
                .iter()
                .all(|path| !path.starts_with("config/")),
            "{:?}",
            listed_paths(&listing)
        );
    }

    #[test]
    fn listing_entries_carry_a_streamed_hash_size_and_mtime() {
        let fx = Fixture::new("list-hash");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        let bytes: Vec<u8> = (0..(4 * 1024 * 1024 + 1))
            .map(|n| (n % 251) as u8)
            .collect();
        let path = fx.root.join("big.bin");
        fs::write(&path, &bytes).unwrap();

        let listing = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert_eq!(listing.entries.len(), 1);
        let entry = &listing.entries[0];
        assert_eq!(entry.path, "big.bin");
        assert_eq!(entry.size, bytes.len() as u64);
        assert_eq!(entry.sha256, <[u8; 32]>::from(Sha256::digest(&bytes)));
        assert_eq!(
            entry.mtime,
            fs::metadata(&path).unwrap().modified().unwrap()
        );
    }

    #[test]
    fn a_tiny_walk_bound_truncates_the_listing() {
        let fx = Fixture::new("list-bound");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        for path in ["a.md", "b.md", "c.md", "d.md"] {
            fx.file(path);
        }

        let bounded = listing(&set, None, 3);
        let whole = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert!(bounded.truncated);
        assert!(bounded.entries.len() <= 2, "{:?}", listed_paths(&bounded));
        assert!(!whole.truncated);
        assert_eq!(whole.entries.len(), 4);
        assert_eq!(DEFAULT_LIST_WALK_BOUND, 100_000);
        assert_eq!(LIST_PAGE_SIZE, 1_000);
    }

    /// `hydrate` drops what it cannot read, and `list` runs it on the returned page alone:
    /// a cursor on the unreadable `b.md` resolves to `[c.md]`, where a walk that hashed as
    /// it went would have lost `b.md`, failed to find the cursor and started over at
    /// `[a.md, c.md]`.
    #[cfg(unix)]
    #[test]
    fn a_listing_hashes_only_the_page_it_returns() {
        let fx = Fixture::new("list-page-hash");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        let names = ["a.md", "b.md", "c.md"];
        let files = names.map(|name| fx.file(name));
        set_mode(&files[1], 0o000);
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        let hydrated = hydrate(
            files
                .iter()
                .zip(names)
                .map(|(canonical, name)| Candidate {
                    path: name.to_string(),
                    canonical: canonical.clone(),
                    verdict: Verdict::Shared,
                })
                .collect(),
        );
        let after_b = set.list(
            &peer,
            None,
            Some(&list_cursor("b.md")),
            false,
            DEFAULT_LIST_WALK_BOUND,
        );
        let first = set.list(&peer, None, None, false, DEFAULT_LIST_WALK_BOUND);
        set_mode(&files[1], 0o644);

        assert_eq!(
            hydrated
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["a.md", "c.md"]
        );
        assert_eq!(
            hydrated[1].sha256,
            <[u8; 32]>::from(Sha256::digest(b"c.md"))
        );
        assert_eq!(listed_paths(&after_b), ["c.md"]);
        assert_eq!(after_b.entries[0].sha256, hydrated[1].sha256);
        assert_eq!(listed_paths(&first), ["a.md", "c.md"]);
        assert!(first.next.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn listing_never_follows_a_symlinked_directory() {
        let fx = Fixture::new("list-linked-dir");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("src/x.rs");
        fx.link("docs/linked", "../src");

        let listing = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        assert_eq!(listed_paths(&listing), ["src/x.rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_listed_only_where_it_resolves_inside_the_root_and_an_allow() {
        let fx = Fixture::new("list-linked-file-allowed");
        let mut set = fx.load();
        set.apply(allow("src/**"), WriteScope::Global).unwrap();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("src/x.rs");
        fx.link("docs/link", "../src/x.rs");
        let outside = fx._tmp.path.join("outside.md");
        fs::write(&outside, "outside").unwrap();
        fx.link("docs/escape", "../../outside.md");
        assert_eq!(
            listed_paths(&listing(&set, None, DEFAULT_LIST_WALK_BOUND)),
            ["docs/link", "src/x.rs"]
        );

        let fx = Fixture::new("list-linked-file-unallowed");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("src/x.rs");
        fx.link("docs/link", "../src/x.rs");
        assert_eq!(
            listed_paths(&listing(&set, None, DEFAULT_LIST_WALK_BOUND)),
            Vec::<&str>::new(),
            "the alias is under docs/ but the file is not"
        );
    }

    #[test]
    fn a_name_the_grammar_refuses_is_left_out_of_the_listing() {
        let fx = Fixture::new("list-nfc");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fx.file("docs/ok.md");
        fx.file("docs/e\u{301}.md");

        let listing = listing(&set, None, DEFAULT_LIST_WALK_BOUND);

        let paths = listed_paths(&listing);
        assert!(paths.contains(&"docs/ok.md"), "{paths:?}");
        assert!(
            paths.iter().all(|path| unicode_normalization::is_nfc(path)),
            "{paths:?}"
        );
        assert!(!paths.contains(&"docs/e\u{301}.md"), "{paths:?}");
    }

    #[test]
    fn a_prefix_keeps_only_the_paths_that_start_with_it() {
        let fx = Fixture::new("list-prefix");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        for path in ["docs/a.md", "docs/sub/b.md", "documents/c.md", "src/d.rs"] {
            fx.file(path);
        }

        assert_eq!(
            listed_paths(&listing(&set, Some("docs/"), DEFAULT_LIST_WALK_BOUND)),
            ["docs/a.md", "docs/sub/b.md"]
        );
        assert_eq!(
            listed_paths(&listing(&set, Some("doc"), DEFAULT_LIST_WALK_BOUND)),
            ["docs/a.md", "docs/sub/b.md", "documents/c.md"]
        );
        assert!(
            listing(&set, Some("../"), DEFAULT_LIST_WALK_BOUND)
                .entries
                .is_empty()
        );
    }

    #[test]
    fn a_granted_file_never_appears_in_a_listing() {
        let fx = Fixture::new("list-grant");
        let mut set = fx.load();
        set.apply(allow("docs/**"), WriteScope::Global).unwrap();
        fx.file("docs/a.md");
        fx.file("src/x.rs");
        let store = grant_store(&fx);
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        store
            .grant(
                "req",
                &fake_hash(0x2b),
                &["src/x.rs".to_string()],
                None,
                now,
            )
            .unwrap();

        assert!(matches!(
            fetch_with(&set, "src/x.rs", false, Some((&store, now))),
            Served::File(_)
        ));
        assert_eq!(
            listed_paths(&listing(&set, None, DEFAULT_LIST_WALK_BOUND)),
            ["docs/a.md"]
        );
    }

    #[test]
    fn a_poisoned_or_rootless_set_lists_nothing() {
        install_log_collector();
        let fx = Fixture::new("list-poisoned");
        fx.write(Layer::Global, "version: 1\nallow: [\n");
        fx.write(Layer::Workspace, "version: 1\nallow:\n- pattern: '**'\n");
        fx.file("docs/a.md");
        let set = fx.load();
        assert_eq!(
            listing(&set, None, DEFAULT_LIST_WALK_BOUND),
            Listing::default()
        );

        let fx = Fixture::new("list-rootless");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        fs::remove_dir_all(&fx.root).unwrap();
        assert_eq!(
            listing(&set, None, DEFAULT_LIST_WALK_BOUND),
            Listing::default()
        );
        assert!(
            debug_snapshot()
                .iter()
                .any(|line| line.contains("nothing is listed")),
            "{:#?}",
            debug_snapshot()
        );
    }

    #[test]
    fn a_list_cursor_is_thirty_two_lowercase_hex_characters() {
        let cursor = list_cursor("docs/a.md");

        assert_eq!(cursor.len(), 32);
        assert!(
            cursor
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{cursor}"
        );
        assert_eq!(cursor, list_cursor("docs/a.md"));
        assert_ne!(cursor, list_cursor("docs/b.md"));
        assert!(!cursor.contains("docs"));
    }

    /// A remote client computes the same cursor, so the digest is pinned to a value
    /// worked out independently: `printf 'docs/a.md' | sha256sum`, first 32 hex
    /// characters. Any other 128-bit digest of the path fails here.
    #[test]
    fn a_list_cursor_is_the_first_half_of_the_sha256_of_the_path() {
        assert_eq!(list_cursor("docs/a.md"), "5231f8a11b65145a1b0727cb8d209819");
        assert_eq!(
            hex_lower(&Sha256::digest(b"docs/a.md")),
            "5231f8a11b65145a1b0727cb8d209819\
             e95360a5f1e17e4f757b61dbda1af3cc",
            "the literal above is the first half of this full digest"
        );
    }

    #[test]
    fn paging_resumes_after_the_cursor_and_an_unknown_cursor_starts_over() {
        let entries = || {
            ["a", "b", "c", "d", "e"]
                .into_iter()
                .map(candidate)
                .collect::<Vec<Candidate>>()
        };
        let paths = |page: &[Candidate]| {
            page.iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        };

        let (first, next) = paginate(entries(), None, 2);
        assert_eq!(paths(&first), ["a", "b"]);
        assert_eq!(next.as_deref(), Some(list_cursor("b").as_str()));

        let (second, next) = paginate(entries(), next.as_deref(), 2);
        assert_eq!(paths(&second), ["c", "d"]);
        assert_eq!(next.as_deref(), Some(list_cursor("d").as_str()));

        let (last, next) = paginate(entries(), next.as_deref(), 2);
        assert_eq!(paths(&last), ["e"]);
        assert!(next.is_none());

        let (restarted, next) = paginate(entries(), Some("not-a-cursor"), 2);
        assert_eq!(paths(&restarted), ["a", "b"]);
        assert!(next.is_some());

        let (exact, next) = paginate(entries(), Some(&list_cursor("c")), 2);
        assert_eq!(paths(&exact), ["d", "e"]);
        assert!(next.is_none(), "a page that fills exactly has no next");

        let (whole, next) = paginate(entries(), None, LIST_PAGE_SIZE);
        assert_eq!(whole.len(), 5);
        assert!(next.is_none());
    }

    #[test]
    fn a_listing_pages_through_list_with_its_own_cursor() {
        let fx = Fixture::new("list-cursor");
        let mut set = fx.load();
        set.apply(allow("**"), WriteScope::Global).unwrap();
        for path in ["a.md", "b.md", "c.md"] {
            fx.file(path);
        }
        let (identity, destination) = anyone();
        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };

        let after_a = set.list(
            &peer,
            None,
            Some(&list_cursor("a.md")),
            false,
            DEFAULT_LIST_WALK_BOUND,
        );
        let unknown = set.list(&peer, None, Some("unknown"), false, DEFAULT_LIST_WALK_BOUND);

        assert_eq!(listed_paths(&after_a), ["b.md", "c.md"]);
        assert_eq!(listed_paths(&unknown), ["a.md", "b.md", "c.md"]);
        assert!(unknown.next.is_none());
    }
}
