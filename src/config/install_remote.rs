use anyhow::{Context, Result, bail};
use inquire::Select;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{InstallFilter, paths};
use crate::function::Language;
use crate::utils;
use crate::utils::IS_STDOUT_TERMINAL;

pub fn install_remote(git_url: &str, filter: Option<InstallFilter>, force: bool) -> Result<()> {
    let (url, reference) = parse_url_with_ref(git_url)?;
    let temp = clone_to_temp(&url, reference.as_deref())?;
    println!("Cloned {git_url} to {}", temp.path().display());

    let layout = scan_remote_layout(temp.path())?;
    let layout = apply_filter(layout, filter);

    if layout.is_empty() {
        println!(
            "No recognized assets found in {git_url}. Expected one or more of: \
             agents/, roles/, macros/, functions/tools/, functions/mcp.json"
        );
        return Ok(());
    }

    let plan = plan_changes(&layout)?;

    if !plan.files.is_empty() {
        print_plan_summary(&plan);
        apply_plan(&plan, force)?;
    }

    if plan.skipped_mcp_json.is_some() {
        println!(
            "\nNote: functions/mcp.json detected but MCP merge is not yet wired up \
             (Step 3 of the install-remote rollout)."
        );
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
    let dest = utils::temp_file("loki-remote-install-", "");
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
    macros: Option<PathBuf>,
    functions_tools: Option<PathBuf>,
    mcp_json: Option<PathBuf>,
}

impl RemoteLayout {
    fn is_empty(&self) -> bool {
        self.agents.is_none()
            && self.roles.is_none()
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
    Macros,
    FunctionsTools,
}

impl TopCategory {
    fn label(&self) -> &'static str {
        match self {
            TopCategory::Agents => "agents",
            TopCategory::Roles => "roles",
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
    skipped_mcp_json: Option<(PathBuf, PathBuf)>,
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

    let skipped_mcp_json = layout
        .mcp_json
        .as_ref()
        .map(|src| (src.clone(), paths::mcp_config_file()));

    Ok(InstallPlan {
        files,
        skipped_mcp_json,
    })
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
        "abort" => bail!("Aborted by user."),
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

#[cfg(test)]
mod tests {
    use super::*;

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
            macros: Some(PathBuf::from("m")),
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
        };
        let out = apply_filter(l, None);
        assert!(out.agents.is_some() && out.roles.is_some() && out.macros.is_some());
        assert!(out.functions_tools.is_some() && out.mcp_json.is_some());
    }

    #[test]
    fn apply_filter_functions_keeps_only_tools_not_mcp() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: None,
            macros: None,
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
        };
        let out = apply_filter(l, Some(InstallFilter::Functions));
        assert!(out.agents.is_none());
        assert_eq!(out.functions_tools, Some(PathBuf::from("f")));
        assert!(out.mcp_json.is_none());
    }

    #[test]
    fn apply_filter_mcp_config_keeps_only_mcp_json() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: None,
            macros: None,
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
        };
        let out = apply_filter(l, Some(InstallFilter::McpConfig));
        assert!(out.agents.is_none() && out.functions_tools.is_none());
        assert_eq!(out.mcp_json, Some(PathBuf::from("j")));
    }

    #[test]
    fn apply_filter_roles_keeps_only_roles() {
        let l = RemoteLayout {
            agents: Some(PathBuf::from("a")),
            roles: Some(PathBuf::from("r")),
            macros: Some(PathBuf::from("m")),
            functions_tools: Some(PathBuf::from("f")),
            mcp_json: Some(PathBuf::from("j")),
        };
        let out = apply_filter(l, Some(InstallFilter::Roles));
        assert_eq!(out.roles, Some(PathBuf::from("r")));
        assert!(out.agents.is_none() && out.macros.is_none());
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
        fs::create_dir_all(root.join("macros")).unwrap();
        fs::create_dir_all(root.join("functions/tools")).unwrap();
        touch(&root.join("functions/mcp.json"));
        touch(&root.join("README.md"));

        let layout = scan_remote_layout(&root).unwrap();
        assert!(layout.agents.is_some());
        assert!(layout.roles.is_some());
        assert!(layout.macros.is_some());
        assert!(layout.functions_tools.is_some());
        assert!(layout.mcp_json.is_some());
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
}
