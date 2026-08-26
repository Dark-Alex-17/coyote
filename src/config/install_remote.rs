use super::bundles::{
    BundleStore, FileAction, FileRecord, InstallMetadata, McpAction, McpServerRecord, hash_bytes,
    hash_file,
};
use crate::config::{AssetCategory, BUNDLE_MANIFEST_FILE, InstallFilter, paths};
#[cfg(not(windows))]
use crate::function::Language;
use crate::mcp::{McpServer, McpServersConfig};
use crate::utils;
use crate::utils::IS_STDOUT_TERMINAL;
use crate::vault::{SECRET_RE, Vault, create_vault_password_file, interpolate_secrets};
use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use indexmap::IndexMap;
use indoc::formatdoc;
use inquire::{Confirm, Select};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::{fs, iter};

pub fn install_remote(git_url: &str, filter: Option<InstallFilter>, force: bool) -> Result<()> {
    let (url, reference) = parse_url_with_ref(git_url)?;
    let temp = clone_to_temp(&url, reference.as_deref())?;
    println!("Cloned {git_url} to {}", temp.path().display());

    let mut layout = scan_remote_layout(temp.path())?;
    layout.head_sha = Some(temp.head_sha().to_string());
    let layout = apply_filter(layout, filter);

    if layout.is_empty() {
        println!(
            "No recognized assets found in {git_url}. Expected one or more of: \
             agents/, roles/, skills/, macros/, functions/tools/, mcp.json"
        );
        return Ok(());
    }

    let mut store = BundleStore::load()?;
    let bundle = register_bundle(
        &mut store,
        &url,
        reference.as_deref(),
        layout.manifest.as_ref(),
        temp.head_sha(),
        false,
    )?;

    let plan = plan_changes(&layout)?;
    let plan = reclassify_owned_unmodified(plan, &store, &bundle)?;

    if !plan.files.is_empty() {
        print_plan_summary(&plan);
        let sticky = if force {
            StickyMode::ReplaceAll
        } else {
            StickyMode::None
        };
        apply_plan(&plan, sticky, &mut store, &bundle)?;
    }

    if let Some((remote_mcp, local_mcp)) = &plan.mcp_json {
        let local = local_mcp.exists().then_some(local_mcp.as_path());
        let report = merge_mcp_json(local, remote_mcp, local_mcp, force, &HashSet::new(), false)?;
        record_mcp_merge(&mut store, &bundle, &report)?;
        print_mcp_merge_report(&report);
        handle_missing_secrets(&report.missing_secrets)?;
    }

    Ok(())
}

#[derive(Debug)]
struct ReplInstallArgs {
    value: Option<String>,
    filter: Option<InstallFilter>,
    force: bool,
    git_host: Option<String>,
}

/// Flags may appear before or after the positional value, matching how the
/// completer offers them at any argument position.
fn parse_repl_install_flags(
    command: &str,
    mut iter: impl Iterator<Item = String>,
) -> Result<ReplInstallArgs> {
    let mut value: Option<String> = None;
    let mut filter: Option<InstallFilter> = None;
    let mut force = false;
    let mut git_host: Option<String> = None;

    while let Some(tok) = iter.next() {
        match tok.as_str() {
            "--force" => force = true,
            "--filter" => {
                let val = iter.next().with_context(|| {
                    format!(
                        "--filter requires a value (one of: {})",
                        InstallFilter::NAMES.join(", ")
                    )
                })?;
                filter = Some(parse_filter(&val)?);
            }
            s if s.starts_with("--filter=") => {
                filter = Some(parse_filter(&s["--filter=".len()..])?);
            }
            "--git-host" => {
                let val = iter
                    .next()
                    .with_context(|| "--git-host requires a value (e.g. git.somedomain.com)")?;
                git_host = Some(val);
            }
            s if s.starts_with("--git-host=") => {
                git_host = Some(s["--git-host=".len()..].to_string());
            }
            other if other.starts_with('-') => {
                bail!("Unexpected argument to '{command}': {other}")
            }
            other => {
                if value.is_some() {
                    bail!("Unexpected argument to '{command}': {other}");
                }
                value = Some(other.to_string());
            }
        }
    }

    Ok(ReplInstallArgs {
        value,
        filter,
        force,
        git_host,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum InstallTarget {
    Category(AssetCategory),
    InstalledBundle,
    RemoteSource,
    Shorthand,
    Unknown,
}

fn classify_install_target(value: &str, installed_names: &[String]) -> InstallTarget {
    if let Some(category) = AssetCategory::parse(value) {
        return InstallTarget::Category(category);
    }

    let name = strip_ref_suffix(value);

    if installed_names.iter().any(|installed| installed == name) {
        return InstallTarget::InstalledBundle;
    }

    if looks_like_remote_source(value) {
        return InstallTarget::RemoteSource;
    }

    if is_repo_shorthand(value) {
        return InstallTarget::Shorthand;
    }

    InstallTarget::Unknown
}

fn looks_like_remote_source(value: &str) -> bool {
    if value.contains("://")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('/')
        || value.starts_with('~')
    {
        return true;
    }

    match value.split_once(':') {
        Some((host, _)) => {
            !host.is_empty() && !host.contains('/') && !host.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

pub const DEFAULT_GIT_HOST: &str = "github.com";

fn is_repo_shorthand(value: &str) -> bool {
    let path = strip_ref_suffix(value);
    if path.contains("://")
        || path.contains(':')
        || path.contains('\\')
        || path.contains(char::is_whitespace)
        || path.starts_with(['/', '~', '.', '-'])
    {
        return false;
    }

    let mut segments = path.split('/');
    segments.clone().count() >= 2 && segments.all(|segment| !segment.is_empty())
}

fn expand_repo_shorthand(value: &str, git_host: Option<&str>) -> Result<String> {
    let raw = git_host.unwrap_or(DEFAULT_GIT_HOST);
    let host = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw)
        .trim_matches('/');

    if host.is_empty() || host.contains(['/', '#', '?']) || host.chars().any(char::is_whitespace) {
        bail!("invalid --git-host '{raw}': expected a bare host like git.somedomain.com");
    }

    Ok(format!("https://{host}/{value}"))
}

pub fn install_or_update(
    value: &str,
    git_host: Option<&str>,
    filter: Option<InstallFilter>,
    force: bool,
) -> Result<()> {
    if let Some(host) = git_host {
        if !is_repo_shorthand(value) {
            bail!(
                "--git-host only applies to <owner>/<repo> shorthand values; \
                 '{value}' is not one"
            );
        }

        let url = expand_repo_shorthand(value, Some(host))?;
        println!("Resolved '{value}' to '{url}'");
        return install_remote(&url, filter, force);
    }

    let store = BundleStore::load()?;
    let installed: Vec<String> = store
        .bundle_names()
        .into_iter()
        .map(str::to_string)
        .collect();

    match classify_install_target(value, &installed) {
        InstallTarget::Category(category) => {
            let name = category
                .to_possible_value()
                .map_or_else(|| value.to_string(), |v| v.get_name().to_string());
            let update_hint = if installed.iter().any(|installed| installed == value) {
                format!(" To update the installed bundle '{value}', use `--update-bundle {value}`.")
            } else {
                String::new()
            };
            bail!(
                "'{value}' is an asset category, not a bundle; did you mean \
                 `--install-builtins {name}`? (categories are reinstalled from \
                 the assets built into coyote, not from a remote){update_hint}"
            );
        }
        InstallTarget::InstalledBundle => {
            if filter.is_some() || force {
                bail!("--filter/--install-force only apply to remote installs, not bundle updates");
            }
            update_bundle(value, false)
        }
        InstallTarget::RemoteSource => install_remote(value, filter, force),
        InstallTarget::Shorthand => {
            let url = expand_repo_shorthand(value, None)?;
            println!("Resolved '{value}' to '{url}'");
            install_remote(&url, filter, force)
        }
        InstallTarget::Unknown => {
            let hint = "a remote source must be a git URL, an <owner>/<repo> shorthand \
                 (expanded against --git-host, default github.com), an scp-style \
                 host:path, or an explicit local path (./dir, /abs, ~)";

            if installed.is_empty() {
                bail!("no bundle named '{value}' is installed; none are installed ({hint})");
            }

            bail!(
                "no bundle named '{value}' is installed; installed bundles: {} ({hint})",
                installed.join(", ")
            )
        }
    }
}

pub fn install_or_update_from_repl_args(args: &str) -> Result<()> {
    let tokens = shell_words::split(args)
        .with_context(|| format!("failed to parse '.install' args: {args}"))?;

    let parsed = parse_repl_install_flags(".install", tokens.into_iter())?;
    let value = parsed.value.with_context(|| {
        format!(
            "Usage: .install <git-url|owner/repo|installed-bundle> \
             [--git-host <host>] [--filter <{}>] [--force]",
            InstallFilter::NAMES.join("|")
        )
    })?;
    install_or_update(
        &value,
        parsed.git_host.as_deref(),
        parsed.filter,
        parsed.force,
    )
}

/// The whole remote is always processed, including categories a filtered
/// install excluded, because filtered installs merge into a single record.
pub fn update_bundle(spec: &str, assume_yes: bool) -> Result<()> {
    let (name, ref_override) = parse_url_with_ref(spec)?;

    let mut store = BundleStore::load()?;
    let Some(record) = store.get(&name) else {
        let installed = store.bundle_names();
        if installed.is_empty() {
            bail!("no bundle named '{name}' is installed; none are installed");
        }
        bail!(
            "no bundle named '{name}' is installed; installed bundles: {}",
            installed.join(", ")
        );
    };
    let source = record.source.clone();
    let recorded_ref = record.git_ref.clone();

    let has_override = ref_override.is_some();
    let effective_ref = ref_override.or(recorded_ref);
    if !has_override
        && let Some(pinned) = effective_ref.as_deref()
        && is_commit_sha(pinned)
    {
        println!("Bundle '{name}' is pinned to commit {pinned}; pass #<ref> to move the pin.");
    }

    let temp = clone_to_temp(&source, effective_ref.as_deref())?;
    println!("Cloned {source} to {}", temp.path().display());

    let mut layout = scan_remote_layout(temp.path())?;
    layout.head_sha = Some(temp.head_sha().to_string());
    if layout.is_empty() {
        println!(
            "The source for '{name}' no longer contains recognized assets; \
             leaving installed files and the bundle record untouched."
        );
        return Ok(());
    }

    let bundle = register_bundle(
        &mut store,
        &source,
        effective_ref.as_deref(),
        layout.manifest.as_ref(),
        temp.head_sha(),
        true,
    )?;

    let plan = plan_changes(&layout)?;
    let plan = reclassify_owned_unmodified(plan, &store, &bundle)?;

    if !plan.files.is_empty() {
        print_plan_summary(&plan);
        let sticky = if assume_yes {
            StickyMode::KeepAll
        } else {
            StickyMode::None
        };
        apply_plan(&plan, sticky, &mut store, &bundle)?;
    }

    handle_obsolete_files(&mut store, &bundle, &plan, assume_yes)?;

    if let Some((remote_mcp, local_mcp)) = &plan.mcp_json {
        let local = local_mcp.exists().then_some(local_mcp.as_path());
        let auto_take = owned_unmodified_mcp_keys(&store, &bundle, local)?;
        let report = merge_mcp_json(local, remote_mcp, local_mcp, false, &auto_take, assume_yes)?;
        record_mcp_merge(&mut store, &bundle, &report)?;
        print_mcp_merge_report(&report);
        handle_missing_secrets(&report.missing_secrets)?;
    }

    let version = layout
        .manifest
        .as_ref()
        .and_then(|m| m.version.clone())
        .unwrap_or_else(|| temp.head_sha().chars().take(7).collect());
    store.set_bundle_versions(&bundle, temp.head_sha(), Some(version))?;
    store.mark_updated(&bundle)?;

    Ok(())
}

fn reclassify_owned_unmodified(
    mut plan: InstallPlan,
    store: &BundleStore,
    bundle: &str,
) -> Result<InstallPlan> {
    let owned: HashMap<&str, &str> = store
        .get(bundle)
        .map(|record| {
            record
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.sha256.as_str()))
                .collect()
        })
        .unwrap_or_default();

    for planned in &mut plan.files {
        if planned.kind != PlannedKind::Conflict {
            continue;
        }

        let Some(recorded) = owned.get(provenance_path(&planned.dst).as_str()) else {
            continue;
        };

        if hash_file(&planned.dst)? == *recorded {
            planned.kind = PlannedKind::Refresh;
        }
    }

    Ok(plan)
}

/// Keys of this bundle's mcp entries whose recorded hash still matches the
/// local entry: the bundle wrote them and the user never touched them, so an
/// upstream change takes the remote side without prompting.
fn owned_unmodified_mcp_keys(
    store: &BundleStore,
    bundle: &str,
    local: Option<&Path>,
) -> Result<HashSet<String>> {
    let mut keys = HashSet::new();
    let (Some(local_path), Some(record)) = (local, store.get(bundle)) else {
        return Ok(keys);
    };
    let content = fs::read_to_string(local_path)
        .with_context(|| format!("failed to read local mcp.json at {}", local_path.display()))?;
    let config: McpServersConfig = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse local mcp.json at {}", local_path.display()))?;

    for server in &record.mcp_servers {
        let Some(recorded_hash) = server.sha256.as_deref() else {
            continue;
        };
        let key = server.effective_key();
        let Some(entry) = config.mcp_servers.get(key) else {
            continue;
        };
        let serialized = serde_json::to_string(entry)
            .with_context(|| format!("failed to serialize MCP server '{key}'"))?;

        if hash_bytes(serialized.as_bytes()) == recorded_hash {
            keys.insert(key.to_string());
        }
    }

    Ok(keys)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObsoleteAction {
    Keep,
    Delete,
}

/// Kept files stay in the record, so a later uninstall still offers to
/// remove them.
fn handle_obsolete_files(
    store: &mut BundleStore,
    bundle: &str,
    plan: &InstallPlan,
    assume_yes: bool,
) -> Result<()> {
    let planned: HashSet<String> = plan
        .files
        .iter()
        .map(|planned| provenance_path(&planned.dst))
        .collect();
    let obsolete: Vec<String> = store
        .get(bundle)
        .map(|record| {
            record
                .files
                .iter()
                .map(|file| file.path.clone())
                .filter(|path| !planned.contains(path))
                .collect()
        })
        .unwrap_or_default();

    let config_dir = paths::config_dir();
    let mut sticky: Option<ObsoleteAction> = assume_yes.then_some(ObsoleteAction::Keep);
    for path in obsolete {
        if !is_safe_relative_path(&path) {
            eprintln!("skipping suspicious recorded path {path}; keeping its record");
            continue;
        }

        let full = config_dir.join(&path);
        if !full.exists() {
            println!("dropped record for obsolete file {path} (already absent locally)");
            store.remove_file_record(bundle, &path)?;
            continue;
        }

        let action = resolve_obsolete(&path, &mut sticky)?;
        apply_obsolete_action(store, bundle, &path, &full, &config_dir, action)?;
    }

    Ok(())
}

fn resolve_obsolete(path: &str, sticky: &mut Option<ObsoleteAction>) -> Result<ObsoleteAction> {
    if let Some(action) = *sticky {
        return Ok(action);
    }

    if !*IS_STDOUT_TERMINAL {
        return Ok(ObsoleteAction::Keep);
    }

    let prompt = format!("Obsolete file {path} is no longer shipped by the bundle");
    let choice = Select::new(&prompt, vec!["keep", "delete", "keep-all", "delete-all"])
        .prompt()
        .with_context(|| "failed to read obsolete-file choice")?;

    match choice {
        "keep" => Ok(ObsoleteAction::Keep),
        "delete" => Ok(ObsoleteAction::Delete),
        "keep-all" => {
            *sticky = Some(ObsoleteAction::Keep);
            Ok(ObsoleteAction::Keep)
        }
        "delete-all" => {
            *sticky = Some(ObsoleteAction::Delete);
            Ok(ObsoleteAction::Delete)
        }
        _ => unreachable!("inquire::Select returned an unexpected option"),
    }
}

fn apply_obsolete_action(
    store: &mut BundleStore,
    bundle: &str,
    path: &str,
    full: &Path,
    config_dir: &Path,
    action: ObsoleteAction,
) -> Result<()> {
    match action {
        ObsoleteAction::Keep => {
            println!("kept obsolete file {path} (no longer shipped by the bundle)");
        }
        ObsoleteAction::Delete => {
            fs::remove_file(full)
                .with_context(|| format!("failed to delete obsolete file {}", full.display()))?;
            store.remove_file_record(bundle, path)?;
            prune_empty_dirs(full, config_dir);
            println!("deleted obsolete file {path}");
        }
    }

    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct UninstallFileSummary {
    deleted: usize,
    kept: usize,
    missing: usize,
    failed: usize,
    /// Set when a functions/tools file was found on disk; its compiled binary
    /// lingers in the functions bin dir until the next --build-tools prune.
    tools_seen: bool,
}

#[derive(Debug, Default)]
struct UninstallMcpSummary {
    removed: Vec<String>,
    kept: Vec<String>,
    secrets: Vec<String>,
}

/// Kept and failed items stay in the record, so a re-run offers them again.
pub fn uninstall_bundle(spec: &str, assume_yes: bool) -> Result<()> {
    let mut store = BundleStore::load()?;
    let name = match store.get(spec) {
        Some(_) => spec.to_string(),
        None => match store.find_by_source(spec) {
            Some((name, _)) => name.to_string(),
            None => match select_uninstall_candidate(&store, spec)? {
                Some(name) => name,
                None => {
                    let installed = store.bundle_names();
                    if installed.is_empty() {
                        bail!("no bundle named '{spec}' is installed; none are installed");
                    }

                    bail!(
                        "no bundle named '{spec}' is installed; installed bundles: {}",
                        installed.join(", ")
                    );
                }
            },
        },
    };
    let record = store.get(&name).expect("resolved above").clone();

    if !assume_yes {
        if !*IS_STDOUT_TERMINAL {
            bail!(
                "refusing to uninstall bundle '{name}' non-interactively; \
                 re-run with --yes to confirm"
            );
        }
        println!(
            "Bundle '{name}' owns {} file(s) and {} mcp.json server(s).",
            record.files.len(),
            record.mcp_servers.len()
        );
        let proceed = Confirm::new(&format!("Uninstall bundle '{name}'?"))
            .with_default(false)
            .prompt()
            .with_context(|| "failed to read uninstall confirmation")?;
        if !proceed {
            println!("Uninstall of '{name}' aborted; nothing was changed.");
            return Ok(());
        }
    }

    let files = uninstall_owned_files(
        &mut store,
        &name,
        &record.files,
        &paths::config_dir(),
        assume_yes,
    )?;

    if files.tools_seen {
        println!(
            "Note: compiled tool binaries remain in {} until the next --build-tools prune.",
            paths::functions_bin_dir().display()
        );
    }

    let mcp = uninstall_mcp_entries(
        &mut store,
        &name,
        &record.mcp_servers,
        &paths::mcp_config_file(),
        assume_yes,
    )?;

    let empty = store
        .get(&name)
        .map(|record| record.files.is_empty() && record.mcp_servers.is_empty())
        .unwrap_or(true);

    if empty {
        store.remove_bundle(&name)?;
        println!("\nUninstalled bundle '{name}' and removed its record.");
    } else {
        println!(
            "\nBundle '{name}' partially uninstalled; its record keeps the remaining \
             items, so re-running --uninstall offers them again."
        );
    }

    println!(
        "  files: deleted={} kept={} missing={} failed={}",
        files.deleted, files.kept, files.missing, files.failed
    );
    println!(
        "  mcp servers: removed={} kept={}",
        mcp.removed.len(),
        mcp.kept.len()
    );

    if !mcp.removed.is_empty() {
        println!("  - removed servers: {}", mcp.removed.join(", "));
    }

    if !mcp.kept.is_empty() {
        println!("  = kept servers:    {}", mcp.kept.join(", "));
    }

    if !mcp.secrets.is_empty() {
        println!(
            "  ~ vault secrets referenced by this bundle's servers \
             (installed by this bundle, not removed): {}",
            mcp.secrets.join(", ")
        );
    }

    Ok(())
}

/// Never auto-picks between multiple matches: ambiguity is resolved by an
/// interactive prompt, and non-interactive runs bail even under --yes.
fn select_uninstall_candidate(store: &BundleStore, spec: &str) -> Result<Option<String>> {
    if !is_repo_shorthand(spec) {
        return Ok(None);
    }

    let needle = format!("/{}", canonical_source_url(spec));
    let candidates: Vec<(String, String)> = store
        .iter()
        .filter(|(_, record)| canonical_source_url(&record.source).ends_with(&needle))
        .map(|(name, record)| (name.to_string(), record.source.clone()))
        .collect();

    match candidates.as_slice() {
        [] => Ok(None),
        [(name, _)] => Ok(Some(name.clone())),
        _ => {
            let described: Vec<String> = candidates
                .iter()
                .map(|(name, source)| format!("{name} ({source})"))
                .collect();

            if !*IS_STDOUT_TERMINAL {
                bail!(
                    "'{spec}' matches multiple installed bundles: {}; re-run with the \
                     exact bundle name or source URL",
                    described.join(", ")
                );
            }

            let picked = Select::new(
                &format!("Multiple bundles match '{spec}'; which one should be uninstalled?"),
                described.clone(),
            )
            .prompt()
            .with_context(|| "failed to read uninstall selection")?;

            let index = described
                .iter()
                .position(|option| option == &picked)
                .expect("selection came from the presented options");

            Ok(Some(candidates[index].0.clone()))
        }
    }
}

/// True only when joining the path onto the config dir cannot escape it:
/// relative and made of plain components. Anything else in a recorded path
/// means a tampered store, and it must never become a file deletion.
fn is_safe_relative_path(path: &str) -> bool {
    let recorded = Path::new(path);
    !recorded.is_absolute()
        && recorded.components().all(|c| match c {
            Component::Normal(name) => is_safe_component(&name.to_string_lossy()),
            _ => false,
        })
}

/// Rejects names Windows refuses or silently rewrites (alternate data stream
/// colons, reserved device names, trailing dots or spaces) so a recorded path
/// denotes the same regular file on every platform.
fn is_safe_component(name: &str) -> bool {
    !name.contains(':')
        && !name.ends_with('.')
        && !name.ends_with(' ')
        && !is_windows_reserved_name(name)
}

fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let lower = stem.to_ascii_lowercase();
    matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || (lower.len() == 4
            && (lower.starts_with("com") || lower.starts_with("lpt"))
            && matches!(lower.as_bytes()[3], b'1'..=b'9'))
}

fn uninstall_owned_files(
    store: &mut BundleStore,
    bundle: &str,
    files: &[FileRecord],
    config_dir: &Path,
    assume_yes: bool,
) -> Result<UninstallFileSummary> {
    let mut summary = UninstallFileSummary::default();
    let mut sticky: Option<ObsoleteAction> = None;
    for file in files {
        if !is_safe_relative_path(&file.path) {
            eprintln!(
                "skipping suspicious recorded path {}; keeping its record",
                file.path
            );
            summary.failed += 1;
            continue;
        }

        let full = config_dir.join(Path::new(&file.path));
        if !full.exists() {
            println!(
                "dropped record for missing file {} (already absent locally)",
                file.path
            );
            store.remove_file_record(bundle, &file.path)?;
            summary.missing += 1;
            continue;
        }

        summary.tools_seen |= file.category == "functions/tools";

        if matches!(hash_file(&full), Ok(hash) if hash == file.sha256) {
            delete_owned_file(store, bundle, &file.path, &full, config_dir, &mut summary)?;
            continue;
        }

        let prompt = format!("File {} was modified locally after install", file.path);
        let action = resolve_uninstall_action(&prompt, assume_yes, &mut sticky)?;
        apply_uninstall_file_action(
            store,
            bundle,
            &file.path,
            &full,
            config_dir,
            action,
            &mut summary,
        )?;
    }

    Ok(summary)
}

fn apply_uninstall_file_action(
    store: &mut BundleStore,
    bundle: &str,
    path: &str,
    full: &Path,
    config_dir: &Path,
    action: ObsoleteAction,
    summary: &mut UninstallFileSummary,
) -> Result<()> {
    match action {
        ObsoleteAction::Keep => {
            println!("kept modified file {path}");
            summary.kept += 1;
        }
        ObsoleteAction::Delete => {
            delete_owned_file(store, bundle, path, full, config_dir, summary)?;
        }
    }

    Ok(())
}

fn delete_owned_file(
    store: &mut BundleStore,
    bundle: &str,
    path: &str,
    full: &Path,
    config_dir: &Path,
    summary: &mut UninstallFileSummary,
) -> Result<()> {
    match fs::remove_file(full) {
        Ok(()) => {
            store.remove_file_record(bundle, path)?;
            println!("deleted {path}");
            summary.deleted += 1;
            prune_empty_dirs(full, config_dir);
        }
        Err(err) => {
            eprintln!(
                "failed to delete {}: {err}; keeping its record",
                full.display()
            );
            summary.failed += 1;
        }
    }
    Ok(())
}

/// Remove now-empty ancestors of a deleted file, climbing strictly below
/// `config_dir`. `fs::remove_dir` fails on a non-empty directory, which is
/// the stop condition.
fn prune_empty_dirs(full: &Path, config_dir: &Path) {
    let mut current = full.parent();
    while let Some(dir) = current {
        if dir == config_dir || !dir.starts_with(config_dir) {
            break;
        }

        if fs::remove_dir(dir).is_err() {
            break;
        }

        current = dir.parent();
    }
}

fn resolve_uninstall_action(
    prompt: &str,
    assume_yes: bool,
    sticky: &mut Option<ObsoleteAction>,
) -> Result<ObsoleteAction> {
    if assume_yes || !*IS_STDOUT_TERMINAL {
        return Ok(ObsoleteAction::Keep);
    }

    if let Some(action) = *sticky {
        return Ok(action);
    }

    let choice = Select::new(prompt, vec!["keep", "delete", "keep-all", "delete-all"])
        .prompt()
        .with_context(|| "failed to read uninstall choice")?;
    match choice {
        "keep" => Ok(ObsoleteAction::Keep),
        "delete" => Ok(ObsoleteAction::Delete),
        "keep-all" => {
            *sticky = Some(ObsoleteAction::Keep);
            Ok(ObsoleteAction::Keep)
        }
        "delete-all" => {
            *sticky = Some(ObsoleteAction::Delete);
            Ok(ObsoleteAction::Delete)
        }
        _ => unreachable!("inquire::Select returned an unexpected option"),
    }
}

/// mcp.json is written before any ownership record is dropped: a failed write
/// leaves every record intact for a retry, while a crash after it leaves
/// stale records the absent-server branch cleans up on re-run.
fn uninstall_mcp_entries(
    store: &mut BundleStore,
    bundle: &str,
    servers: &[McpServerRecord],
    mcp_path: &Path,
    assume_yes: bool,
) -> Result<UninstallMcpSummary> {
    let mut summary = UninstallMcpSummary::default();
    if servers.is_empty() {
        return Ok(summary);
    }

    let mut config = if mcp_path.exists() {
        let content = fs::read_to_string(mcp_path)
            .with_context(|| format!("failed to read {}", mcp_path.display()))?;
        Some(
            serde_json::from_str::<McpServersConfig>(&content)
                .with_context(|| format!("failed to parse {}", mcp_path.display()))?,
        )
    } else {
        None
    };

    let mut secret_names: BTreeSet<String> = BTreeSet::new();
    for server in servers {
        if let Some(entry) = config
            .as_ref()
            .and_then(|cfg| cfg.mcp_servers.get(server.effective_key()))
            && let Ok(serialized) = serde_json::to_string(entry)
        {
            for capture in SECRET_RE.captures_iter(&serialized) {
                if let Ok(capture) = capture
                    && let Some(name) = capture.get(1)
                {
                    secret_names.insert(name.as_str().to_string());
                }
            }
        }
    }
    summary.secrets = secret_names.into_iter().collect();

    let mut changed = false;
    let mut released = Vec::new();
    let mut sticky: Option<ObsoleteAction> = None;
    for server in servers {
        let key = server.effective_key().to_string();
        if server.action == McpAction::Replaced {
            println!("kept pre-existing server '{key}'");
            released.push(key.clone());
            summary.kept.push(key);
            continue;
        }

        let Some(entry) = config.as_ref().and_then(|cfg| cfg.mcp_servers.get(&key)) else {
            println!("dropped record for absent server '{key}'");
            released.push(key);
            continue;
        };
        let serialized = serde_json::to_string(entry)
            .with_context(|| format!("failed to serialize MCP server '{key}'"))?;
        let intact = server.sha256.as_deref() == Some(hash_bytes(serialized.as_bytes()).as_str());
        let action = if intact {
            ObsoleteAction::Delete
        } else {
            let reason = if server.sha256.is_none() {
                "predates entry hashing"
            } else {
                "was modified locally after install"
            };
            let prompt = format!("MCP server '{key}' {reason}");
            resolve_uninstall_action(&prompt, assume_yes, &mut sticky)?
        };
        match action {
            ObsoleteAction::Keep => {
                println!("kept server '{key}'");
                summary.kept.push(key);
            }
            ObsoleteAction::Delete => {
                config
                    .as_mut()
                    .expect("entry was found in the config above")
                    .mcp_servers
                    .shift_remove(&key);
                changed = true;
                println!("removed server '{key}'");
                released.push(key.clone());
                summary.removed.push(key);
            }
        }
    }

    if changed {
        let config = config.expect("changed implies a parsed config");
        let serialized = serde_json::to_string_pretty(&config)
            .context("failed to serialize mcp.json after uninstall")?;
        write_atomically(mcp_path, &serialized)?;
    }
    for key in &released {
        store.remove_mcp_record(bundle, key)?;
    }

    Ok(summary)
}

fn parse_filter(name: &str) -> Result<InstallFilter> {
    InstallFilter::parse(name).with_context(|| {
        format!(
            "Unknown filter '{name}'. Valid values: {}",
            InstallFilter::NAMES.join(", ")
        )
    })
}

fn parse_url_with_ref(input: &str) -> Result<(String, Option<String>)> {
    match input.rsplit_once('#') {
        Some((url, refspec)) if !url.is_empty() => {
            if refspec.is_empty() {
                bail!("Empty ref after '#' in URL: {input}");
            }
            if refspec.contains("..") {
                bail!("Invalid ref '{refspec}': cannot contain '..'");
            }
            if refspec.starts_with('-') {
                bail!(
                    "Invalid ref '{refspec}': cannot start with '-' \
                     (would be parsed by git as a CLI flag)"
                );
            }
            if !refspec
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '+'))
            {
                bail!("Invalid ref '{refspec}': only [A-Za-z0-9._/+-] characters allowed");
            }
            Ok((url.to_string(), Some(refspec.to_string())))
        }
        _ => Ok((input.to_string(), None)),
    }
}

struct TempRepoDir {
    path: PathBuf,
    head_sha: String,
}

impl TempRepoDir {
    fn path(&self) -> &Path {
        &self.path
    }

    fn head_sha(&self) -> &str {
        &self.head_sha
    }
}

impl Drop for TempRepoDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            log::warn!(
                "failed to remove temp clone {}: {error}",
                self.path.display()
            );
        }
    }
}

