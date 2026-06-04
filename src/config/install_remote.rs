use crate::config::{InstallFilter, paths};
#[cfg(not(windows))]
use crate::function::Language;
use crate::mcp::{McpServer, McpServersConfig};
use crate::utils;
use crate::utils::IS_STDOUT_TERMINAL;
use crate::vault::{Vault, create_vault_password_file, interpolate_secrets};
use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use indoc::formatdoc;
use inquire::{Confirm, Select};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

pub fn install_remote(git_url: &str, filter: Option<InstallFilter>, force: bool) -> Result<()> {
    let (url, reference) = parse_url_with_ref(git_url)?;
    let temp = clone_to_temp(&url, reference.as_deref())?;
    println!("Cloned {git_url} to {}", temp.path().display());

    let layout = scan_remote_layout(temp.path())?;
    let layout = apply_filter(layout, filter);

    if layout.is_empty() {
        println!(
            "No recognized assets found in {git_url}. Expected one or more of: \
             agents/, roles/, skills/, macros/, functions/tools/, functions/mcp.json"
        );
        return Ok(());
    }

    let plan = plan_changes(&layout)?;

    if !plan.files.is_empty() {
        print_plan_summary(&plan);
        apply_plan(&plan, force)?;
    }

    if let Some((remote_mcp, local_mcp)) = &plan.mcp_json {
        let local = local_mcp.exists().then_some(local_mcp.as_path());
        let report = merge_mcp_json(local, remote_mcp, local_mcp, force)?;
        print_mcp_merge_report(&report);
        handle_missing_secrets(&report.missing_secrets)?;
    }

    Ok(())
}

pub fn install_remote_from_repl_args(args: &str) -> Result<()> {
    let tokens = shell_words::split(args)
        .with_context(|| format!("failed to parse '.install remote' args: {args}"))?;

    let mut iter = tokens.into_iter();
    let url = iter.next().with_context(|| {
        format!(
            "Usage: .install remote <git-url> [--filter <{}>] [--force]",
            InstallFilter::NAMES.join("|")
        )
    })?;

    let mut filter: Option<InstallFilter> = None;
    let mut force = false;

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
            other => bail!("Unexpected argument to '.install remote': {other}"),
        }
    }

    install_remote(&url, filter, force)
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
}

impl TempRepoDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempRepoDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn clone_to_temp(url: &str, reference: Option<&str>) -> Result<TempRepoDir> {
    let dest = utils::temp_file("coyote-remote-install-", "");
    let dest_arg: OsString = dest.as_os_str().into();

