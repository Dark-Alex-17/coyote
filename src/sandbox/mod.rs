use anyhow::{Context, Result, anyhow, bail};
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use which::which;

use crate::config::paths;
use crate::utils::run_command_with_output;
use crate::vault::Vault;

const SBX_BINARY: &str = "sbx";
pub(crate) const SANDBOX_ENV_FLAG: &str = "IS_SANDBOX";
const SANDBOX_AGENT: &str = "coyote";

#[derive(RustEmbed)]
#[folder = "assets/sbx-kit/"]
struct EmbeddedKit;

pub fn launch(name: Option<String>, fresh: bool) -> Result<()> {
    ensure_sbx_installed()?;
    bail_if_nested()?;

    let name = resolve_name(name)?;
    let kit_path = resolve_kit_path()?;

    if sandbox_exists(&name)? {
        info!("Re-attaching to existing sandbox '{name}'");
        if fresh {
            debug!("--fresh ignored: re-attaching to existing sandbox '{name}'");
        }
    } else if fresh {
        let msg = format!("Creating fresh sandbox '{name}' (no host config will be copied)");
        info!("{msg}");
        println!("{msg}");
        create_sandbox(&name, &kit_path)?;
    } else {
        create_sandbox(&name, &kit_path)?;
        copy_host_files(&name)?;
    }

    exec_run(&name, &kit_path)
}

fn ensure_sbx_installed() -> Result<()> {
    which(SBX_BINARY).map_err(|_| {
        anyhow!(
            "`sbx` binary not found in PATH.\n\n\
             Install Docker Sandboxes:\n  https://docs.docker.com/ai/sandboxes/get-started/"
        )
    })?;

    Ok(())
}

fn bail_if_nested() -> Result<()> {
    if env::var_os(SANDBOX_ENV_FLAG).is_some() {
        bail!("Refusing to nest sandboxes: ${SANDBOX_ENV_FLAG} is set, already inside one");
    }

    Ok(())
}

fn resolve_name(name: Option<String>) -> Result<String> {
    if let Some(n) = name {
        let trimmed = n.trim();
        if !trimmed.is_empty() {
            let sanitized = sanitize_name(trimmed);
            if sanitized.is_empty() {
                bail!("Sandbox name '{trimmed}' sanitizes to an empty string");
            }

            return Ok(sanitized);
        }
    }

    let cwd = env::current_dir().context("Failed to determine current directory")?;
    let basename = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("Could not derive sandbox name from current directory"))?;
    let sanitized = sanitize_name(basename);
    if sanitized.is_empty() {
        bail!("Could not derive a valid sandbox name from '{basename}'; pass --sandbox <NAME>");
    }

    Ok(sanitized)
}

fn sanitize_name(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_dash = false;
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }

    out.trim_matches('-').to_string()
}

fn resolve_kit_path() -> Result<PathBuf> {
    if let Some(path) = paths::sandbox_kit_override() {
        if !path.exists() {
            bail!(
                "$COYOTE_SANDBOX_KIT is set but path does not exist: {}",
                path.display()
            );
        }

        debug!(
            "Using kit override from $COYOTE_SANDBOX_KIT: {}",
            path.display()
        );

        return Ok(path);
    }

    extract_embedded_kit()
}

fn extract_embedded_kit() -> Result<PathBuf> {
    let cache_root = paths::sbx_kit_dir();
    let new_hash = compute_kit_hash()?;
    let hash_file = paths::sbx_kit_hash_file();
    if let Ok(existing) = fs::read_to_string(&hash_file)
        && existing == new_hash
    {
        return Ok(cache_root);
    }

    if cache_root.exists() {
        fs::remove_dir_all(&cache_root)
            .with_context(|| format!("Failed to clear stale kit at {}", cache_root.display()))?;
    }
    fs::create_dir_all(&cache_root)
        .with_context(|| format!("Failed to create {}", cache_root.display()))?;

    for entry in EmbeddedKit::iter() {
        let file = EmbeddedKit::get(&entry)
            .ok_or_else(|| anyhow!("Embedded kit file missing during extraction: {entry}"))?;
        let dest = cache_root.join(entry.as_ref());
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        fs::write(&dest, &file.data)
            .with_context(|| format!("Failed to write {}", dest.display()))?;
    }

    fs::write(&hash_file, &new_hash)
        .with_context(|| format!("Failed to write {}", hash_file.display()))?;
    debug!("Extracted embedded sbx-kit to {}", cache_root.display());

    Ok(cache_root)
}