fn is_commit_sha(reference: &str) -> bool {
    reference.len() >= 4
        && reference.len() <= 40
        && reference.chars().all(|c| c.is_ascii_hexdigit())
}

fn clone_to_temp(url: &str, reference: Option<&str>) -> Result<TempRepoDir> {
    let dest = utils::temp_file("coyote-remote-install-", "");
    match clone_into(&dest, url, reference) {
        Ok(head_sha) => Ok(TempRepoDir {
            path: dest,
            head_sha,
        }),
        Err(error) => {
            let _ = fs::remove_dir_all(&dest);
            Err(error)
        }
    }
}

/// Checked-out bytes must not depend on the machine's git configuration:
/// recorded sha256 provenance would otherwise drift with autocrlf settings.
/// Long paths are opted into for deep bundle trees on Windows.
fn git_content_config() -> Vec<OsString> {
    ["core.autocrlf=false", "core.eol=lf", "core.longpaths=true"]
        .iter()
        .flat_map(|setting| ["-c".into(), (*setting).into()])
        .collect()
}

fn clone_into(dest: &Path, url: &str, reference: Option<&str>) -> Result<String> {
    let dest_arg: OsString = dest.as_os_str().into();

    let is_sha = reference.is_some_and(is_commit_sha);

    match reference {
        Some(r) if !is_sha => {
            let mut args = git_content_config();
            args.extend([
                "clone".into(),
                "--depth".into(),
                "1".into(),
                "--branch".into(),
                r.into(),
                "--".into(),
                url.into(),
                dest_arg,
            ]);
            run_git(args)?;
        }
        Some(r) => {
            let mut args = git_content_config();
            args.extend(["clone".into(), "--".into(), url.into(), dest_arg.clone()]);
            run_git(args)?;
            let mut args = git_content_config();
            args.extend(["-C".into(), dest_arg, "checkout".into(), r.into()]);
            run_git(args)?;
        }
        None => {
            let mut args = git_content_config();
            args.extend([
                "clone".into(),
                "--depth".into(),
                "1".into(),
                "--".into(),
                url.into(),
                dest_arg,
            ]);
            run_git(args)?;
        }
    }

    let head_sha = run_git_capture(vec![
        "-C".into(),
        dest.as_os_str().into(),
        "rev-parse".into(),
        "HEAD".into(),
    ])?;
    Ok(head_sha)
}