    let is_sha = reference
        .map(|r| r.len() >= 4 && r.len() <= 40 && r.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or(false);

    match reference {
        Some(r) if !is_sha => {
            run_git(vec![
                "clone".into(),
                "--depth".into(),
                "1".into(),
                "--branch".into(),
                r.into(),
                url.into(),
                dest_arg,
            ])?;
        }
        Some(r) => {
            run_git(vec!["clone".into(), url.into(), dest_arg.clone()])?;
            run_git(vec!["-C".into(), dest_arg, "checkout".into(), r.into()])?;
        }
        None => {
            run_git(vec![
                "clone".into(),
                "--depth".into(),
                "1".into(),
                url.into(),
                dest_arg,
            ])?;
        }
    }

    Ok(TempRepoDir { path: dest })
}

fn run_git(args: Vec<OsString>) -> Result<()> {
    let output = duct::cmd("git", &args)
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

#[derive(Default)]
struct RemoteLayout {
    agents: Option<PathBuf>,
    roles: Option<PathBuf>,
    skills: Option<PathBuf>,
    macros: Option<PathBuf>,
    functions_tools: Option<PathBuf>,
    mcp_json: Option<PathBuf>,
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
    let mut layout = RemoteLayout::default();

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

    let functions = root.join("functions");
    if functions.is_dir() {
        let tools = functions.join("tools");
        if tools.is_dir() {
            layout.functions_tools = Some(tools);
        }

        let mcp = functions.join("mcp.json");
        if mcp.is_file() {
            layout.mcp_json = Some(mcp);
        }
    }

    Ok(layout)
}

fn apply_filter(mut layout: RemoteLayout, filter: Option<InstallFilter>) -> RemoteLayout {
    let Some(filter) = filter else {
        return layout;
    };
    match filter {
        InstallFilter::Agents => RemoteLayout {
            agents: layout.agents.take(),
            ..RemoteLayout::default()
        },
        InstallFilter::Roles => RemoteLayout {
            roles: layout.roles.take(),
            ..RemoteLayout::default()
        },
        InstallFilter::Skills => RemoteLayout {
            skills: layout.skills.take(),
            ..RemoteLayout::default()
        },
        InstallFilter::Macros => RemoteLayout {
            macros: layout.macros.take(),
            ..RemoteLayout::default()
        },
        InstallFilter::Functions => RemoteLayout {
            functions_tools: layout.functions_tools.take(),
            ..RemoteLayout::default()
        },
        InstallFilter::McpConfig => RemoteLayout {
            mcp_json: layout.mcp_json.take(),
            ..RemoteLayout::default()
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
    use std::io::Read;
    let mut fa = fs::File::open(a).with_context(|| format!("open {}", a.display()))?;
    let mut fb = fs::File::open(b).with_context(|| format!("open {}", b.display()))?;
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let na = fa.read(&mut buf_a)?;
        let nb = fb.read(&mut buf_b)?;
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
        if new_ + identical + conflict > 0 {
            println!(
                "  {:<16} new={new_}  identical={identical}  conflict={conflict}",
                cat.label()
            );
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

struct ApplyReport {
    new_count: usize,
    identical_count: usize,
    replaced_count: usize,
    kept_count: usize,
}

fn apply_plan(plan: &InstallPlan, force: bool) -> Result<ApplyReport> {
    let mut report = ApplyReport {
        new_count: 0,
        identical_count: 0,
        replaced_count: 0,
        kept_count: 0,
    };
    let mut sticky = if force {
        StickyMode::ReplaceAll
    } else {
        StickyMode::None
    };

    for planned in &plan.files {
        match planned.kind {
            PlannedKind::New => {
                write_file(&planned.src, &planned.dst)?;
                report.new_count += 1;
            }
            PlannedKind::Identical => {
                report.identical_count += 1;
            }
            PlannedKind::Conflict => match resolve_conflict(planned, &mut sticky)? {
                ConflictAction::Keep => report.kept_count += 1,
                ConflictAction::Replace => {
                    write_file(&planned.src, &planned.dst)?;
                    report.replaced_count += 1;
                }
            },
        }
    }

    println!(
        "\nInstalled: {} new, {} replaced, {} kept, {} identical.",
        report.new_count, report.replaced_count, report.kept_count, report.identical_count
    );

    Ok(report)
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
             Re-run with --install-force or in a terminal.",
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
        final_path: final_path.clone(),
        missing_secrets: Vec::new(),
    };
    let mut to_validate: Vec<String> = Vec::new();

    for (name, remote_server) in remote_config.mcp_servers {
        if let Some(local_server) = merged.mcp_servers.get(&name) {
            if local_server == &remote_server {
                continue;
            }
            match resolve_mcp_conflict(&name, force)? {
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

fn resolve_mcp_conflict(name: &str, force: bool) -> Result<McpConflictAction> {
    if force {
        return Ok(McpConflictAction::TakeRemote);
    }
    if !*IS_STDOUT_TERMINAL {
        bail!(
            "MCP server '{name}' already exists locally. Refusing to merge non-interactively. \
             Re-run with --install-force or in a terminal."
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
    use crate::utils::get_env_name;
    use serial_test::serial;
    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

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

        let report = merge_mcp_json(None, &remote, &target, false).unwrap();

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

        let report = merge_mcp_json(Some(&target), &remote, &target, true).unwrap();

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

        let err = merge_mcp_json(Some(&target), &remote, &target, false).unwrap_err();

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

        let err = merge_mcp_json(None, &remote, &target, false).unwrap_err();

        assert!(
            format!("{err:#}").contains("missing a \"command\" field"),
            "got: {err:#}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[serial]
    async fn merge_detects_missing_secrets_in_output() {
        let _guard = TestVaultConfigGuard::new("merge-secret");
        let dir = fresh_temp_dir("merge-secret-");
        let remote = dir.join("remote.json");
        let target = dir.join("target.json");
        write_mcp(
            &remote,
            r#"{"mcpServers": {"x": {"type":"stdio","command":"echo","env":{"K":"{{COYOTE_TEST_MERGE_SECRET}}"}}}}"#,
        );

        let report = merge_mcp_json(None, &remote, &target, false).unwrap();

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

        merge_mcp_json(None, &remote, &target, false).unwrap();
        let after_first = fs::read(&target).unwrap();

        let report = merge_mcp_json(Some(&target), &remote, &target, false).unwrap();
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
}
