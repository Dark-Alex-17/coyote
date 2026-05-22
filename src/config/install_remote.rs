use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::InstallFilter;
use crate::utils;

pub fn install_remote(git_url: &str, filter: Option<InstallFilter>, force: bool) -> Result<()> {
    let (url, reference) = parse_url_with_ref(git_url)?;
    let temp = clone_to_temp(&url, reference.as_deref())?;
    println!("Cloned {git_url} to {}", temp.path().display());
    print_repo_tree(temp.path())?;
    let _ = (force, filter);
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

fn print_repo_tree(root: &Path) -> Result<()> {
    println!("Repository contents ({}):", root.display());
    print_children(root, "")
}

fn print_children(dir: &Path, prefix: &str) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != OsStr::new(".git"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let n = entries.len();
    for (i, entry) in entries.iter().enumerate() {
        let is_last = i == n - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let name = entry.file_name().to_string_lossy().to_string();
        println!("{prefix}{connector}{name}");

        if entry.file_type()?.is_dir() {
            let extension = if is_last { "    " } else { "│   " };
            let new_prefix = format!("{prefix}{extension}");
            print_children(&entry.path(), &new_prefix)?;
        }
    }
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
}