fn run_git(args: Vec<OsString>) -> Result<()> {
    let output = duct::cmd("git", &args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin_null()
        .stderr_to_stdout()
        .stdout_capture()
        .unchecked()
        .run()
        .context("failed to spawn git (is it installed and on PATH?)")?;

    if !output.status.success() {
        let combined = String::from_utf8_lossy(&output.stdout);
        bail!("git failed: {}", combined.trim());
    }

    Ok(())
}

fn run_git_capture(args: Vec<OsString>) -> Result<String> {
    let output = duct::cmd("git", &args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin_null()
        .stdout_capture()
        .stderr_capture()
        .unchecked()
        .run()
        .context("failed to spawn git (is it installed and on PATH?)")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git failed: {}", format!("{stdout} {stderr}").trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Debug, Default)]
struct RemoteLayout {
    agents: Option<PathBuf>,
    roles: Option<PathBuf>,
    skills: Option<PathBuf>,
    macros: Option<PathBuf>,
    functions_tools: Option<PathBuf>,
    mcp_json: Option<PathBuf>,
    manifest: Option<BundleManifest>,
    head_sha: Option<String>,
}

impl RemoteLayout {
    fn is_empty(&self) -> bool {
        self.agents.is_none()
            && self.roles.is_none()
            && self.skills.is_none()
            && self.macros.is_none()
            && self.functions_tools.is_none()
            && self.mcp_json.is_none()
    }
}

fn scan_remote_layout(root: &Path) -> Result<RemoteLayout> {
    let mut layout = RemoteLayout {
        manifest: parse_bundle_manifest(root)?,
        ..RemoteLayout::default()
    };

    let agents = root.join("agents");
    if agents.is_dir() {
        layout.agents = Some(agents);
    }

    let roles = root.join("roles");
    if roles.is_dir() {
        layout.roles = Some(roles);
    }

    let skills = root.join("skills");
    if skills.is_dir() {
        layout.skills = Some(skills);
    }

    let macros = root.join("macros");
    if macros.is_dir() {
        layout.macros = Some(macros);
    }

    let root_mcp = root.join("mcp.json");
    if root_mcp.is_file() {
        layout.mcp_json = Some(root_mcp);
    }

    let functions = root.join("functions");
    if functions.is_dir() {
        let tools = functions.join("tools");
        if tools.is_dir() {
            layout.functions_tools = Some(tools);
        }

        // Legacy bundle layout; a root-level mcp.json wins when both exist.
        let mcp = functions.join("mcp.json");
        if layout.mcp_json.is_none() && mcp.is_file() {
            layout.mcp_json = Some(mcp);
        }
    }

    Ok(layout)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct BundleManifest {
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) homepage: Option<String>,
}

/// Returns the record key, possibly migrated or owner-qualified, that all
/// subsequent recording must use in place of the requested name.
/// An update passes `preserve_versions` so the record keeps claiming the old
/// commit and version until the new content actually lands on disk.
fn register_bundle(
    store: &mut BundleStore,
    url: &str,
    git_ref: Option<&str>,
    manifest: Option<&BundleManifest>,
    commit: &str,
    preserve_versions: bool,
) -> Result<String> {
    let resolved = store.resolve_bundle_name(url, manifest.map(|m| m.name.as_str()))?;
    let version = manifest
        .and_then(|m| m.version.clone())
        .unwrap_or_else(|| commit.chars().take(7).collect());
    let (commit, version) = match (preserve_versions, store.get(&resolved.name)) {
        (true, Some(existing)) => (existing.commit.clone(), existing.version.clone()),
        _ => (commit.to_string(), Some(version)),
    };
    store.upsert_bundle(
        &resolved.name,
        InstallMetadata {
            source: url.to_string(),
            git_ref: git_ref.map(str::to_string),
            commit,
            version,
            description: manifest.and_then(|m| m.description.clone()),
            homepage: manifest.and_then(|m| m.homepage.clone()),
        },
    )?;

    Ok(resolved.name)
}

fn parse_bundle_manifest(root: &Path) -> Result<Option<BundleManifest>> {
    let path = root.join(BUNDLE_MANIFEST_FILE);
    if !path.is_file() {
        return Ok(None);
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read bundle manifest at {}", path.display()))?;
    let manifest: BundleManifest = serde_yaml::from_str(&content)
        .with_context(|| format!("invalid bundle manifest at {}", path.display()))?;
    validate_bundle_name(&manifest.name)
        .with_context(|| format!("invalid bundle name in manifest at {}", path.display()))?;

    Ok(Some(manifest))
}

pub(crate) fn validate_bundle_name(name: &str) -> Result<()> {
    let (owner, base) = match name.split_once('/') {
        Some((owner, base)) => (Some(owner), base),
        None => (None, name),
    };
    if base.contains('/') {
        bail!(
            "Invalid bundle name '{name}': at most one '/' is allowed \
             (as the owner qualifier separator)"
        );
    }

    for part in owner.into_iter().chain(iter::once(base)) {
        if part.is_empty() {
            bail!("Invalid bundle name '{name}': name segments cannot be empty");
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!(
                "Invalid bundle name '{name}': only letters, digits, '-', and '_' are allowed \
                 (plus a single '/' separating an owner qualifier)"
            );
        }
    }

    Ok(())
}

fn strip_ref_suffix(url: &str) -> &str {
    match url.rsplit_once('#') {
        Some((base, _)) if !base.is_empty() => base,
        _ => url,
    }
}

fn split_host_and_path(url: &str) -> (String, String) {
    let url = strip_ref_suffix(url);
    let url = &url.replace('\\', "/");
    if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host.to_string(), path.trim_matches('/').to_string())
    } else if let Some((prefix, path)) = url.split_once(':')
        && !prefix.contains('/')
    {
        let host = prefix.rsplit_once('@').map_or(prefix, |(_, h)| h);
        (host.to_string(), path.trim_matches('/').to_string())
    } else {
        (String::new(), url.trim_end_matches('/').to_string())
    }
}

fn strip_git_suffix(segment: &str) -> &str {
    if segment.len() > 4 && segment.to_ascii_lowercase().ends_with(".git") {
        &segment[..segment.len() - 4]
    } else {
        segment
    }
}

pub(crate) fn repo_name_slug(url: &str) -> String {
    let (_, path) = split_host_and_path(url);
    let last = path.rsplit('/').next().unwrap_or("");
    strip_git_suffix(last).to_string()
}

pub(crate) fn owner_qualifier(url: &str) -> Option<String> {
    let (host, path) = split_host_and_path(url);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() >= 2 {
        return Some(segments[segments.len() - 2].to_string());
    }

    let sanitized = sanitize_host(&host);
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitize_host(host: &str) -> String {
    host.to_ascii_lowercase()
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

/// The host compares case-insensitively but the path keeps its case: many
/// self-hosted forges treat repository paths as case-sensitive, and collapsing
/// distinct repos into one record misdirects updates and uninstalls.
pub(crate) fn canonical_source_url(url: &str) -> String {
    let (host, path) = split_host_and_path(url);
    let mut path = path;
    if path.to_ascii_lowercase().ends_with(".git") {
        let stripped = &path[..path.len() - 4];
        if !stripped.is_empty() && !stripped.ends_with('/') {
            path.truncate(path.len() - 4);
        }
    }
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        path
    } else if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

fn apply_filter(mut layout: RemoteLayout, filter: Option<InstallFilter>) -> RemoteLayout {
    let Some(filter) = filter else {
        return layout;
    };
    let base = RemoteLayout {
        manifest: layout.manifest.take(),
        head_sha: layout.head_sha.take(),
        ..RemoteLayout::default()
    };
    match filter {
        InstallFilter::Agents => RemoteLayout {
            agents: layout.agents.take(),
            ..base
        },
        InstallFilter::Roles => RemoteLayout {
            roles: layout.roles.take(),
            ..base
        },
        InstallFilter::Skills => RemoteLayout {
            skills: layout.skills.take(),
            ..base
        },
        InstallFilter::Macros => RemoteLayout {
            macros: layout.macros.take(),
            ..base
        },
        InstallFilter::Functions => RemoteLayout {
            functions_tools: layout.functions_tools.take(),
            ..base
        },
        InstallFilter::McpConfig => RemoteLayout {
            mcp_json: layout.mcp_json.take(),
            ..base
        },
    }
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_files_inner(root, &mut out)?;
    Ok(out)
}

fn walk_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();

        if file_type.is_symlink() {
            bail!(
                "Symlink not allowed in remote install source: {}",
                entry.path().display()
            );
        }
        if name == OsStr::new(".git") {
            continue;
        }
        if name == OsStr::new("..") {
            bail!(
                "Path traversal '..' not allowed: {}",
                entry.path().display()
            );
        }

        let path = entry.path();
        if file_type.is_dir() {
            walk_files_inner(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopCategory {
    Agents,
    Roles,
    Skills,
    Macros,
    FunctionsTools,
}

impl TopCategory {
    fn label(&self) -> &'static str {
        match self {
            TopCategory::Agents => "agents",
            TopCategory::Roles => "roles",
            TopCategory::Skills => "skills",
            TopCategory::Macros => "macros",
            TopCategory::FunctionsTools => "functions/tools",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedKind {
    New,
    Identical,
    Conflict,
    /// A conflict downgraded because this bundle owns the file and the local
    /// content still matches the recorded hash; applied without prompting.
    Refresh,
}

struct PlannedFile {
    src: PathBuf,
    dst: PathBuf,
    kind: PlannedKind,
    top_category: TopCategory,
}

struct InstallPlan {
    files: Vec<PlannedFile>,
    mcp_json: Option<(PathBuf, PathBuf)>,
}

fn plan_changes(layout: &RemoteLayout) -> Result<InstallPlan> {
    let mut files = Vec::new();

    if let Some(src_dir) = &layout.agents {
        plan_dir_into(
            src_dir,
            &paths::agents_data_dir(),
            TopCategory::Agents,
            &mut files,
        )?;
    }
    if let Some(src_dir) = &layout.roles {
        plan_dir_into(src_dir, &paths::roles_dir(), TopCategory::Roles, &mut files)?;
    }

    if let Some(src_dir) = &layout.skills {
        plan_dir_into(
            src_dir,
            &paths::skills_dir(),
            TopCategory::Skills,
            &mut files,
        )?;
    }

    if let Some(src_dir) = &layout.macros {
        plan_dir_into(
            src_dir,
            &paths::macros_dir(),
            TopCategory::Macros,
            &mut files,
        )?;
    }
    if let Some(src_dir) = &layout.functions_tools {
        plan_dir_into(
            src_dir,
            &paths::functions_dir().join("tools"),
            TopCategory::FunctionsTools,
            &mut files,
        )?;
    }

    let mcp_json = layout
        .mcp_json
        .as_ref()
        .map(|src| (src.clone(), paths::mcp_config_file()));

    Ok(InstallPlan { files, mcp_json })
}

fn plan_dir_into(
    src_dir: &Path,
    dst_dir: &Path,
    category: TopCategory,
    out: &mut Vec<PlannedFile>,
) -> Result<()> {
    for src in walk_files(src_dir)? {
        let rel = src
            .strip_prefix(src_dir)
            .expect("walk_files only returns paths under src_dir");

        if category == TopCategory::Skills {
            let skill_name = rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .ok_or_else(|| {
                    anyhow!(
                        "remote skill bundle has unparseable path component: {}",
                        rel.display()
                    )
                })?;
            paths::validate_skill_name(skill_name).with_context(|| {
                format!(
                    "remote skill '{skill_name}' has an invalid name \
                     (skill names must contain only ASCII alphanumerics, '-', or '_')"
                )
            })?;
        }

        let dst = dst_dir.join(rel);
        let kind = classify_file(&src, &dst)?;
        out.push(PlannedFile {
            src,
            dst,
            kind,
            top_category: category,
        });
    }
    Ok(())
}

fn classify_file(src: &Path, dst: &Path) -> Result<PlannedKind> {
    if !dst.exists() {
        return Ok(PlannedKind::New);
    }
    if files_equal(src, dst)? {
        Ok(PlannedKind::Identical)
    } else {
        Ok(PlannedKind::Conflict)
    }
}

const LARGE_FILE_THRESHOLD: u64 = 8 * 1024 * 1024;

fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    let a_meta = fs::metadata(a).with_context(|| format!("stat {}", a.display()))?;
    let b_meta = fs::metadata(b).with_context(|| format!("stat {}", b.display()))?;
    if a_meta.len() != b_meta.len() {
        return Ok(false);
    }
    if a_meta.len() > LARGE_FILE_THRESHOLD {
        files_equal_streaming(a, b)
    } else {
        let a_bytes = fs::read(a).with_context(|| format!("read {}", a.display()))?;
        let b_bytes = fs::read(b).with_context(|| format!("read {}", b.display()))?;
        Ok(a_bytes == b_bytes)
    }
}

fn files_equal_streaming(a: &Path, b: &Path) -> Result<bool> {
    let mut fa = fs::File::open(a).with_context(|| format!("open {}", a.display()))?;
    let mut fb = fs::File::open(b).with_context(|| format!("open {}", b.display()))?;
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let na = read_full(&mut fa, &mut buf_a)?;
        let nb = read_full(&mut fb, &mut buf_b)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if buf_a[..na] != buf_b[..nb] {
            return Ok(false);
        }
    }
}

/// `read` may return short counts without EOF; comparing partially filled
/// buffers positionally would misreport identical files as different.
fn read_full(file: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }

    Ok(filled)
}

fn print_plan_summary(plan: &InstallPlan) {
    println!("Plan:");
    for cat in [
        TopCategory::Agents,
        TopCategory::Roles,
        TopCategory::Skills,
        TopCategory::Macros,
        TopCategory::FunctionsTools,
    ] {
        let new_ = count_kind(plan, cat, PlannedKind::New);
        let identical = count_kind(plan, cat, PlannedKind::Identical);
        let conflict = count_kind(plan, cat, PlannedKind::Conflict);
        let refresh = count_kind(plan, cat, PlannedKind::Refresh);
        if new_ + identical + conflict + refresh > 0 {
            let mut line = format!(
                "  {:<16} new={new_}  identical={identical}  conflict={conflict}",
                cat.label()
            );
            if refresh > 0 {
                line.push_str(&format!("  refresh={refresh}"));
            }

            println!("{line}");
        }
    }
}

fn count_kind(plan: &InstallPlan, cat: TopCategory, kind: PlannedKind) -> usize {
    plan.files
        .iter()
        .filter(|p| p.top_category == cat && p.kind == kind)
        .count()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StickyMode {
    None,
    KeepAll,
    ReplaceAll,
}

enum ConflictAction {
    Keep,
    Replace,
}

#[derive(Debug)]
struct ApplyReport {
    new_count: usize,
    identical_count: usize,
    replaced_count: usize,
    refreshed_count: usize,
    kept_count: usize,
}

fn apply_plan(
    plan: &InstallPlan,
    initial_mode: StickyMode,
    store: &mut BundleStore,
    bundle: &str,
) -> Result<ApplyReport> {
    let mut report = ApplyReport {
        new_count: 0,
        identical_count: 0,
        replaced_count: 0,
        refreshed_count: 0,
        kept_count: 0,
    };
    let mut sticky = initial_mode;

    for planned in &plan.files {
        match planned.kind {
            PlannedKind::New => {
                write_file(&planned.src, &planned.dst)?;
                record_written_file(store, bundle, planned, FileAction::New)?;
                report.new_count += 1;
            }
            PlannedKind::Identical => {
                report.identical_count += 1;
            }
            PlannedKind::Refresh => {
                write_file(&planned.src, &planned.dst)?;
                record_written_file(store, bundle, planned, FileAction::Replaced)?;
                report.refreshed_count += 1;
            }
            PlannedKind::Conflict => match resolve_conflict(planned, &mut sticky)? {
                ConflictAction::Keep => report.kept_count += 1,
                ConflictAction::Replace => {
                    write_file(&planned.src, &planned.dst)?;
                    record_written_file(store, bundle, planned, FileAction::Replaced)?;
                    report.replaced_count += 1;
                }
            },
        }
    }

    if report.refreshed_count > 0 {
        println!(
            "\nInstalled: {} new, {} refreshed, {} replaced, {} kept, {} identical.",
            report.new_count,
            report.refreshed_count,
            report.replaced_count,
            report.kept_count,
            report.identical_count
        );
    } else {
        println!(
            "\nInstalled: {} new, {} replaced, {} kept, {} identical.",
            report.new_count, report.replaced_count, report.kept_count, report.identical_count
        );
    }

    Ok(report)
}

/// Kept and identical files are never recorded, because ownership means
/// "this content exists because of this bundle": a file another bundle's
/// record already owns stays with that owner.
fn record_written_file(
    store: &mut BundleStore,
    bundle: &str,
    planned: &PlannedFile,
    action: FileAction,
) -> Result<()> {
    store.record_file(
        bundle,
        FileRecord {
            path: provenance_path(&planned.dst),
            category: planned.top_category.label().to_string(),
            sha256: hash_file(&planned.dst)?,
            action,
        },
    )
}

fn provenance_path(dst: &Path) -> String {
    let rel = match dst.strip_prefix(paths::config_dir()) {
        Ok(rel) => rel,
        Err(_) => {
            log::warn!(
                "bundle file {} lies outside the config dir (an asset dir override?); \
                 it will not be uninstallable and drift checks may misreport it",
                dst.display()
            );
            dst
        }
    };
    rel.to_string_lossy().replace('\\', "/")
}

/// Entries the merge kept local are deliberately absent: an entry another
/// bundle already owns stays with that owner.
fn record_mcp_merge(store: &mut BundleStore, bundle: &str, report: &McpMergeReport) -> Result<()> {
    let mut entries: Vec<McpServerRecord> = Vec::new();
    entries.extend(report.added.iter().map(|name| McpServerRecord {
        name: name.clone(),
        action: McpAction::Added,
        renamed_to: None,
        sha256: report.entry_hashes.get(name).cloned(),
    }));
    entries.extend(report.replaced.iter().map(|name| McpServerRecord {
        name: name.clone(),
        action: McpAction::Replaced,
        renamed_to: None,
        sha256: report.entry_hashes.get(name).cloned(),
    }));
    entries.extend(
        report
            .renamed
            .iter()
            .map(|(name, renamed_to)| McpServerRecord {
                name: name.clone(),
                action: McpAction::Renamed,
                renamed_to: Some(renamed_to.clone()),
                sha256: report.entry_hashes.get(renamed_to).cloned(),
            }),
    );

    if entries.is_empty() {
        return Ok(());
    }

    store.record_mcp_servers(bundle, entries)
}

fn resolve_conflict(planned: &PlannedFile, sticky: &mut StickyMode) -> Result<ConflictAction> {
    match *sticky {
        StickyMode::KeepAll => return Ok(ConflictAction::Keep),
        StickyMode::ReplaceAll => return Ok(ConflictAction::Replace),
        StickyMode::None => {}
    }

    if !*IS_STDOUT_TERMINAL {
        bail!(
            "Refusing to overwrite local file {} non-interactively. \
             Re-run in a terminal, with --install-force (installs), \
             or with --yes (updates).",
            planned.dst.display()
        );
    }

    let prompt = format!(
        "Conflict at {} (category: {})",
        planned.dst.display(),
        planned.top_category.label()
    );
    let choice = Select::new(
        &prompt,
        vec!["keep", "replace", "keep-all", "replace-all", "abort"],
    )
    .prompt()
    .with_context(|| "failed to read conflict choice")?;

    match choice {
        "keep" => Ok(ConflictAction::Keep),
        "replace" => Ok(ConflictAction::Replace),
        "keep-all" => {
            *sticky = StickyMode::KeepAll;
            Ok(ConflictAction::Keep)
        }
        "replace-all" => {
            *sticky = StickyMode::ReplaceAll;
            Ok(ConflictAction::Replace)
        }
        "abort" => bail!("Install aborted by user at conflict resolution."),
        _ => unreachable!("inquire::Select returned an unexpected option"),
    }
}

fn write_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::copy(src, dst)
        .with_context(|| format!("failed to copy {} to {}", src.display(), dst.display()))?;
    set_executable_bit_if_script(dst)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable_bit_if_script(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return Ok(());
    };
    if Language::from_extension(ext) == Language::Unsupported {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable_bit_if_script(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug)]
struct McpMergeReport {
    added: Vec<String>,
    kept_local: Vec<String>,
    replaced: Vec<String>,
    renamed: Vec<(String, String)>,
    /// Hash of each entry this merge wrote, by its final key in mcp.json, so
    /// uninstall can tell the entry apart from a later local edit.
    entry_hashes: HashMap<String, String>,
    final_path: PathBuf,
    missing_secrets: Vec<String>,
}

enum McpConflictAction {
    KeepLocal,
    TakeRemote,
    RenameRemote,
}

fn merge_mcp_json(
    local: Option<&Path>,
    remote: &Path,
    target: &Path,
    force: bool,
    auto_take: &HashSet<String>,
    assume_yes: bool,
) -> Result<McpMergeReport> {
    let remote_content = fs::read_to_string(remote)
        .with_context(|| format!("failed to read remote mcp.json at {}", remote.display()))?;
    let remote_config: McpServersConfig = serde_json::from_str(&remote_content)
        .with_context(|| format!("failed to parse remote mcp.json at {}", remote.display()))?;

    let mut merged = if let Some(local_path) = local {
        let content = fs::read_to_string(local_path).with_context(|| {
            format!("failed to read local mcp.json at {}", local_path.display())
        })?;
        serde_json::from_str::<McpServersConfig>(&content).with_context(|| {
            format!("failed to parse local mcp.json at {}", local_path.display())
        })?
    } else {
        McpServersConfig {
            mcp_servers: IndexMap::new(),
        }
    };

    let final_path = target.to_path_buf();
    let mut report = McpMergeReport {
        added: Vec::new(),
        kept_local: Vec::new(),
        replaced: Vec::new(),
        renamed: Vec::new(),
        entry_hashes: HashMap::new(),
        final_path: final_path.clone(),
        missing_secrets: Vec::new(),
    };
    let mut to_validate: Vec<String> = Vec::new();

    for (name, remote_server) in remote_config.mcp_servers {
        if let Some(local_server) = merged.mcp_servers.get(&name) {
            if local_server == &remote_server {
                continue;
            }
            let action = if auto_take.contains(&name) {
                McpConflictAction::TakeRemote
            } else {
                resolve_mcp_conflict(&name, force, assume_yes)?
            };
            match action {
                McpConflictAction::KeepLocal => report.kept_local.push(name),
                McpConflictAction::TakeRemote => {
                    merged.mcp_servers.insert(name.clone(), remote_server);
                    report.replaced.push(name.clone());
                    to_validate.push(name);
                }
                McpConflictAction::RenameRemote => {
                    let new_name = unique_renamed_key(&name, &merged.mcp_servers);
                    merged.mcp_servers.insert(new_name.clone(), remote_server);
                    report.renamed.push((name, new_name.clone()));
                    to_validate.push(new_name);
                }
            }
        } else {
            merged.mcp_servers.insert(name.clone(), remote_server);
            report.added.push(name.clone());
            to_validate.push(name);
        }
    }

    for key in &to_validate {
        let spec = merged
            .mcp_servers
            .get(key)
            .expect("entry was just inserted");
        spec.validate(key).with_context(|| {
            format!("MCP server '{key}' failed validation; refusing to write merged mcp.json")
        })?;
        let serialized = serde_json::to_string(spec)
            .with_context(|| format!("failed to serialize MCP server '{key}'"))?;
        report
            .entry_hashes
            .insert(key.clone(), hash_bytes(serialized.as_bytes()));
    }

    let serialized =
        serde_json::to_string_pretty(&merged).context("failed to serialize merged mcp.json")?;
    write_atomically(&final_path, &serialized)?;

    let vault = Vault::init_bare()?;
    let missing = match interpolate_secrets(&serialized, &vault) {
        Ok((_, missing)) => missing,
        Err(e) => {
            eprintln!(
                "{}",
                formatdoc! {"
                Skipping secret resolution for merged mcp.json: {e:#}
                Continuing without resolving missing secrets
                You may need to add any additional missing secrets to the vault manually.
            "}
            );
            Vec::new()
        }
    };
    let mut deduped: Vec<String> = Vec::new();
    for s in missing {
        if !deduped.contains(&s) {
            deduped.push(s);
        }
    }
    report.missing_secrets = deduped;

    Ok(report)
}

fn resolve_mcp_conflict(name: &str, force: bool, assume_yes: bool) -> Result<McpConflictAction> {
    if force {
        return Ok(McpConflictAction::TakeRemote);
    }
    if assume_yes {
        return Ok(McpConflictAction::KeepLocal);
    }
    if !*IS_STDOUT_TERMINAL {
        bail!(
            "MCP server '{name}' already exists locally. Refusing to merge non-interactively. \
             Re-run in a terminal, with --install-force (installs), or with --yes (updates)."
        );
    }
    let rename_label = format!("rename remote as \"{name}-remote\"");
    let prompt = format!("Conflict on MCP server '{name}'");
    let choice = Select::new(
        &prompt,
        vec![
            "keep local".to_string(),
            "take remote".to_string(),
            rename_label.clone(),
            "abort merge".to_string(),
        ],
    )
    .prompt()
    .with_context(|| "failed to read MCP conflict choice")?;

    if choice == "keep local" {
        Ok(McpConflictAction::KeepLocal)
    } else if choice == "take remote" {
        Ok(McpConflictAction::TakeRemote)
    } else if choice == rename_label {
        Ok(McpConflictAction::RenameRemote)
    } else if choice == "abort merge" {
        bail!("Aborted MCP merge by user.")
    } else {
        unreachable!("inquire::Select returned an unexpected option")
    }
}

fn unique_renamed_key(name: &str, existing: &IndexMap<String, McpServer>) -> String {
    let base = format!("{name}-remote");
    if !existing.contains_key(&base) {
        return base;
    }
    for i in 2..=u32::MAX {
        let candidate = format!("{name}-remote-{i}");
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("ran out of suffix variants")
}

fn write_atomically(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, content)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn print_mcp_merge_report(report: &McpMergeReport) {
    println!("\nMCP merge ({}):", report.final_path.display());
    println!(
        "  added: {}, replaced: {}, kept local: {}, renamed: {}",
        report.added.len(),
        report.replaced.len(),
        report.kept_local.len(),
        report.renamed.len()
    );
    if !report.added.is_empty() {
        println!("  + new servers: {}", report.added.join(", "));
    }
    if !report.replaced.is_empty() {
        println!("  ~ replaced:    {}", report.replaced.join(", "));
    }
    if !report.kept_local.is_empty() {
        println!("  = kept local:  {}", report.kept_local.join(", "));
    }
    if !report.renamed.is_empty() {
        let pairs: Vec<String> = report
            .renamed
            .iter()
            .map(|(orig, new_)| format!("{orig} -> {new_}"))
            .collect();
        println!("  > renamed:     {}", pairs.join(", "));
    }
}

fn handle_missing_secrets(missing: &[String]) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let (added, deferred) = if *IS_STDOUT_TERMINAL {
        println!(
            "\nThe merged mcp.json references {} secret(s) not yet in the vault.",
            missing.len()
        );
        prompt_for_each_secret(missing)?
    } else {
        (Vec::new(), missing.to_vec())
    };

    print_secret_summary(&added, &deferred);
    Ok(())
}

fn prompt_for_each_secret(missing: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut vault = Vault::init_bare()?;
    let mut password_file_ensured = false;
    let mut added = Vec::new();
    let mut deferred = Vec::new();

    for name in missing {
        let proceed = Confirm::new(&format!("Add {{{{ {name} }}}} to vault now?"))
            .with_default(false)
            .prompt()
            .with_context(|| format!("failed to read confirmation for secret '{name}'"))?;
        if !proceed {
            deferred.push(name.clone());
            continue;
        }
        if !password_file_ensured {
            create_vault_password_file(&mut vault)
                .context("Failed to initialize the vault password file")?;
            password_file_ensured = true;
        }

        match vault.add_secret(name) {
            Ok(()) => added.push(name.clone()),
            Err(e) => {
                eprintln!("Failed to add '{name}' to vault: {e:#}");
                deferred.push(name.clone());
            }
        }
    }

    Ok((added, deferred))
}

fn print_secret_summary(added: &[String], deferred: &[String]) {
    if !added.is_empty() {
        println!(
            "\nAdded {} secret(s) to the vault: {}",
            added.len(),
            added.join(", ")
        );
    }
    if !deferred.is_empty() {
        println!(
            "\nThe following secrets are still required by your MCP servers. \
             Add them with `coyote --add-secret <NAME>` or `.vault add <NAME>` in the REPL:"
        );
        for name in deferred {
            println!("  {{{{ {name} }}}}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SANDBOX_ENV_FLAG;
    use crate::utils::get_env_name;
    use serial_test::serial;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn safe_relative_path_accepts_plain_portable_components() {
        assert!(is_safe_relative_path("macros/a.yaml"));
        assert!(is_safe_relative_path("skills/deep/nested/file.md"));
        assert!(is_safe_relative_path("functions/tools/console.sh"));
        assert!(is_safe_relative_path("roles/common.md"));
    }

    #[test]
    fn safe_relative_path_rejects_escapes_and_windows_hazards() {
        assert!(!is_safe_relative_path("../outside.yaml"));
        assert!(!is_safe_relative_path("/abs/path.yaml"));
        assert!(!is_safe_relative_path("macros/../../evil.yaml"));
        assert!(!is_safe_relative_path("macros/a.yaml:stream"));
        assert!(!is_safe_relative_path("macros/trailing."));
        assert!(!is_safe_relative_path("macros/trailing "));
        assert!(!is_safe_relative_path("macros/nul"));
        assert!(!is_safe_relative_path("macros/NUL.yaml"));
        assert!(!is_safe_relative_path("con/a.yaml"));
        assert!(!is_safe_relative_path("macros/COM1.txt"));
        assert!(!is_safe_relative_path("macros/lpt9"));
    }

    #[test]
    fn windows_reserved_name_check_is_stem_based() {
        assert!(is_windows_reserved_name("nul"));
        assert!(is_windows_reserved_name("NUL.txt"));
        assert!(is_windows_reserved_name("com1"));
        assert!(!is_windows_reserved_name("com0"));
        assert!(!is_windows_reserved_name("com10"));
        assert!(!is_windows_reserved_name("console"));
        assert!(!is_windows_reserved_name("nullable.yaml"));
    }

    #[test]
    fn git_content_config_pins_line_endings_and_long_paths() {
        let args = git_content_config();
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(rendered.len(), 6);
        assert!(rendered.contains(&"core.autocrlf=false".to_string()));
        assert!(rendered.contains(&"core.eol=lf".to_string()));
        assert!(rendered.contains(&"core.longpaths=true".to_string()));
    }

    struct TestVaultConfigGuard {
        dir_key: String,
        file_key: String,
        previous_dir: Option<OsString>,
        previous_file: Option<OsString>,
        path: PathBuf,
    }

    impl TestVaultConfigGuard {
        fn new(label: &str) -> Self {
            let dir_key = get_env_name("config_dir");
            let file_key = get_env_name("config_file");
            let previous_dir = env::var_os(&dir_key);
            let previous_file = env::var_os(&file_key);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-vault-test-{label}-{unique}"));
            fs::create_dir_all(&path).unwrap();
            let config_path = path.join("config.yaml");
            fs::write(&config_path, "{}").unwrap();
            unsafe {
                env::set_var(&dir_key, &path);
                env::set_var(&file_key, &config_path);
            }
            Self {
                dir_key,
                file_key,
                previous_dir,
                previous_file,
                path,
            }
        }
    }

    impl Drop for TestVaultConfigGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous_dir {
                    Some(p) => env::set_var(&self.dir_key, p),
                    None => env::remove_var(&self.dir_key),
                }
                match &self.previous_file {
                    Some(p) => env::set_var(&self.file_key, p),
                    None => env::remove_var(&self.file_key),
                }
            }
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parse_url_no_ref() {
        let (url, r) = parse_url_with_ref("https://github.com/foo/bar.git").unwrap();

        assert_eq!(url, "https://github.com/foo/bar.git");
        assert_eq!(r, None);
    }

    #[test]
    fn parse_url_with_branch_ref() {
        let (url, r) = parse_url_with_ref("https://github.com/foo/bar.git#main").unwrap();

        assert_eq!(url, "https://github.com/foo/bar.git");
        assert_eq!(r.as_deref(), Some("main"));
    }

    #[test]
    fn parse_url_with_tag_ref() {
        let (url, r) = parse_url_with_ref("https://github.com/foo/bar.git#v1.2.3").unwrap();

        assert_eq!(url, "https://github.com/foo/bar.git");
        assert_eq!(r.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn parse_url_with_sha_ref() {
        let (url, r) = parse_url_with_ref("https://github.com/foo/bar.git#abc1234").unwrap();

        assert_eq!(url, "https://github.com/foo/bar.git");
        assert_eq!(r.as_deref(), Some("abc1234"));
    }

    #[test]
    fn parse_url_with_slash_in_ref() {
        let (url, r) = parse_url_with_ref("git@github.com:foo/bar.git#release/v2").unwrap();

        assert_eq!(url, "git@github.com:foo/bar.git");
        assert_eq!(r.as_deref(), Some("release/v2"));
    }

    #[test]
    fn parse_url_rejects_empty_ref() {
        assert!(parse_url_with_ref("https://github.com/foo/bar.git#").is_err());
    }

    #[test]
    fn parse_url_rejects_dotdot() {
        assert!(parse_url_with_ref("https://github.com/foo/bar.git#foo..bar").is_err());
    }

    #[test]
    fn parse_url_rejects_leading_dash_argument_injection() {
        assert!(parse_url_with_ref("https://github.com/foo/bar.git#-evil").is_err());
    }

    #[test]
    fn parse_url_rejects_shell_metachars() {
        assert!(parse_url_with_ref("https://github.com/foo/bar.git#foo bar").is_err());
        assert!(parse_url_with_ref("https://github.com/foo/bar.git#$inject").is_err());
        assert!(parse_url_with_ref("https://github.com/foo/bar.git#;rm -rf /").is_err());
    }

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, b"").unwrap();
    }

    fn fresh_temp_dir(name: &str) -> PathBuf {
        let dir = utils::temp_file(name, "");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn remote_layout_is_empty_when_default() {
        assert!(RemoteLayout::default().is_empty());
    }

    #[test]
    fn remote_layout_is_not_empty_when_any_field_set() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("/x")),
            ..RemoteLayout::default()
        };

        assert!(!l.is_empty());
    }

    #[test]
    fn apply_filter_none_passes_through() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: Some(PathBuf::from("r")),
            skills: Some(PathBuf::from("s")),
            macros: Some(PathBuf::from("m")),
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
            ..RemoteLayout::default()
        };

        let out = apply_filter(l, None);

        assert!(out.agents.is_some() && out.roles.is_some() && out.skills.is_some());
        assert!(out.macros.is_some() && out.functions_tools.is_some() && out.mcp_json.is_some());
    }

    #[test]
    fn apply_filter_functions_keeps_only_tools_not_mcp() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: None,
            skills: Some(PathBuf::from("s")),
            macros: None,
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
            ..RemoteLayout::default()
        };

        let out = apply_filter(l, Some(InstallFilter::Functions));

        assert!(out.agents.is_none());
        assert!(out.skills.is_none());
        assert_eq!(out.functions_tools, Some(PathBuf::from("f")));
        assert!(out.mcp_json.is_none());
    }

    #[test]
    fn apply_filter_mcp_config_keeps_only_mcp_json() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: None,
            skills: Some(PathBuf::from("s")),
            macros: None,
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
            ..RemoteLayout::default()
        };

        let out = apply_filter(l, Some(InstallFilter::McpConfig));

        assert!(out.agents.is_none() && out.skills.is_none() && out.functions_tools.is_none());
        assert_eq!(out.mcp_json, Some(PathBuf::from("j")));
    }

    #[test]
    fn apply_filter_roles_keeps_only_roles() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: Some(PathBuf::from("r")),
            skills: Some(PathBuf::from("s")),
            macros: Some(PathBuf::from("m")),
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
            ..RemoteLayout::default()
        };

        let out = apply_filter(l, Some(InstallFilter::Roles));

        assert_eq!(out.roles, Some(PathBuf::from("r")));
        assert!(out.agents.is_none() && out.skills.is_none() && out.macros.is_none());
        assert!(out.functions_tools.is_none() && out.mcp_json.is_none());
    }

    #[test]
    fn apply_filter_skills_keeps_only_skills() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: Some(PathBuf::from("r")),
            skills: Some(PathBuf::from("s")),
            macros: Some(PathBuf::from("m")),
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
            ..RemoteLayout::default()
        };

        let out = apply_filter(l, Some(InstallFilter::Skills));

        assert_eq!(out.skills, Some(PathBuf::from("s")));
        assert!(out.agents.is_none() && out.roles.is_none() && out.macros.is_none());
        assert!(out.functions_tools.is_none() && out.mcp_json.is_none());
    }

    #[test]
    fn walk_files_skips_dot_git_and_collects_regular_files() {
        let root = fresh_temp_dir("walk-test-");
        touch(&root.join("a.txt"));
        touch(&root.join("sub/b.txt"));
        touch(&root.join(".git/HEAD"));
        touch(&root.join(".git/objects/pack/foo"));

        let mut files = walk_files(&root).unwrap();
        files.sort();
        let rels: Vec<_> = files
            .iter()
            .map(|p| p.strip_prefix(&root).unwrap().to_owned())
            .collect();

        assert_eq!(
            rels,
            vec![PathBuf::from("a.txt"), PathBuf::from("sub/b.txt")]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn walk_files_rejects_symlink() {
        let root = fresh_temp_dir("walk-symlink-test-");
        touch(&root.join("real.txt"));
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let err = walk_files(&root).unwrap_err();

        assert!(
            err.to_string().contains("Symlink not allowed"),
            "got error: {err}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_finds_known_subdirs() {
        let root = fresh_temp_dir("scan-test-");

        fs::create_dir_all(root.join("agents/sample")).unwrap();
        fs::create_dir_all(root.join("roles")).unwrap();
        fs::create_dir_all(root.join("skills")).unwrap();
        fs::create_dir_all(root.join("macros")).unwrap();
        fs::create_dir_all(root.join("functions/tools")).unwrap();
        touch(&root.join("functions/mcp.json"));
        touch(&root.join("README.md"));

        let layout = scan_remote_layout(&root).unwrap();
        assert!(layout.agents.is_some());
        assert!(layout.roles.is_some());
        assert!(layout.skills.is_some());
        assert!(layout.macros.is_some());
        assert!(layout.functions_tools.is_some());
        assert!(layout.mcp_json.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_finds_root_mcp_json() {
        let root = fresh_temp_dir("scan-root-mcp-test-");
        touch(&root.join("mcp.json"));

        let layout = scan_remote_layout(&root).unwrap();

        assert_eq!(layout.mcp_json, Some(root.join("mcp.json")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_prefers_root_mcp_json_over_functions() {
        let root = fresh_temp_dir("scan-mcp-precedence-test-");
        touch(&root.join("mcp.json"));
        fs::create_dir_all(root.join("functions")).unwrap();
        touch(&root.join("functions/mcp.json"));

        let layout = scan_remote_layout(&root).unwrap();

        assert_eq!(layout.mcp_json, Some(root.join("mcp.json")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_finds_skills_only() {
        let root = fresh_temp_dir("scan-skills-only-");
        fs::create_dir_all(root.join("skills/git-master")).unwrap();
        touch(&root.join("skills/git-master/SKILL.md"));

        let layout = scan_remote_layout(&root).unwrap();

        assert!(layout.skills.is_some());
        assert!(layout.agents.is_none());
        assert!(layout.roles.is_none());
        assert!(layout.macros.is_none());
        assert!(layout.functions_tools.is_none());
        assert!(layout.mcp_json.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_ignores_unrelated_files() {
        let root = fresh_temp_dir("scan-unrelated-");
        fs::create_dir_all(root.join("docs")).unwrap();
        touch(&root.join("docs/intro.md"));
        touch(&root.join("README.md"));

        let layout = scan_remote_layout(&root).unwrap();

        assert!(layout.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_file_new_when_dst_missing() {
        let dir = fresh_temp_dir("classify-new-");
        let src = dir.join("src");
        fs::write(&src, b"hello").unwrap();

        let dst = dir.join("dst");

        assert_eq!(classify_file(&src, &dst).unwrap(), PlannedKind::New);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_file_identical_when_bytes_match() {
        let dir = fresh_temp_dir("classify-identical-");
        let src = dir.join("src");
        let dst = dir.join("dst");

        fs::write(&src, b"same bytes").unwrap();
        fs::write(&dst, b"same bytes").unwrap();

        assert_eq!(classify_file(&src, &dst).unwrap(), PlannedKind::Identical);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_file_conflict_when_bytes_differ() {
        let dir = fresh_temp_dir("classify-conflict-");
        let src = dir.join("src");
        let dst = dir.join("dst");

        fs::write(&src, b"version A").unwrap();
        fs::write(&dst, b"version B").unwrap();

        assert_eq!(classify_file(&src, &dst).unwrap(), PlannedKind::Conflict);
        let _ = fs::remove_dir_all(&dir);
    }

    fn write_mcp(path: &Path, json: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, json).unwrap();
    }

    const FIXTURE_REMOTE: &str = r#"{
        "mcpServers": {
            "alpha": {"type": "stdio", "command": "echo", "args": ["a"]},
            "beta":  {"type": "stdio", "command": "echo", "args": ["b"]}
        }
    }"#;

    #[test]
    fn unique_renamed_key_appends_remote_suffix() {
        let map: IndexMap<String, McpServer> = IndexMap::new();
        assert_eq!(unique_renamed_key("foo", &map), "foo-remote");
    }

    #[test]
    fn unique_renamed_key_appends_numeric_when_remote_taken() {
        let mut map: IndexMap<String, McpServer> = IndexMap::new();
        map.insert(
            "foo-remote".to_string(),
            serde_json::from_str(r#"{"type":"stdio","command":"x"}"#).unwrap(),
        );
        assert_eq!(unique_renamed_key("foo", &map), "foo-remote-2");
    }

    #[test]
    #[serial]
    fn merge_into_empty_local_adds_all_remote_servers() {
        let _guard = TestVaultConfigGuard::new("merge-empty");
        let dir = fresh_temp_dir("merge-empty-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(&remote, FIXTURE_REMOTE);

        let report = merge_mcp_json(None, &remote, &target, false, &HashSet::new(), false).unwrap();

        assert_eq!(report.added, vec!["alpha", "beta"]);
        assert!(report.kept_local.is_empty());
        assert!(report.replaced.is_empty());
        assert!(report.renamed.is_empty());
        assert!(target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn merge_force_replaces_local_on_conflict() {
        let _guard = TestVaultConfigGuard::new("merge-force");
        let dir = fresh_temp_dir("merge-force-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(
            &target,
            r#"{"mcpServers": {"alpha": {"type": "stdio", "command": "OLD"}}}"#,
        );
        write_mcp(&remote, FIXTURE_REMOTE);

        let report = merge_mcp_json(
            Some(&target),
            &remote,
            &target,
            true,
            &HashSet::new(),
            false,
        )
        .unwrap();

        assert_eq!(report.added, vec!["beta"]);
        assert_eq!(report.replaced, vec!["alpha"]);

        let written = fs::read_to_string(&target).unwrap();

        assert!(written.contains("\"command\": \"echo\""), "got: {written}");
        assert!(!written.contains("OLD"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_non_tty_conflict_aborts_without_force() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping merge_non_tty_conflict_aborts_without_force: requires non-TTY stdout"
            );
            return;
        }
        let dir = fresh_temp_dir("merge-non-tty-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(
            &target,
            r#"{"mcpServers": {"alpha": {"type": "stdio", "command": "LOCAL"}}}"#,
        );
        write_mcp(&remote, FIXTURE_REMOTE);

        let err = merge_mcp_json(
            Some(&target),
            &remote,
            &target,
            false,
            &HashSet::new(),
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("Refusing to merge non-interactively"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_rejects_invalid_remote_server() {
        let dir = fresh_temp_dir("merge-invalid-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(&remote, r#"{"mcpServers": {"broken": {"type": "stdio"}}}"#);

        let err =
            merge_mcp_json(None, &remote, &target, false, &HashSet::new(), false).unwrap_err();

        assert!(
            format!("{err:#}").contains("missing a \"command\" field"),
            "got: {err:#}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[serial]
    async fn merge_detects_missing_secrets_in_output() {
        if env::var_os(SANDBOX_ENV_FLAG).is_some() {
            eprintln!(
                "Skipping merge_detects_missing_secrets_in_output: secret interpolation is disabled inside a sandbox"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("merge-secret");
        let dir = fresh_temp_dir("merge-secret-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(
            &remote,
            r#"{"mcpServers": {"x": {"type":"stdio","command":"echo","env":{"K":"{{COYOTE_TEST_MERGE_SECRET}}"}}}}"#,
        );

        let report = merge_mcp_json(None, &remote, &target, false, &HashSet::new(), false).unwrap();

        assert_eq!(report.missing_secrets, vec!["COYOTE_TEST_MERGE_SECRET"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn merge_is_idempotent_on_re_run() {
        let _guard = TestVaultConfigGuard::new("merge-idempotent");
        let dir = fresh_temp_dir("merge-idempotent-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(&remote, FIXTURE_REMOTE);

        merge_mcp_json(None, &remote, &target, false, &HashSet::new(), false).unwrap();
        let after_first = fs::read(&target).unwrap();

        let report = merge_mcp_json(
            Some(&target),
            &remote,
            &target,
            false,
            &HashSet::new(),
            false,
        )
        .unwrap();
        assert!(report.added.is_empty(), "got: {:?}", report.added);
        let after_second = fs::read(&target).unwrap();

        assert_eq!(after_first, after_second);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_missing_secrets_noop_on_empty_input() {
        assert!(handle_missing_secrets(&[]).is_ok());
    }

    #[test]
    fn handle_missing_secrets_defers_all_in_non_tty() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping handle_missing_secrets_defers_all_in_non_tty: requires non-TTY stdout"
            );
            return;
        }
        let missing = vec![
            "COYOTE_TEST_STEP4_A".to_string(),
            "COYOTE_TEST_STEP4_B".to_string(),
        ];

        assert!(handle_missing_secrets(&missing).is_ok());
    }

    fn write_manifest(root: &Path, yaml: &str) {
        fs::write(root.join(BUNDLE_MANIFEST_FILE), yaml).unwrap();
    }

    #[test]
    fn scan_remote_layout_without_manifest_has_none() {
        let root = fresh_temp_dir("scan-no-manifest-");
        fs::create_dir_all(root.join("macros")).unwrap();

        let layout = scan_remote_layout(&root).unwrap();

        assert_eq!(layout.manifest, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_parses_full_manifest_and_ignores_unknown_fields() {
        let root = fresh_temp_dir("scan-manifest-full-");
        fs::create_dir_all(root.join("macros")).unwrap();
        write_manifest(
            &root,
            "name: oh-my-coyote\n\
             version: \"1.4.0\"\n\
             description: Opinionated roles and macros\n\
             homepage: https://github.com/x/oh-my-coyote\n\
             future_field: ignored\n",
        );

        let manifest = scan_remote_layout(&root).unwrap().manifest.unwrap();

        assert_eq!(manifest.name, "oh-my-coyote");
        assert_eq!(manifest.version.as_deref(), Some("1.4.0"));
        assert_eq!(
            manifest.description.as_deref(),
            Some("Opinionated roles and macros")
        );
        assert_eq!(
            manifest.homepage.as_deref(),
            Some("https://github.com/x/oh-my-coyote")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_parses_name_only_manifest() {
        let root = fresh_temp_dir("scan-manifest-minimal-");
        write_manifest(&root, "name: minimal\n");

        let manifest = scan_remote_layout(&root).unwrap().manifest.unwrap();

        assert_eq!(manifest.name, "minimal");
        assert_eq!(manifest.version, None);
        assert_eq!(manifest.description, None);
        assert_eq!(manifest.homepage, None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_fails_on_malformed_manifest() {
        let root = fresh_temp_dir("scan-manifest-malformed-");
        write_manifest(&root, "name: [unclosed\n");

        let err = scan_remote_layout(&root).unwrap_err();

        assert!(
            format!("{err:#}").contains("invalid bundle manifest"),
            "got: {err:#}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_fails_on_manifest_missing_name() {
        let root = fresh_temp_dir("scan-manifest-no-name-");
        write_manifest(&root, "version: \"1.0\"\n");

        let err = scan_remote_layout(&root).unwrap_err();

        assert!(
            format!("{err:#}").contains("invalid bundle manifest"),
            "got: {err:#}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_remote_layout_fails_on_invalid_manifest_name() {
        let root = fresh_temp_dir("scan-manifest-bad-name-");
        write_manifest(&root, "name: not a valid name\n");

        let err = scan_remote_layout(&root).unwrap_err();

        assert!(
            format!("{err:#}").contains("Invalid bundle name"),
            "got: {err:#}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_filter_carries_manifest_and_head_sha_through() {
        let l = RemoteLayout {
            macros: Some(PathBuf::from("m")),
            manifest: Some(BundleManifest {
                name: "bundle".to_string(),
                version: None,
                description: None,
                homepage: None,
            }),
            head_sha: Some("abc123".to_string()),
            ..RemoteLayout::default()
        };

        let out = apply_filter(l, Some(InstallFilter::Macros));

        assert_eq!(out.macros, Some(PathBuf::from("m")));
        assert_eq!(out.manifest.unwrap().name, "bundle");
        assert_eq!(out.head_sha.as_deref(), Some("abc123"));
    }

    #[test]
    fn validate_bundle_name_accepts_simple_names() {
        assert!(validate_bundle_name("oh-my-coyote").is_ok());
        assert!(validate_bundle_name("under_score").is_ok());
        assert!(validate_bundle_name("Abc123").is_ok());
    }

    #[test]
    fn validate_bundle_name_accepts_owner_qualified_form() {
        assert!(validate_bundle_name("x/oh-my-coyote").is_ok());
    }

    #[test]
    fn validate_bundle_name_rejects_empty() {
        assert!(validate_bundle_name("").is_err());
    }

    #[test]
    fn validate_bundle_name_rejects_multiple_slashes() {
        assert!(validate_bundle_name("a/b/c").is_err());
    }

    #[test]
    fn validate_bundle_name_rejects_empty_segments() {
        assert!(validate_bundle_name("/repo").is_err());
        assert!(validate_bundle_name("owner/").is_err());
        assert!(validate_bundle_name("/").is_err());
    }

    #[test]
    fn validate_bundle_name_rejects_disallowed_characters() {
        assert!(validate_bundle_name("bad name").is_err());
        assert!(validate_bundle_name("dot.name").is_err());
        assert!(validate_bundle_name("owner/bad!base").is_err());
    }

    #[test]
    fn repo_name_slug_from_https_url() {
        assert_eq!(
            repo_name_slug("https://github.com/x/oh-my-coyote"),
            "oh-my-coyote"
        );
    }

    #[test]
    fn repo_name_slug_strips_git_suffix() {
        assert_eq!(
            repo_name_slug("https://github.com/x/oh-my-coyote.git"),
            "oh-my-coyote"
        );
    }

    #[test]
    fn repo_name_slug_from_scp_style_url() {
        assert_eq!(
            repo_name_slug("git@github.com:x/oh-my-coyote.git"),
            "oh-my-coyote"
        );
    }

    #[test]
    fn repo_name_slug_ignores_ref_suffix() {
        assert_eq!(
            repo_name_slug("https://github.com/x/repo.git#release/v2"),
            "repo"
        );
    }

    #[test]
    fn repo_name_slug_ignores_trailing_slash() {
        assert_eq!(repo_name_slug("https://github.com/x/repo/"), "repo");
    }

    #[test]
    fn owner_qualifier_from_https_url() {
        assert_eq!(
            owner_qualifier("https://github.com/x/repo.git").as_deref(),
            Some("x")
        );
    }

    #[test]
    fn owner_qualifier_from_scp_style_url() {
        assert_eq!(
            owner_qualifier("git@github.com:x/repo.git").as_deref(),
            Some("x")
        );
    }

    #[test]
    fn owner_qualifier_falls_back_to_sanitized_host() {
        assert_eq!(
            owner_qualifier("https://example.com/repo.git").as_deref(),
            Some("example-com")
        );
    }

    #[test]
    fn owner_qualifier_scp_without_owner_falls_back_to_host() {
        assert_eq!(
            owner_qualifier("git@host.example.com:repo.git").as_deref(),
            Some("host-example-com")
        );
    }

    #[test]
    fn owner_qualifier_none_without_owner_or_host() {
        assert_eq!(owner_qualifier("repo"), None);
    }

    #[test]
    fn canonical_source_url_treats_equivalent_forms_identically() {
        let canonical = canonical_source_url("https://github.com/x/r");

        assert_eq!(canonical, "github.com/x/r");
        assert_eq!(
            canonical_source_url("https://github.com/x/r.git"),
            canonical
        );
        assert_eq!(canonical_source_url("git@github.com:x/r.git"), canonical);
    }

    #[test]
    fn canonical_source_url_lowercases_host_but_not_path() {
        assert_eq!(
            canonical_source_url("https://GitHub.COM/X/R.git"),
            "github.com/X/R"
        );
        assert_ne!(
            canonical_source_url("https://gitlab.example.com/team/Repo"),
            canonical_source_url("https://gitlab.example.com/team/repo")
        );
    }

    #[test]
    fn canonical_source_url_ignores_ref_suffix() {
        assert_eq!(
            canonical_source_url("https://github.com/x/r.git#main"),
            "github.com/x/r"
        );
    }

    #[test]
    fn canonical_source_url_strips_userinfo() {
        assert_eq!(
            canonical_source_url("https://user@github.com/x/r.git"),
            "github.com/x/r"
        );
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let mut full: Vec<OsString> = vec!["-C".into(), dir.as_os_str().into()];
        full.extend(args.iter().map(OsString::from));
        run_git(full).unwrap();
    }

    fn commit_file(dir: &Path, name: &str, content: &str) -> String {
        fs::write(dir.join(name), content).unwrap();
        git_in(dir, &["add", "."]);
        git_in(
            dir,
            &[
                "-c",
                "user.email=coyote-test@localhost",
                "-c",
                "user.name=coyote-test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                name,
            ],
        );
        run_git_capture(vec![
            "-C".into(),
            dir.as_os_str().into(),
            "rev-parse".into(),
            "HEAD".into(),
        ])
        .unwrap()
    }

    fn init_git_repo(dir: &Path) -> String {
        run_git(vec!["init".into(), "-q".into(), dir.as_os_str().into()]).unwrap();
        fs::write(dir.join(".gitattributes"), "* -text\n").unwrap();
        commit_file(dir, "seed.txt", "one")
    }

    #[test]
    fn clone_to_temp_captures_resolved_head_sha() {
        let repo = fresh_temp_dir("clone-sha-");
        let sha = init_git_repo(&repo);

        let temp = clone_to_temp(repo.to_str().unwrap(), None).unwrap();

        assert_eq!(temp.head_sha(), sha);
        assert_eq!(temp.head_sha().len(), 40);
        assert!(temp.head_sha().chars().all(|c| c.is_ascii_hexdigit()));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn clone_to_temp_respects_sha_ref_pinning() {
        let repo = fresh_temp_dir("clone-pin-sha-");
        let first = init_git_repo(&repo);
        let second = commit_file(&repo, "next.txt", "two");
        assert_ne!(first, second);

        let temp = clone_to_temp(repo.to_str().unwrap(), Some(&first)).unwrap();

        assert_eq!(temp.head_sha(), first);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn clone_to_temp_respects_branch_ref_pinning() {
        let repo = fresh_temp_dir("clone-pin-branch-");
        let first = init_git_repo(&repo);
        git_in(&repo, &["branch", "pinned"]);
        let second = commit_file(&repo, "next.txt", "two");
        assert_ne!(first, second);

        let temp = clone_to_temp(repo.to_str().unwrap(), Some("pinned")).unwrap();

        assert_eq!(temp.head_sha(), first);
        let _ = fs::remove_dir_all(&repo);
    }

    fn write_src(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn init_bundle_repo(dir: &Path) -> String {
        run_git(vec!["init".into(), "-q".into(), dir.as_os_str().into()]).unwrap();
        fs::write(dir.join(".gitattributes"), "* -text\n").unwrap();
        commit_file(dir, ".seed", "seed")
    }

    fn test_metadata(source: &str) -> InstallMetadata {
        InstallMetadata {
            source: source.to_string(),
            git_ref: None,
            commit: "abc123".to_string(),
            version: None,
            description: None,
            homepage: None,
        }
    }

    #[test]
    #[serial]
    fn install_remote_records_files_mcp_entries_and_metadata() {
        use crate::config::bundles::hash_bytes;

        let _guard = TestVaultConfigGuard::new("prov-full");
        let src_root = fresh_temp_dir("prov-full-src-");
        let repo = src_root.join("my-bundle");
        write_src(
            &repo,
            BUNDLE_MANIFEST_FILE,
            "name: my-bundle\n\
             version: \"2.0\"\n\
             description: Test bundle\n\
             homepage: https://example.com/my-bundle\n",
        );
        write_src(&repo, "macros/hello.yaml", "name: hello\n");
        write_src(
            &repo,
            "functions/mcp.json",
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "echo"}}}"#,
        );
        let sha = init_bundle_repo(&repo);

        install_remote(repo.to_str().unwrap(), None, false).unwrap();

        let store = BundleStore::load().unwrap();
        let record = store.get("my-bundle").unwrap();
        assert_eq!(record.source, repo.to_str().unwrap());
        assert_eq!(record.git_ref, None);
        assert_eq!(record.commit, sha);
        assert_eq!(record.version.as_deref(), Some("2.0"));
        assert_eq!(record.description.as_deref(), Some("Test bundle"));
        assert_eq!(
            record.homepage.as_deref(),
            Some("https://example.com/my-bundle")
        );
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.files[0].path, "macros/hello.yaml");
        assert_eq!(record.files[0].category, "macros");
        assert_eq!(record.files[0].action, FileAction::New);
        assert_eq!(record.files[0].sha256, hash_bytes(b"name: hello\n"));
        assert_eq!(record.mcp_servers.len(), 1);
        assert_eq!(record.mcp_servers[0].name, "srv");
        assert_eq!(record.mcp_servers[0].action, McpAction::Added);
        assert_eq!(record.mcp_servers[0].renamed_to, None);
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn install_remote_without_manifest_uses_repo_slug_and_short_sha_version() {
        let _guard = TestVaultConfigGuard::new("prov-slug");
        let src_root = fresh_temp_dir("prov-slug-src-");
        let repo = src_root.join("plain.bundle");
        write_src(&repo, "macros/m.yaml", "a: 1\n");
        let sha = init_bundle_repo(&repo);

        install_remote(&format!("{}#{sha}", repo.display()), None, false).unwrap();

        let store = BundleStore::load().unwrap();
        let record = store.get("plain-bundle").unwrap();
        assert_eq!(record.git_ref.as_deref(), Some(sha.as_str()));
        assert_eq!(record.commit, sha);
        assert_eq!(record.version.as_deref(), Some(&sha[..7]));
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn apply_plan_records_files_written_before_a_mid_run_abort() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping apply_plan_records_files_written_before_a_mid_run_abort: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("prov-abort");
        let src_dir = fresh_temp_dir("prov-abort-src-");
        let src_new = src_dir.join("new.yaml");
        let src_conflict = src_dir.join("conflict.yaml");
        fs::write(&src_new, "new content").unwrap();
        fs::write(&src_conflict, "remote content").unwrap();
        let dst_new = paths::macros_dir().join("new.yaml");
        let dst_conflict = paths::macros_dir().join("conflict.yaml");
        fs::create_dir_all(paths::macros_dir()).unwrap();
        fs::write(&dst_conflict, "local content").unwrap();
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("aborty", test_metadata("https://github.com/x/aborty"))
            .unwrap();
        let plan = InstallPlan {
            files: vec![
                PlannedFile {
                    src: src_new,
                    dst: dst_new.clone(),
                    kind: PlannedKind::New,
                    top_category: TopCategory::Macros,
                },
                PlannedFile {
                    src: src_conflict,
                    dst: dst_conflict.clone(),
                    kind: PlannedKind::Conflict,
                    top_category: TopCategory::Macros,
                },
            ],
            mcp_json: None,
        };

        let err = apply_plan(&plan, StickyMode::None, &mut store, "aborty").unwrap_err();

        assert!(
            err.to_string().contains("Refusing to overwrite"),
            "got: {err}"
        );
        assert_eq!(fs::read_to_string(&dst_new).unwrap(), "new content");
        assert_eq!(fs::read_to_string(&dst_conflict).unwrap(), "local content");
        let reloaded = BundleStore::load().unwrap();
        let files = &reloaded.get("aborty").unwrap().files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "macros/new.yaml");
        assert_eq!(files[0].action, FileAction::New);
        let _ = fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn apply_plan_keep_all_leaves_prior_owner_intact() {
        let dir = fresh_temp_dir("prov-keep-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("alpha", test_metadata("https://github.com/a/alpha"))
            .unwrap();
        store
            .upsert_bundle("beta", test_metadata("https://github.com/b/beta"))
            .unwrap();
        let dst = dir.join("macros/owned.yaml");
        write_src(&dir, "macros/owned.yaml", "alpha content");
        store
            .record_file(
                "alpha",
                FileRecord {
                    path: "macros/owned.yaml".to_string(),
                    category: "macros".to_string(),
                    sha256: hash_file(&dst).unwrap(),
                    action: FileAction::New,
                },
            )
            .unwrap();
        let src = dir.join("beta-src/owned.yaml");
        write_src(&dir, "beta-src/owned.yaml", "beta content");
        let plan = InstallPlan {
            files: vec![PlannedFile {
                src,
                dst: dst.clone(),
                kind: PlannedKind::Conflict,
                top_category: TopCategory::Macros,
            }],
            mcp_json: None,
        };

        apply_plan(&plan, StickyMode::KeepAll, &mut store, "beta").unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "alpha content");
        let reloaded = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        assert_eq!(reloaded.get("alpha").unwrap().files.len(), 1);
        assert!(reloaded.get("beta").unwrap().files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn second_install_transfers_file_and_mcp_ownership() {
        use crate::config::bundles::hash_bytes;

        let _guard = TestVaultConfigGuard::new("prov-transfer");
        let src_root = fresh_temp_dir("prov-transfer-src-");
        let alpha = src_root.join("alpha");
        write_src(&alpha, "macros/shared.yaml", "from: alpha\n");
        write_src(
            &alpha,
            "functions/mcp.json",
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "alpha"}}}"#,
        );
        init_bundle_repo(&alpha);
        let beta = src_root.join("beta");
        write_src(&beta, "macros/shared.yaml", "from: beta\n");
        write_src(
            &beta,
            "functions/mcp.json",
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "beta"}}}"#,
        );
        init_bundle_repo(&beta);

        install_remote(alpha.to_str().unwrap(), None, false).unwrap();
        install_remote(beta.to_str().unwrap(), None, true).unwrap();

        let store = BundleStore::load().unwrap();
        let alpha_record = store.get("alpha").unwrap();
        assert!(alpha_record.files.is_empty());
        assert!(alpha_record.mcp_servers.is_empty());
        let beta_record = store.get("beta").unwrap();
        assert_eq!(beta_record.files.len(), 1);
        assert_eq!(beta_record.files[0].path, "macros/shared.yaml");
        assert_eq!(beta_record.files[0].action, FileAction::Replaced);
        assert_eq!(beta_record.files[0].sha256, hash_bytes(b"from: beta\n"));
        assert_eq!(beta_record.mcp_servers.len(), 1);
        assert_eq!(beta_record.mcp_servers[0].name, "srv");
        assert_eq!(beta_record.mcp_servers[0].action, McpAction::Transferred);
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn filtered_installs_merge_into_one_record() {
        let _guard = TestVaultConfigGuard::new("prov-filter");
        let src_root = fresh_temp_dir("prov-filter-src-");
        let repo = src_root.join("combo");
        write_src(&repo, "macros/m.yaml", "a: 1\n");
        write_src(&repo, "skills/myskill/SKILL.md", "# skill\n");
        let first_sha = init_bundle_repo(&repo);

        install_remote(repo.to_str().unwrap(), Some(InstallFilter::Macros), false).unwrap();
        let second_sha = commit_file(&repo, "extra.txt", "two");
        install_remote(repo.to_str().unwrap(), Some(InstallFilter::Skills), false).unwrap();

        let store = BundleStore::load().unwrap();
        assert_eq!(store.bundle_names(), vec!["combo"]);
        let record = store.get("combo").unwrap();
        assert_ne!(first_sha, second_sha);
        assert_eq!(record.commit, second_sha);
        let mut owned: Vec<&str> = record.files.iter().map(|f| f.path.as_str()).collect();
        owned.sort();
        assert_eq!(owned, vec!["macros/m.yaml", "skills/myskill/SKILL.md"]);
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    fn record_mcp_merge_ignores_kept_local_entries() {
        let dir = fresh_temp_dir("prov-mcp-keep-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("alpha", test_metadata("https://github.com/a/alpha"))
            .unwrap();
        store
            .upsert_bundle("beta", test_metadata("https://github.com/b/beta"))
            .unwrap();
        store
            .record_mcp_servers(
                "alpha",
                vec![McpServerRecord {
                    name: "srv".to_string(),
                    action: McpAction::Added,
                    renamed_to: None,
                    sha256: None,
                }],
            )
            .unwrap();
        let report = McpMergeReport {
            added: Vec::new(),
            kept_local: vec!["srv".to_string()],
            replaced: Vec::new(),
            renamed: Vec::new(),
            entry_hashes: HashMap::new(),
            final_path: dir.join("mcp.json"),
            missing_secrets: Vec::new(),
        };

        record_mcp_merge(&mut store, "beta", &report).unwrap();

        assert_eq!(store.get("alpha").unwrap().mcp_servers.len(), 1);
        assert!(store.get("beta").unwrap().mcp_servers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn builtin_asset_install_writes_no_provenance() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping builtin_asset_install_writes_no_provenance: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("prov-builtin");

        crate::config::install_assets(crate::config::AssetCategory::Macros).unwrap();

        assert!(fs::read_dir(paths::macros_dir()).unwrap().next().is_some());
        assert!(!paths::installed_bundles_file().exists());
    }

    #[test]
    #[serial]
    fn update_silently_refreshes_owned_unmodified_files() {
        use crate::config::bundles::hash_bytes;

        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping update_silently_refreshes_owned_unmodified_files: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("upd-refresh");
        let src_root = fresh_temp_dir("upd-refresh-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: refresh-bundle\n");
        write_src(&repo, "macros/hello.yaml", "v1\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();
        commit_file(&repo, "macros/hello.yaml", "v2\n");

        update_bundle("refresh-bundle", false).unwrap();

        let installed = paths::macros_dir().join("hello.yaml");
        assert_eq!(fs::read_to_string(&installed).unwrap(), "v2\n");
        let store = BundleStore::load().unwrap();
        let files = &store.get("refresh-bundle").unwrap().files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].sha256, hash_bytes(b"v2\n"));
        assert_eq!(files[0].action, FileAction::Replaced);
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn update_with_yes_keeps_modified_files_and_updates_the_rest() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping update_with_yes_keeps_modified_files_and_updates_the_rest: \
                 requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("upd-yes");
        let src_root = fresh_temp_dir("upd-yes-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: yes-bundle\n");
        write_src(&repo, "macros/edited.yaml", "v1\n");
        write_src(&repo, "macros/pristine.yaml", "v1\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();
        fs::write(paths::macros_dir().join("edited.yaml"), "local\n").unwrap();
        commit_file(&repo, "macros/edited.yaml", "v2\n");
        commit_file(&repo, "macros/pristine.yaml", "v2\n");

        let err = update_bundle("yes-bundle", false).unwrap_err();
        assert!(
            format!("{err:#}").contains("Refusing to overwrite"),
            "err: {err:#}"
        );

        update_bundle("yes-bundle", true).unwrap();

        assert_eq!(
            fs::read_to_string(paths::macros_dir().join("edited.yaml")).unwrap(),
            "local\n"
        );
        assert_eq!(
            fs::read_to_string(paths::macros_dir().join("pristine.yaml")).unwrap(),
            "v2\n"
        );
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn update_takes_remote_for_owned_unmodified_mcp_entries() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping update_takes_remote_for_owned_unmodified_mcp_entries: \
                 requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("upd-mcp-auto");
        let src_root = fresh_temp_dir("upd-mcp-auto-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: mcp-auto-bundle\n");
        write_src(
            &repo,
            "functions/mcp.json",
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "echo"}}}"#,
        );
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();
        commit_file(
            &repo,
            "functions/mcp.json",
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "printf"}}}"#,
        );

        update_bundle("mcp-auto-bundle", false).unwrap();

        let merged = fs::read_to_string(paths::mcp_config_file()).unwrap();
        assert!(merged.contains("printf"), "merged mcp.json: {merged}");
        assert!(!merged.contains("echo"), "merged mcp.json: {merged}");
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    fn apply_plan_keep_all_preserves_local_edit_and_stale_record() {
        let dir = fresh_temp_dir("upd-keep-modified-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let dst = dir.join("macros/owned.yaml");
        write_src(&dir, "macros/owned.yaml", "installed content");
        let recorded_sha = hash_file(&dst).unwrap();
        store
            .record_file(
                "omc",
                FileRecord {
                    path: provenance_path(&dst),
                    category: "macros".to_string(),
                    sha256: recorded_sha.clone(),
                    action: FileAction::New,
                },
            )
            .unwrap();
        fs::write(&dst, "local edit").unwrap();
        let src = dir.join("upstream/owned.yaml");
        write_src(&dir, "upstream/owned.yaml", "upstream content");
        let plan = InstallPlan {
            files: vec![PlannedFile {
                src,
                dst: dst.clone(),
                kind: PlannedKind::Conflict,
                top_category: TopCategory::Macros,
            }],
            mcp_json: None,
        };

        apply_plan(&plan, StickyMode::KeepAll, &mut store, "omc").unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "local edit");
        let reloaded = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        let files = &reloaded.get("omc").unwrap().files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].sha256, recorded_sha);
        assert_ne!(files[0].sha256, hash_file(&dst).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_plan_replace_all_takes_upstream_and_rerecords_hash() {
        use crate::config::bundles::hash_bytes;

        let dir = fresh_temp_dir("upd-replace-modified-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let dst = dir.join("macros/owned.yaml");
        write_src(&dir, "macros/owned.yaml", "installed content");
        store
            .record_file(
                "omc",
                FileRecord {
                    path: provenance_path(&dst),
                    category: "macros".to_string(),
                    sha256: hash_file(&dst).unwrap(),
                    action: FileAction::New,
                },
            )
            .unwrap();
        fs::write(&dst, "local edit").unwrap();
        let src = dir.join("upstream/owned.yaml");
        write_src(&dir, "upstream/owned.yaml", "upstream content");
        let plan = InstallPlan {
            files: vec![PlannedFile {
                src,
                dst: dst.clone(),
                kind: PlannedKind::Conflict,
                top_category: TopCategory::Macros,
            }],
            mcp_json: None,
        };

        apply_plan(&plan, StickyMode::ReplaceAll, &mut store, "omc").unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "upstream content");
        let reloaded = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        let files = &reloaded.get("omc").unwrap().files;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].sha256, hash_bytes(b"upstream content"));
        assert_eq!(files[0].action, FileAction::Replaced);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn update_bails_non_interactively_on_unowned_conflict() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping update_bails_non_interactively_on_unowned_conflict: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("upd-unowned");
        let src_root = fresh_temp_dir("upd-unowned-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: unowned-bundle\n");
        write_src(&repo, "macros/a.yaml", "a\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();
        commit_file(&repo, "macros/user.yaml", "upstream\n");
        fs::create_dir_all(paths::macros_dir()).unwrap();
        fs::write(paths::macros_dir().join("user.yaml"), "local\n").unwrap();

        let err = update_bundle("unowned-bundle", false).unwrap_err();

        assert!(
            err.to_string().contains("Refusing to overwrite"),
            "got: {err}"
        );
        assert_eq!(
            fs::read_to_string(paths::macros_dir().join("user.yaml")).unwrap(),
            "local\n"
        );
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    fn apply_plan_replace_all_transfers_ownership_between_bundles() {
        let dir = fresh_temp_dir("upd-transfer-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("alpha", test_metadata("https://github.com/a/alpha"))
            .unwrap();
        store
            .upsert_bundle("beta", test_metadata("https://github.com/b/beta"))
            .unwrap();
        let dst = dir.join("macros/shared.yaml");
        write_src(&dir, "macros/shared.yaml", "alpha content");
        store
            .record_file(
                "alpha",
                FileRecord {
                    path: provenance_path(&dst),
                    category: "macros".to_string(),
                    sha256: hash_file(&dst).unwrap(),
                    action: FileAction::New,
                },
            )
            .unwrap();
        let src = dir.join("beta-src/shared.yaml");
        write_src(&dir, "beta-src/shared.yaml", "beta content");
        let plan = InstallPlan {
            files: vec![PlannedFile {
                src,
                dst: dst.clone(),
                kind: PlannedKind::Conflict,
                top_category: TopCategory::Macros,
            }],
            mcp_json: None,
        };

        apply_plan(&plan, StickyMode::ReplaceAll, &mut store, "beta").unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "beta content");
        let reloaded = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        assert!(reloaded.get("alpha").unwrap().files.is_empty());
        let beta_files = &reloaded.get("beta").unwrap().files;
        assert_eq!(beta_files.len(), 1);
        assert_eq!(beta_files[0].path, provenance_path(&dst));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn update_keeps_obsolete_files_non_interactively() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping update_keeps_obsolete_files_non_interactively: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("upd-obsolete-keep");
        let src_root = fresh_temp_dir("upd-obsolete-keep-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: obs-keep\n");
        write_src(&repo, "macros/keep.yaml", "k\n");
        write_src(&repo, "macros/gone.yaml", "g\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();
        fs::remove_file(repo.join("macros/gone.yaml")).unwrap();
        commit_file(&repo, "macros/keep.yaml", "k2\n");

        update_bundle("obs-keep", false).unwrap();

        assert_eq!(
            fs::read_to_string(paths::macros_dir().join("gone.yaml")).unwrap(),
            "g\n"
        );
        let store = BundleStore::load().unwrap();
        let record = store.get("obs-keep").unwrap();
        assert!(record.files.iter().any(|f| f.path == "macros/gone.yaml"));
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    fn apply_obsolete_delete_removes_file_and_record() {
        let dir = fresh_temp_dir("upd-obsolete-delete-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let dst = dir.join("macros/gone.yaml");
        write_src(&dir, "macros/gone.yaml", "g");
        let path = provenance_path(&dst);
        store
            .record_file(
                "omc",
                FileRecord {
                    path: path.clone(),
                    category: "macros".to_string(),
                    sha256: hash_file(&dst).unwrap(),
                    action: FileAction::New,
                },
            )
            .unwrap();

        apply_obsolete_action(&mut store, "omc", &path, &dst, &dir, ObsoleteAction::Delete)
            .unwrap();

        assert!(!dst.exists());
        let reloaded = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        assert!(reloaded.get("omc").unwrap().files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_obsolete_drops_records_for_missing_files() {
        let dir = fresh_temp_dir("upd-obsolete-missing-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        store
            .record_file(
                "omc",
                FileRecord {
                    path: "macros/ghost-bundle-test.yaml".to_string(),
                    category: "macros".to_string(),
                    sha256: "0".repeat(64),
                    action: FileAction::New,
                },
            )
            .unwrap();
        let plan = InstallPlan {
            files: Vec::new(),
            mcp_json: None,
        };

        handle_obsolete_files(&mut store, "omc", &plan, false).unwrap();

        assert!(store.get("omc").unwrap().files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_obsolete_never_touches_suspicious_recorded_paths() {
        let dir = fresh_temp_dir("upd-obsolete-suspicious-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        for hostile in ["/etc/nonexistent-coyote-test", "../escape.yaml"] {
            store
                .record_file(
                    "omc",
                    FileRecord {
                        path: hostile.to_string(),
                        category: "macros".to_string(),
                        sha256: "0".repeat(64),
                        action: FileAction::New,
                    },
                )
                .unwrap();
        }
        let plan = InstallPlan {
            files: Vec::new(),
            mcp_json: None,
        };

        handle_obsolete_files(&mut store, "omc", &plan, false).unwrap();

        assert_eq!(store.get("omc").unwrap().files.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn update_refreshes_record_metadata_and_stamps_updated_at() {
        let _guard = TestVaultConfigGuard::new("upd-meta");
        let src_root = fresh_temp_dir("upd-meta-src-");
        let repo = src_root.join("bundle");
        write_src(
            &repo,
            BUNDLE_MANIFEST_FILE,
            "name: meta-bundle\n\
             version: \"1.0\"\n\
             description: Old\n\
             homepage: https://example.com/old\n",
        );
        write_src(&repo, "macros/m.yaml", "a: 1\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();
        let installed_at = BundleStore::load()
            .unwrap()
            .get("meta-bundle")
            .unwrap()
            .installed_at
            .clone();
        let new_sha = commit_file(
            &repo,
            BUNDLE_MANIFEST_FILE,
            "name: meta-bundle\n\
             version: \"2.0\"\n\
             description: New\n\
             homepage: https://example.com/new\n",
        );

        update_bundle("meta-bundle", false).unwrap();

        let store = BundleStore::load().unwrap();
        let record = store.get("meta-bundle").unwrap();
        assert_eq!(record.commit, new_sha);
        assert_eq!(record.version.as_deref(), Some("2.0"));
        assert_eq!(record.description.as_deref(), Some("New"));
        assert_eq!(record.homepage.as_deref(), Some("https://example.com/new"));
        assert!(record.updated_at.is_some());
        assert_eq!(record.installed_at, installed_at);
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn update_unknown_bundle_lists_installed_names() {
        let _guard = TestVaultConfigGuard::new("upd-unknown-name");

        let err = update_bundle("nope", false).unwrap_err();
        assert!(err.to_string().contains("none are installed"), "got: {err}");

        let src_root = fresh_temp_dir("upd-unknown-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: known-bundle\n");
        write_src(&repo, "macros/m.yaml", "a: 1\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();

        let err = update_bundle("nope", false).unwrap_err();

        assert!(
            err.to_string().contains("installed bundles: known-bundle"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn update_honors_recorded_sha_pin() {
        let _guard = TestVaultConfigGuard::new("upd-pin");
        let src_root = fresh_temp_dir("upd-pin-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: pin-bundle\n");
        write_src(&repo, "macros/one.yaml", "1\n");
        let pinned = init_bundle_repo(&repo);
        install_remote(&format!("{}#{pinned}", repo.display()), None, false).unwrap();
        let newer = commit_file(&repo, "macros/two.yaml", "2\n");
        assert_ne!(pinned, newer);

        update_bundle("pin-bundle", false).unwrap();

        let store = BundleStore::load().unwrap();
        let record = store.get("pin-bundle").unwrap();
        assert_eq!(record.commit, pinned);
        assert_eq!(record.git_ref.as_deref(), Some(pinned.as_str()));
        assert!(!paths::macros_dir().join("two.yaml").exists());
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn update_ref_override_moves_the_pin() {
        let _guard = TestVaultConfigGuard::new("upd-move-pin");
        let src_root = fresh_temp_dir("upd-move-pin-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: move-bundle\n");
        write_src(&repo, "macros/one.yaml", "1\n");
        let pinned = init_bundle_repo(&repo);
        install_remote(&format!("{}#{pinned}", repo.display()), None, false).unwrap();
        let newer = commit_file(&repo, "macros/two.yaml", "2\n");

        update_bundle(&format!("move-bundle#{newer}"), false).unwrap();

        let store = BundleStore::load().unwrap();
        let record = store.get("move-bundle").unwrap();
        assert_eq!(record.commit, newer);
        assert_eq!(record.git_ref.as_deref(), Some(newer.as_str()));
        assert_eq!(
            fs::read_to_string(paths::macros_dir().join("two.yaml")).unwrap(),
            "2\n"
        );
        let _ = fs::remove_dir_all(&src_root);
    }

    fn owned_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_urls_and_paths_as_remote_sources() {
        for value in [
            "https://github.com/x/y",
            "git@github.com:x/y.git",
            "./bundle-dir",
            "../up",
            "/abs/path",
            "~/home-rel",
        ] {
            assert_eq!(
                classify_install_target(value, &[]),
                InstallTarget::RemoteSource,
                "value: {value}"
            );
        }
    }

    #[test]
    fn classify_categories_win_over_installed_names() {
        assert_eq!(
            classify_install_target("agents", &owned_names(&["agents"])),
            InstallTarget::Category(AssetCategory::Agents)
        );
        assert_eq!(
            classify_install_target("mcp_config", &[]),
            InstallTarget::Category(AssetCategory::McpConfig)
        );
    }

    #[test]
    fn classify_installed_names_with_optional_ref() {
        let installed = owned_names(&["omc"]);
        assert_eq!(
            classify_install_target("omc", &installed),
            InstallTarget::InstalledBundle
        );
        assert_eq!(
            classify_install_target("omc#v2", &installed),
            InstallTarget::InstalledBundle
        );
    }

    #[test]
    fn classify_bare_names_without_a_match_as_unknown() {
        assert_eq!(classify_install_target("omc", &[]), InstallTarget::Unknown);
        assert_eq!(
            classify_install_target("not-a-bundle", &owned_names(&["omc"])),
            InstallTarget::Unknown
        );
    }

    #[test]
    #[serial]
    fn install_or_update_redirects_categories_to_install_builtins() {
        let _guard = TestVaultConfigGuard::new("iou-category");

        let err = install_or_update("agents", None, None, false).unwrap_err();

        assert!(
            err.to_string().contains("--install-builtins agents"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn install_or_update_unknown_name_lists_installed_bundles() {
        let _guard = TestVaultConfigGuard::new("iou-unknown");

        let err = install_or_update("not-a-bundle", None, None, false).unwrap_err();
        assert!(err.to_string().contains("none are installed"), "got: {err}");

        let src_root = fresh_temp_dir("iou-unknown-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: known-bundle\n");
        write_src(&repo, "macros/m.yaml", "a: 1\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();

        let err = install_or_update("not-a-bundle", None, None, false).unwrap_err();

        assert!(
            err.to_string().contains("no bundle named 'not-a-bundle'"),
            "got: {err}"
        );
        assert!(
            err.to_string().contains("installed bundles: known-bundle"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    #[serial]
    fn install_or_update_category_error_hints_update_for_shadowed_bundle() {
        let _guard = TestVaultConfigGuard::new("iou-shadow");

        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle(
                "agents",
                InstallMetadata {
                    source: "https://github.com/x/agents".to_string(),
                    git_ref: None,
                    commit: "abc123".to_string(),
                    version: None,
                    description: None,
                    homepage: None,
                },
            )
            .unwrap();

        let err = install_or_update("agents", None, None, false).unwrap_err();

        assert!(
            err.to_string().contains("--install-builtins agents"),
            "got: {err}"
        );
        assert!(
            err.to_string().contains("--update-bundle agents"),
            "got: {err}"
        );
    }

    #[test]
    #[serial]
    fn install_or_update_rejects_remote_flags_for_updates() {
        let _guard = TestVaultConfigGuard::new("iou-flags");
        let src_root = fresh_temp_dir("iou-flags-src-");
        let repo = src_root.join("bundle");
        write_src(&repo, BUNDLE_MANIFEST_FILE, "name: flag-bundle\n");
        write_src(&repo, "macros/m.yaml", "a: 1\n");
        init_bundle_repo(&repo);
        install_remote(repo.to_str().unwrap(), None, false).unwrap();

        let err =
            install_or_update("flag-bundle", None, Some(InstallFilter::Macros), false).unwrap_err();
        assert!(
            err.to_string().contains("only apply to remote installs"),
            "got: {err}"
        );

        let err = install_or_update("flag-bundle", None, None, true).unwrap_err();
        assert!(
            err.to_string().contains("only apply to remote installs"),
            "got: {err}"
        );
        let _ = fs::remove_dir_all(&src_root);
    }

    #[test]
    fn shorthand_accepts_owner_repo_and_deeper_paths() {
        assert!(is_repo_shorthand("someuser/oh-my-coyote"));
        assert!(is_repo_shorthand("group/subgroup/repo"));
        assert!(is_repo_shorthand("someuser/repo#v2"));
    }

    #[test]
    fn shorthand_rejects_urls_paths_and_bare_names() {
        assert!(!is_repo_shorthand("https://github.com/x/y"));
        assert!(!is_repo_shorthand("git@github.com:x/y.git"));
        assert!(!is_repo_shorthand("./local/dir"));
        assert!(!is_repo_shorthand("/abs/path"));
        assert!(!is_repo_shorthand("~/home/path"));
        assert!(!is_repo_shorthand("-flag/like"));
        assert!(!is_repo_shorthand("bare-name"));
        assert!(!is_repo_shorthand("a//b"));
        assert!(!is_repo_shorthand("a/b/"));
        assert!(!is_repo_shorthand("a\\b"));
        assert!(!is_repo_shorthand("a b/c"));
    }

    #[test]
    fn shorthand_expands_against_default_and_custom_hosts() {
        assert_eq!(
            expand_repo_shorthand("someuser/omc", None).unwrap(),
            "https://github.com/someuser/omc"
        );
        assert_eq!(
            expand_repo_shorthand("someuser/omc#v2", Some("git.somedomain.com")).unwrap(),
            "https://git.somedomain.com/someuser/omc#v2"
        );
        assert_eq!(
            expand_repo_shorthand("a/b", Some("https://git.x.com/")).unwrap(),
            "https://git.x.com/a/b"
        );
        assert!(expand_repo_shorthand("a/b", Some("bad/host")).is_err());
        assert!(expand_repo_shorthand("a/b", Some("")).is_err());
    }

    #[test]
    fn classify_prefers_installed_names_over_shorthand() {
        assert_eq!(
            classify_install_target("someuser/omc", &[]),
            InstallTarget::Shorthand
        );
        assert_eq!(
            classify_install_target("someuser/omc", &["someuser/omc".to_string()]),
            InstallTarget::InstalledBundle
        );
    }

    #[test]
    fn install_or_update_git_host_rejects_non_shorthand_values() {
        let err = install_or_update("https://github.com/x/y", Some("git.x.com"), None, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("--git-host only applies"),
            "got: {err}"
        );

        let err = install_or_update("bare-name", Some("git.x.com"), None, false).unwrap_err();
        assert!(
            err.to_string().contains("--git-host only applies"),
            "got: {err}"
        );
    }

    #[test]
    fn repl_install_flags_parse_git_host() {
        let parsed = parse_repl_install_flags(
            ".install",
            vec!["--git-host".to_string(), "git.x.com".to_string()].into_iter(),
        )
        .unwrap();
        assert_eq!(parsed.git_host.as_deref(), Some("git.x.com"));
        assert!(parsed.filter.is_none() && !parsed.force);

        let parsed = parse_repl_install_flags(
            ".install",
            vec!["--git-host=git.y.com".to_string(), "--force".to_string()].into_iter(),
        )
        .unwrap();
        assert_eq!(parsed.git_host.as_deref(), Some("git.y.com"));
        assert!(parsed.force);
    }

    #[test]
    fn repl_install_flags_accept_any_argument_order() {
        let parsed = parse_repl_install_flags(
            ".install",
            vec![
                "--git-host".to_string(),
                "git.x.com".to_string(),
                "owner/repo".to_string(),
                "--force".to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(parsed.value.as_deref(), Some("owner/repo"));
        assert_eq!(parsed.git_host.as_deref(), Some("git.x.com"));
        assert!(parsed.filter.is_none() && parsed.force);

        let err = parse_repl_install_flags(
            ".install",
            vec!["one".to_string(), "two".to_string()].into_iter(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unexpected argument"));

        let err = parse_repl_install_flags(".install", vec!["--bogus".to_string()].into_iter())
            .unwrap_err();
        assert!(err.to_string().contains("Unexpected argument"));
    }

    #[test]
    #[serial]
    fn uninstall_shorthand_resolves_a_single_source_match() {
        let _guard = TestVaultConfigGuard::new("uninst-short-one");
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/someuser/omc"))
            .unwrap();
        drop(store);

        uninstall_bundle("someuser/omc", true).unwrap();

        let store = BundleStore::load().unwrap();
        assert!(store.get("omc").is_none());
    }

    #[test]
    #[serial]
    fn uninstall_shorthand_with_multiple_matches_bails_non_interactively() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping uninstall_shorthand_with_multiple_matches_bails_non_interactively: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("uninst-short-multi");
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/someuser/omc"))
            .unwrap();
        store
            .upsert_bundle(
                "omc-fork",
                test_metadata("https://git.somedomain.com/someuser/omc"),
            )
            .unwrap();
        drop(store);

        let err = uninstall_bundle("someuser/omc", true).unwrap_err();

        assert!(
            err.to_string()
                .contains("matches multiple installed bundles"),
            "got: {err}"
        );
        assert!(err.to_string().contains("omc-fork"), "got: {err}");
        let store = BundleStore::load().unwrap();
        assert!(store.get("omc").is_some());
        assert!(store.get("omc-fork").is_some());
    }

    #[test]
    #[serial]
    fn uninstall_exact_name_wins_over_shorthand_source_matches() {
        let _guard = TestVaultConfigGuard::new("uninst-short-name");
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle(
                "someuser/omc",
                test_metadata("https://git.somedomain.com/other/repo"),
            )
            .unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/someuser/omc"))
            .unwrap();
        drop(store);

        uninstall_bundle("someuser/omc", true).unwrap();

        let store = BundleStore::load().unwrap();
        assert!(store.get("someuser/omc").is_none());
        assert!(store.get("omc").is_some());
    }

    fn owned_file_record(path: &str, contents: &str) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            category: "macros".to_string(),
            sha256: hash_bytes(contents.as_bytes()),
            action: FileAction::New,
        }
    }

    fn mcp_server_record(name: &str, action: McpAction, sha256: Option<String>) -> McpServerRecord {
        McpServerRecord {
            name: name.to_string(),
            action,
            renamed_to: None,
            sha256,
        }
    }

    #[test]
    #[serial]
    fn uninstall_deletes_intact_files_and_removes_the_bundle_record() {
        let _guard = TestVaultConfigGuard::new("uninst-intact");
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("gone", test_metadata("https://github.com/x/gone"))
            .unwrap();
        let dst = paths::macros_dir().join("hello.yaml");
        fs::create_dir_all(paths::macros_dir()).unwrap();
        fs::write(&dst, "hi\n").unwrap();
        store
            .record_file("gone", owned_file_record("macros/hello.yaml", "hi\n"))
            .unwrap();

        uninstall_bundle("gone", true).unwrap();

        assert!(!dst.exists());
        assert!(
            !paths::macros_dir().exists(),
            "emptied macros dir should be pruned"
        );
        assert!(BundleStore::load().unwrap().get("gone").is_none());
    }

    #[test]
    #[serial]
    fn uninstall_keeps_modified_files_and_retains_the_record() {
        let _guard = TestVaultConfigGuard::new("uninst-modified");
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("mods", test_metadata("https://github.com/x/mods"))
            .unwrap();
        let dst = paths::macros_dir().join("edited.yaml");
        fs::create_dir_all(paths::macros_dir()).unwrap();
        fs::write(&dst, "user edit\n").unwrap();
        store
            .record_file(
                "mods",
                owned_file_record("macros/edited.yaml", "original\n"),
            )
            .unwrap();

        uninstall_bundle("mods", true).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "user edit\n");
        let store = BundleStore::load().unwrap();
        let record = store.get("mods").unwrap();
        assert_eq!(record.files.len(), 1);
        assert_eq!(record.files[0].path, "macros/edited.yaml");
    }

    #[test]
    fn apply_uninstall_delete_removes_file_record_and_empty_dirs() {
        let dir = fresh_temp_dir("uninst-apply-delete-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let dst = dir.join("macros/gone.yaml");
        write_src(&dir, "macros/gone.yaml", "user edit");
        store
            .record_file("omc", owned_file_record("macros/gone.yaml", "original"))
            .unwrap();
        let mut summary = UninstallFileSummary::default();

        apply_uninstall_file_action(
            &mut store,
            "omc",
            "macros/gone.yaml",
            &dst,
            &dir,
            ObsoleteAction::Delete,
            &mut summary,
        )
        .unwrap();

        assert!(!dst.exists());
        assert!(!dir.join("macros").exists());
        assert_eq!(summary.deleted, 1);
        assert!(store.get("omc").unwrap().files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_files_drop_records_for_missing_files() {
        let dir = fresh_temp_dir("uninst-missing-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        store
            .record_file("omc", owned_file_record("macros/ghost.yaml", "gone"))
            .unwrap();
        let files = store.get("omc").unwrap().files.clone();

        let summary = uninstall_owned_files(&mut store, "omc", &files, &dir, true).unwrap();

        assert_eq!(
            summary,
            UninstallFileSummary {
                missing: 1,
                ..UninstallFileSummary::default()
            }
        );
        assert!(store.get("omc").unwrap().files.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_files_never_touch_unowned_files() {
        let dir = fresh_temp_dir("uninst-unowned-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        write_src(&dir, "macros/owned.yaml", "a");
        write_src(&dir, "macros/user.yaml", "mine");
        store
            .record_file("omc", owned_file_record("macros/owned.yaml", "a"))
            .unwrap();
        let files = store.get("omc").unwrap().files.clone();

        let summary = uninstall_owned_files(&mut store, "omc", &files, &dir, true).unwrap();

        assert_eq!(summary.deleted, 1);
        assert!(!dir.join("macros/owned.yaml").exists());
        assert_eq!(
            fs::read_to_string(dir.join("macros/user.yaml")).unwrap(),
            "mine"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn uninstall_files_keep_records_when_deletion_fails_and_continue() {
        use std::os::unix::fs::PermissionsExt;

        struct RestorePerms(PathBuf);
        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }

        let dir = fresh_temp_dir("uninst-fail-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        write_src(&dir, "locked/stuck.yaml", "a");
        write_src(&dir, "macros/ok.yaml", "b");
        store
            .record_file("omc", owned_file_record("locked/stuck.yaml", "a"))
            .unwrap();
        store
            .record_file("omc", owned_file_record("macros/ok.yaml", "b"))
            .unwrap();
        let locked = dir.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let restore = RestorePerms(locked.clone());
        let probe = locked.join(".write-probe");
        if fs::write(&probe, "x").is_ok() {
            // A privileged user bypasses permission bits; the failure path
            // cannot be provoked this way.
            let _ = fs::remove_file(&probe);
            drop(restore);
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let files = store.get("omc").unwrap().files.clone();

        let summary = uninstall_owned_files(&mut store, "omc", &files, &dir, true).unwrap();
        drop(restore);

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.deleted, 1);
        assert!(dir.join("locked/stuck.yaml").exists());
        assert!(!dir.join("macros/ok.yaml").exists());
        let paths: Vec<&str> = store
            .get("omc")
            .unwrap()
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        assert_eq!(paths, vec!["locked/stuck.yaml"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_files_skip_suspicious_recorded_paths() {
        let dir = fresh_temp_dir("uninst-suspicious-");
        let config_dir = dir.join("config");
        fs::create_dir_all(&config_dir).unwrap();
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        write_src(&dir, "evil.yaml", "outside");
        let abs_victim = dir.join("abs.yaml");
        fs::write(&abs_victim, "outside").unwrap();
        store
            .record_file("omc", owned_file_record("../evil.yaml", "outside"))
            .unwrap();
        store
            .record_file(
                "omc",
                owned_file_record(abs_victim.to_str().unwrap(), "outside"),
            )
            .unwrap();
        let files = store.get("omc").unwrap().files.clone();

        let summary = uninstall_owned_files(&mut store, "omc", &files, &config_dir, true).unwrap();

        assert_eq!(summary.failed, 2);
        assert_eq!(summary.deleted, 0);
        assert!(dir.join("evil.yaml").exists());
        assert!(abs_victim.exists());
        assert_eq!(store.get("omc").unwrap().files.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_mcp_keeps_replaced_entries_and_drops_the_record() {
        let dir = fresh_temp_dir("uninst-mcp-replaced-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        store
            .record_mcp_servers(
                "omc",
                vec![mcp_server_record("srv", McpAction::Replaced, None)],
            )
            .unwrap();
        let mcp = dir.join("mcp.json");
        write_mcp(
            &mcp,
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "echo"}}}"#,
        );
        let servers = store.get("omc").unwrap().mcp_servers.clone();

        let summary = uninstall_mcp_entries(&mut store, "omc", &servers, &mcp, true).unwrap();

        assert_eq!(summary.kept, vec!["srv"]);
        assert!(summary.removed.is_empty());
        let written: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        assert!(written.mcp_servers.contains_key("srv"));
        assert!(store.get("omc").unwrap().mcp_servers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_mcp_removes_intact_entries_and_leaves_unowned_keys() {
        let dir = fresh_temp_dir("uninst-mcp-intact-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let mcp = dir.join("mcp.json");
        write_mcp(
            &mcp,
            r#"{"mcpServers": {
                "srv": {"type": "stdio", "command": "echo"},
                "user-srv": {"type": "stdio", "command": "mine"}
            }}"#,
        );
        let parsed: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        let hash = hash_bytes(
            serde_json::to_string(parsed.mcp_servers.get("srv").unwrap())
                .unwrap()
                .as_bytes(),
        );
        store
            .record_mcp_servers(
                "omc",
                vec![mcp_server_record("srv", McpAction::Added, Some(hash))],
            )
            .unwrap();
        let servers = store.get("omc").unwrap().mcp_servers.clone();

        let summary = uninstall_mcp_entries(&mut store, "omc", &servers, &mcp, true).unwrap();

        assert_eq!(summary.removed, vec!["srv"]);
        assert!(summary.kept.is_empty());
        let written: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        assert!(!written.mcp_servers.contains_key("srv"));
        assert!(written.mcp_servers.contains_key("user-srv"));
        assert!(store.get("omc").unwrap().mcp_servers.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_mcp_reports_referenced_secrets_without_removing_them() {
        let dir = fresh_temp_dir("uninst-mcp-secrets-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let mcp = dir.join("mcp.json");
        write_mcp(
            &mcp,
            r#"{"mcpServers": {
                "srv": {
                    "type": "stdio",
                    "command": "echo",
                    "env": {"TOKEN": "{{OMC_TOKEN}}", "ORG": "{{OMC_ORG}}"}
                }
            }}"#,
        );
        let parsed: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        let hash = hash_bytes(
            serde_json::to_string(parsed.mcp_servers.get("srv").unwrap())
                .unwrap()
                .as_bytes(),
        );
        store
            .record_mcp_servers(
                "omc",
                vec![mcp_server_record("srv", McpAction::Added, Some(hash))],
            )
            .unwrap();
        let servers = store.get("omc").unwrap().mcp_servers.clone();

        let summary = uninstall_mcp_entries(&mut store, "omc", &servers, &mcp, true).unwrap();

        assert_eq!(summary.removed, vec!["srv"]);
        assert_eq!(summary.secrets, vec!["OMC_ORG", "OMC_TOKEN"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_mcp_keeps_modified_entries_and_their_records() {
        let dir = fresh_temp_dir("uninst-mcp-modified-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let mcp = dir.join("mcp.json");
        write_mcp(
            &mcp,
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "edited"}}}"#,
        );
        store
            .record_mcp_servers(
                "omc",
                vec![mcp_server_record(
                    "srv",
                    McpAction::Added,
                    Some("0".repeat(64)),
                )],
            )
            .unwrap();
        let servers = store.get("omc").unwrap().mcp_servers.clone();

        let summary = uninstall_mcp_entries(&mut store, "omc", &servers, &mcp, true).unwrap();

        assert_eq!(summary.kept, vec!["srv"]);
        assert!(summary.removed.is_empty());
        let written: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        assert!(written.mcp_servers.contains_key("srv"));
        assert_eq!(store.get("omc").unwrap().mcp_servers.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn uninstall_mcp_keeps_legacy_records_without_a_hash() {
        let dir = fresh_temp_dir("uninst-mcp-legacy-");
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let mcp = dir.join("mcp.json");
        write_mcp(
            &mcp,
            r#"{"mcpServers": {"srv": {"type": "stdio", "command": "echo"}}}"#,
        );
        store
            .record_mcp_servers(
                "omc",
                vec![mcp_server_record("srv", McpAction::Added, None)],
            )
            .unwrap();
        let servers = store.get("omc").unwrap().mcp_servers.clone();

        let summary = uninstall_mcp_entries(&mut store, "omc", &servers, &mcp, true).unwrap();

        assert_eq!(summary.kept, vec!["srv"]);
        let written: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        assert!(written.mcp_servers.contains_key("srv"));
        assert_eq!(store.get("omc").unwrap().mcp_servers.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn uninstall_unknown_name_lists_installed_and_url_spelling_resolves() {
        let _guard = TestVaultConfigGuard::new("uninst-unknown");

        let err = uninstall_bundle("nope", true).unwrap_err();
        assert!(err.to_string().contains("none are installed"), "got: {err}");

        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();
        let err = uninstall_bundle("nope", true).unwrap_err();
        assert!(
            err.to_string().contains("installed bundles: omc"),
            "got: {err}"
        );

        uninstall_bundle("git@github.com:x/omc.git", true).unwrap();

        assert!(BundleStore::load().unwrap().get("omc").is_none());
    }

    #[test]
    #[serial]
    fn uninstall_refuses_non_interactive_without_yes() {
        if *IS_STDOUT_TERMINAL {
            eprintln!(
                "Skipping uninstall_refuses_non_interactive_without_yes: requires non-TTY stdout"
            );
            return;
        }
        let _guard = TestVaultConfigGuard::new("uninst-non-tty");
        let mut store = BundleStore::load().unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();

        let err = uninstall_bundle("omc", false).unwrap_err();

        assert!(err.to_string().contains("--yes"), "got: {err}");
        assert!(BundleStore::load().unwrap().get("omc").is_some());
    }

    #[test]
    #[serial]
    fn record_mcp_merge_stores_entry_hashes() {
        let _guard = TestVaultConfigGuard::new("uninst-merge-hash");
        let dir = fresh_temp_dir("uninst-merge-hash-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(&remote, FIXTURE_REMOTE);
        let mut store = BundleStore::load_from(dir.join("installed-bundles.yaml")).unwrap();
        store
            .upsert_bundle("omc", test_metadata("https://github.com/x/omc"))
            .unwrap();

        let report = merge_mcp_json(None, &remote, &target, false, &HashSet::new(), false).unwrap();
        record_mcp_merge(&mut store, "omc", &report).unwrap();

        let written: McpServersConfig =
            serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
        let expected = hash_bytes(
            serde_json::to_string(written.mcp_servers.get("alpha").unwrap())
                .unwrap()
                .as_bytes(),
        );
        let record = store.get("omc").unwrap();
        let alpha = record
            .mcp_servers
            .iter()
            .find(|s| s.name == "alpha")
            .unwrap();
        assert_eq!(alpha.sha256.as_deref(), Some(expected.as_str()));
        let _ = fs::remove_dir_all(&dir);
    }
}
