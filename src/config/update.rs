use std::fs::OpenOptions;
use anyhow::{Context, Result, bail};
use inquire::Confirm;
use is_terminal::IsTerminal;
use std::path::Path;
use std::{env, fs, io, process};
use dunce::canonicalize;
use self_update::backends::github::Update;
use self_update::Status;
use crate::utils::warning_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallSource {
    Cargo,
    Homebrew,
    Manual,
}

impl InstallSource {
    fn is_package_managed(self) -> bool {
        matches!(self, InstallSource::Cargo | InstallSource::Homebrew)
    }

    fn label(self) -> &'static str {
        match self {
            InstallSource::Cargo => "Cargo",
            InstallSource::Homebrew => "Homebrew",
            InstallSource::Manual => "manually-installed",
        }
    }
}

fn classify_install_path(path: &Path) -> InstallSource {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    if components
        .windows(2)
        .any(|w| w[0] == ".cargo" && w[1] == "bin")
    {
        return InstallSource::Cargo;
    }

    if components.contains(&"Cellar") {
        return InstallSource::Homebrew;
    }
    let path_str = path.to_string_lossy();
    if path_str.starts_with("/opt/homebrew/") || path_str.starts_with("/home/linuxbrew/.linuxbrew/")
    {
        return InstallSource::Homebrew;
    }

    InstallSource::Manual
}

fn normalize_version(requested: Option<String>) -> Option<String> {
    let raw = requested?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("latest") {
        return None;
    }
    match trimmed.chars().next() {
        Some('v' | 'V') => Some(trimmed.to_string()),
        Some(c) if c.is_ascii_digit() => Some(format!("v{trimmed}")),
        _ => Some(trimmed.to_string()),
    }
}

fn is_dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".loki-update-write-test-{}", process::id()));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn run_self_update(requested: Option<String>, force: bool) -> Result<()> {
    let target_tag = normalize_version(requested);

    let exe_path = env::current_exe()
        .context("Could not determine the path of the running loki executable")?;
    let resolved = canonicalize(&exe_path).unwrap_or_else(|_| exe_path.clone());
    let source = classify_install_path(&resolved);

    if source.is_package_managed() {
        let body = match source {
            InstallSource::Homebrew => format!(
                "Loki appears to be installed via Homebrew ({}).\n\
                 Updating in place replaces the binary inside Homebrew's Cellar; `brew` will\n\
                 then report a version that no longer matches the file on disk, and a later\n\
                 `brew upgrade`/`brew reinstall` may overwrite it or fail.\n\
                 The clean way to update is:  brew upgrade loki",
                exe_path.display()
            ),
            InstallSource::Cargo => format!(
                "Loki appears to be installed via `cargo install` ({}).\n\
                 Updating in place leaves Cargo's records out of sync with the binary on disk.\n\
                 The clean way to update is:  cargo install --locked loki-ai",
                exe_path.display()
            ),
            InstallSource::Manual => unreachable!("Manual installs are not package-managed"),
        };
        println!("{} {body}", warning_text("WARNING:"));

        if force {
            println!("--force specified; updating anyway.");
        } else if io::stdin().is_terminal() {
            let proceed = Confirm::new("Update anyway?")
                .with_default(false)
                .prompt()?;
            if !proceed {
                println!("Update cancelled.");
                return Ok(());
            }
        } else {
            bail!(
                "Refusing to update a {} install. Re-run with --force to override.",
                source.label()
            );
        }
    }

    if let Some(parent) = exe_path.parent()
        && !is_dir_writable(parent)
    {
        bail!(
            "No write permission for '{}'. Re-run with elevated permissions (e.g. sudo), \
             or update Loki through your package manager.",
            parent.display()
        );
    }

    let interactive = io::stdin().is_terminal();
    let mut builder = Update::configure();
    builder
        .repo_owner("Dark-Alex-17")
        .repo_name("loki")
        .bin_name("loki")
        .current_version(env!("CARGO_PKG_VERSION"))
        .no_confirm(true)
        .show_download_progress(interactive);
    if let Some(tag) = &target_tag {
        builder.target_version_tag(tag.as_str());
    }
    let status = builder
        .build()
        .context("Failed to configure the self-update")?
        .update()
        .context("Self-update failed")?;

    match status {
        Status::UpToDate(version) => {
            println!("Loki is already up to date (v{version}).");
        }
        Status::Updated(version) => {
            println!("Loki updated to v{version}. Restart loki to use the new version.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_cargo_install() {
        assert_eq!(
            classify_install_path(&PathBuf::from("/home/u/.cargo/bin/loki")),
            InstallSource::Cargo
        );
    }

    #[test]
    fn classify_homebrew_opt_prefix() {
        assert_eq!(
            classify_install_path(&PathBuf::from("/opt/homebrew/bin/loki")),
            InstallSource::Homebrew
        );
    }

    #[test]
    fn classify_homebrew_cellar() {
        assert_eq!(
            classify_install_path(&PathBuf::from("/usr/local/Cellar/loki/0.3.0/bin/loki")),
            InstallSource::Homebrew
        );
    }

    #[test]
    fn classify_homebrew_linuxbrew() {
        assert_eq!(
            classify_install_path(&PathBuf::from("/home/linuxbrew/.linuxbrew/bin/loki")),
            InstallSource::Homebrew
        );
    }

    #[test]
    fn classify_manual_usr_local_bin() {
        assert_eq!(
            classify_install_path(&PathBuf::from("/usr/local/bin/loki")),
            InstallSource::Manual
        );
    }

    #[test]
    fn classify_manual_local_bin() {
        assert_eq!(
            classify_install_path(&PathBuf::from("/home/u/.local/bin/loki")),
            InstallSource::Manual
        );
    }

    #[test]
    fn normalize_version_latest_and_empty_are_none() {
        assert_eq!(normalize_version(None), None);
        assert_eq!(normalize_version(Some(String::new())), None);
        assert_eq!(normalize_version(Some("   ".to_string())), None);
        assert_eq!(normalize_version(Some("latest".to_string())), None);
        assert_eq!(normalize_version(Some("LATEST".to_string())), None);
    }

    #[test]
    fn normalize_version_prepends_v_for_bare_semver() {
        assert_eq!(
            normalize_version(Some("0.4.0".to_string())),
            Some("v0.4.0".to_string())
        );
        assert_eq!(
            normalize_version(Some("v0.4.0".to_string())),
            Some("v0.4.0".to_string())
        );
        assert_eq!(
            normalize_version(Some("  v0.4.0  ".to_string())),
            Some("v0.4.0".to_string())
        );
    }
}