fn compute_kit_hash() -> Result<String> {
    let mut hasher = Sha256::new();
    let mut entries: Vec<_> = EmbeddedKit::iter().collect();
    entries.sort();

    for entry in &entries {
        let file = EmbeddedKit::get(entry)
            .ok_or_else(|| anyhow!("Embedded kit file missing during hash: {entry}"))?;
        hasher.update(entry.as_bytes());
        hasher.update(b"\0");
        hasher.update(&file.data);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn sandbox_exists(name: &str) -> Result<bool> {
    let (success, stdout, stderr) =
        run_command_with_output(SBX_BINARY, &["ls"], None).context("Failed to run `sbx ls`")?;
    if !success {
        bail!("`sbx ls` failed: {stderr}");
    }

    Ok(stdout
        .lines()
        .skip(1)
        .any(|line| line.split_whitespace().next() == Some(name)))
}

fn create_sandbox(name: &str, kit_path: &Path) -> Result<()> {
    info!("Creating sandbox '{name}'");
    let kit_str = kit_path
        .to_str()
        .ok_or_else(|| anyhow!("Kit path is not valid UTF-8: {}", kit_path.display()))?;
    let status = Command::new(SBX_BINARY)
        .args([
            "create",
            "--kit",
            kit_str,
            SANDBOX_AGENT,
            "--name",
            name,
            ".",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to spawn `sbx create`")?;

    if !status.success() {
        bail!("`sbx create` exited with {status}");
    }

    Ok(())
}

fn copy_host_files(name: &str) -> Result<()> {
    let config_dir = paths::config_dir();
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;

    if config_dir.exists() {
        ensure_sandbox_dir(name, "/home/agent/.config")?;
        let src = format!("{}/", config_dir.display());
        let dest = format!("{name}:/home/agent/.config/");
        sbx_cp(&src, &dest)?;
    } else {
        debug!(
            "Skipping config copy: {} does not exist",
            config_dir.display()
        );
    }

    match resolve_vault_password_file() {
        Some(password_file) if password_file.exists() => {
            let dest_path = match password_file.strip_prefix(&home_dir) {
                Ok(rel) => format!("/home/agent/{}", rel.display()),
                Err(_) => password_file.display().to_string(),
            };
            if let Some(parent) = Path::new(&dest_path).parent()
                && let Some(parent_str) = parent.to_str()
                && !parent_str.is_empty()
            {
                ensure_sandbox_dir(name, parent_str)?;
            }
            let dest = format!("{name}:{dest_path}");
            sbx_cp(&password_file.display().to_string(), &dest)?;
        }
        Some(password_file) => {
            debug!(
                "Skipping vault password copy: {} does not exist",
                password_file.display()
            );
        }
        None => {
            debug!("Skipping vault password copy: no local vault provider configured");
        }
    }

    Ok(())
}

fn ensure_sandbox_dir(sandbox: &str, dir: &str) -> Result<()> {
    let dir_q = shell_words::quote(dir);
    let cmd = format!("sudo mkdir -p {dir_q} && sudo chown agent:agent {dir_q}");

    debug!("sbx exec {sandbox}: {cmd}");

    let status = Command::new(SBX_BINARY)
        .args(["exec", sandbox, "sh", "-c", &cmd])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to spawn `sbx exec` to prepare destination directory")?;

    if !status.success() {
        bail!("Preparing sandbox directory '{dir}' failed: sbx exec exited with {status}");
    }

    Ok(())
}

fn resolve_vault_password_file() -> Option<PathBuf> {
    Vault::init_bare().ok()?.local_password_file().ok()
}

fn sbx_cp(src: &str, dest: &str) -> Result<()> {
    debug!("sbx cp {src} {dest}");
    let status = Command::new(SBX_BINARY)
        .args(["cp", src, dest])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to spawn `sbx cp`")?;

    if !status.success() {
        bail!("`sbx cp {src} {dest}` exited with {status}");
    }

    Ok(())
}

fn exec_run(name: &str, kit_path: &Path) -> Result<()> {
    let kit_str = kit_path
        .to_str()
        .ok_or_else(|| anyhow!("Kit path is not valid UTF-8: {}", kit_path.display()))?;
    let status = Command::new(SBX_BINARY)
        .args(["run", name, "--kit", kit_str])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to spawn `sbx run`")?;

    if !status.success() {
        bail!("`sbx run` exited with {status}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_name_lowercases() {
        assert_eq!(sanitize_name("Foo"), "foo");
    }

    #[test]
    fn sanitize_name_replaces_non_alphanumeric() {
        assert_eq!(sanitize_name("hello world!"), "hello-world");
    }

    #[test]
    fn sanitize_name_collapses_dash_runs() {
        assert_eq!(sanitize_name("a___b"), "a-b");
    }

    #[test]
    fn sanitize_name_trims_dashes() {
        assert_eq!(sanitize_name("---hi---"), "hi");
    }

    #[test]
    fn sanitize_name_handles_mixed_input() {
        assert_eq!(sanitize_name("My Project (v2)"), "my-project-v2");
    }

    #[test]
    fn sanitize_name_all_invalid_yields_empty() {
        assert_eq!(sanitize_name("///"), "");
    }

    #[test]
    fn resolve_name_uses_explicit_arg() {
        let n = resolve_name(Some("explicit-name".to_string())).unwrap();
        assert_eq!(n, "explicit-name");
    }

    #[test]
    fn resolve_name_sanitizes_explicit_arg() {
        let n = resolve_name(Some("My Sandbox!".to_string())).unwrap();
        assert_eq!(n, "my-sandbox");
    }

    #[test]
    fn resolve_name_rejects_empty_after_sanitize() {
        let err = resolve_name(Some("///".to_string()));
        assert!(err.is_err());
    }

    #[test]
    fn resolve_name_falls_back_to_cwd_when_none() {
        let n = resolve_name(None).unwrap();
        assert!(!n.is_empty());
        assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn compute_kit_hash_is_deterministic() {
        let h1 = compute_kit_hash().unwrap();
        let h2 = compute_kit_hash().unwrap();

        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }
}
