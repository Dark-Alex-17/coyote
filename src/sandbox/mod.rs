use anyhow::{Context, Result, anyhow, bail};
use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use which::which;

mod mixins;

use gman::providers::SupportedProvider;

use crate::config::paths;
use crate::sandbox::mixins::DiscoveredMixin;
use crate::utils::run_command_with_output;
use crate::vault::Vault;

const SBX_BINARY: &str = "sbx";
pub(crate) const SANDBOX_ENV_FLAG: &str = "IS_SANDBOX";
const SANDBOX_AGENT: &str = "coyote";

#[derive(RustEmbed)]
#[folder = "assets/sbx-kit/"]
struct EmbeddedKit;

#[derive(RustEmbed)]
#[folder = "assets/sbx-vault-mixins/"]
struct EmbeddedVaultMixins;

pub fn launch(name: Option<String>, fresh: bool, no_mixins: bool) -> Result<()> {
    ensure_sbx_installed()?;
    bail_if_nested()?;

    let name = resolve_name(name)?;
    let kit_path = resolve_kit_path()?;

    let discovered = if no_mixins {
        Vec::new()
    } else {
        let mut all = mixins::discover()?;
        if let Ok(vault) = Vault::init_bare()
            && let Some(vault_mixin) = extract_vault_mixin(&vault.provider)?
        {
            all.insert(0, vault_mixin);
        }
        all
    };

    if sandbox_exists(&name)? {
        info!("Re-attaching to existing sandbox '{name}'");
        if fresh {
            debug!("--fresh ignored: re-attaching to existing sandbox '{name}'");
        }
        if no_mixins {
            debug!("--no-mixins ignored: re-attaching to existing sandbox '{name}'");
        }
    } else {
        mixins::log_discovery(&discovered, no_mixins);

        if fresh {
            let msg = format!("Creating fresh sandbox '{name}' (no host config will be copied)");
            info!("{msg}");
            println!("{msg}");
            create_sandbox(&name, &kit_path, &discovered)?;
        } else {
            create_sandbox(&name, &kit_path, &discovered)?;
            copy_host_files(&name)?;
        }
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

fn extract_vault_mixin(provider: &SupportedProvider) -> Result<Option<DiscoveredMixin>> {
    let provider_dir = match provider {
        SupportedProvider::Local { .. } => return Ok(None),
        SupportedProvider::AwsSecretsManager { .. } => "aws_secrets_manager",
        SupportedProvider::GcpSecretManager { .. } => "gcp_secret_manager",
        SupportedProvider::AzureKeyVault { .. } => "azure_key_vault",
        SupportedProvider::Gopass { .. } => "gopass",
        SupportedProvider::OnePassword { .. } => "one_password",
    };

    let cache_root = extract_vault_mixins_cache()?;
    let provider_root = cache_root.join(provider_dir);
    let spec_path = provider_root.join("spec.yaml");

    if !spec_path.exists() {
        bail!(
            "Embedded vault mixin for '{provider_dir}' is missing spec.yaml at {}",
            spec_path.display()
        );
    }

    let label = format!("<built-in: vault-{provider_dir}>");
    let (install_count, domain_count) = mixins::summarize(&spec_path)?;

    Ok(Some(DiscoveredMixin {
        path: provider_root,
        label,
        install_count,
        domain_count,
    }))
}

fn extract_vault_mixins_cache() -> Result<PathBuf> {
    let cache_root = paths::sbx_vault_mixins_dir();
    let new_hash = compute_vault_mixins_hash()?;
    let hash_file = paths::sbx_vault_mixins_hash_file();
    if let Ok(existing) = fs::read_to_string(&hash_file)
        && existing == new_hash
    {
        return Ok(cache_root);
    }

    if cache_root.exists() {
        fs::remove_dir_all(&cache_root).with_context(|| {
            format!(
                "Failed to clear stale vault mixins at {}",
                cache_root.display()
            )
        })?;
    }
    fs::create_dir_all(&cache_root)
        .with_context(|| format!("Failed to create {}", cache_root.display()))?;

    for entry in EmbeddedVaultMixins::iter() {
        let file = EmbeddedVaultMixins::get(&entry).ok_or_else(|| {
            anyhow!("Embedded vault mixin file missing during extraction: {entry}")
        })?;
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
    debug!(
        "Extracted embedded sbx-vault-mixins to {}",
        cache_root.display()
    );

    Ok(cache_root)
}

fn compute_vault_mixins_hash() -> Result<String> {
    let mut hasher = Sha256::new();
    let mut entries: Vec<_> = EmbeddedVaultMixins::iter().collect();
    entries.sort();

    for entry in &entries {
        let file = EmbeddedVaultMixins::get(entry)
            .ok_or_else(|| anyhow!("Embedded vault mixin file missing during hash: {entry}"))?;
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

fn create_sandbox(name: &str, kit_path: &Path, mixins: &[DiscoveredMixin]) -> Result<()> {
    info!("Creating sandbox '{name}'");
    let args = build_create_args(name, kit_path, mixins)?;
    debug!("sbx {}", args.join(" "));
    let status = Command::new(SBX_BINARY)
        .args(&args)
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

fn build_create_args(
    name: &str,
    kit_path: &Path,
    mixins: &[DiscoveredMixin],
) -> Result<Vec<String>> {
    let kit_str = kit_path
        .to_str()
        .ok_or_else(|| anyhow!("Kit path is not valid UTF-8: {}", kit_path.display()))?;

    let mut args = vec![
        "create".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--kit".to_string(),
        kit_str.to_string(),
    ];

    for mixin in mixins {
        let mixin_kit = mixin.kit_path()?;
        let mixin_str = mixin_kit
            .to_str()
            .ok_or_else(|| anyhow!("Mixin kit path is not valid UTF-8: {}", mixin_kit.display()))?
            .to_string();
        args.push("--kit".to_string());
        args.push(mixin_str);
    }

    args.push(SANDBOX_AGENT.to_string());
    args.push(".".to_string());

    Ok(args)
}

fn copy_host_files(name: &str) -> Result<()> {
    let config_dir = paths::config_dir();
    let home_dir = dirs::home_dir().context("Could not determine home directory")?;

    if config_dir.exists() {
        let sandbox_config_dir = "/home/agent/.config/coyote";
        ensure_sandbox_dir(name, sandbox_config_dir)?;
        let dest = format!("{name}:{sandbox_config_dir}/");
        for entry in fs::read_dir(&config_dir)
            .with_context(|| format!("Failed to read {}", config_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            sbx_cp(&path.display().to_string(), &dest)?;
        }
        chown_agent_recursive(name, sandbox_config_dir)?;
    } else {
        debug!(
            "Skipping config copy: {} does not exist",
            config_dir.display()
        );
    }

    let oauth_tokens_dir = paths::oauth_tokens_path();
    if oauth_tokens_dir.exists() {
        let sandbox_oauth_dir = "/home/agent/.cache/coyote/oauth";
        ensure_sandbox_dir(name, sandbox_oauth_dir)?;
        let dest = format!("{name}:{sandbox_oauth_dir}/");
        for entry in fs::read_dir(&oauth_tokens_dir)
            .with_context(|| format!("Failed to read {}", oauth_tokens_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            sbx_cp(&path.display().to_string(), &dest)?;
        }
        chown_agent_recursive(name, sandbox_oauth_dir)?;
    } else {
        debug!(
            "Skipping OAuth token copy: {} does not exist",
            oauth_tokens_dir.display()
        );
    }

    match resolve_vault_password_file() {
        Some(password_file) if password_file.exists() => {
            let dest_path = host_to_sandbox_path(&password_file, &home_dir, cfg!(windows))?;
            if let Some(parent) = sandbox_path_parent(&dest_path)
                && !parent.is_empty()
            {
                ensure_sandbox_dir(name, parent)?;
            }
            let dest = format!("{name}:{dest_path}");
            sbx_cp(&password_file.display().to_string(), &dest)?;
            chown_agent_recursive(name, &dest_path)?;
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

fn host_to_sandbox_path(
    host_path: &Path,
    home_dir: &Path,
    is_windows_host: bool,
) -> Result<String> {
    let host_str = host_path.to_str().context("Host path is not valid UTF-8")?;
    let home_str = home_dir
        .to_str()
        .context("Home directory is not valid UTF-8")?;

    if let Some(rel) = strip_host_home(host_str, home_str) {
        let unixified = rel.replace('\\', "/");
        return Ok(format!("/home/agent/{unixified}"));
    }

    if is_windows_host {
        bail!(
            "Path '{host_str}' is outside your Windows user profile ({home_str}). \
             Sandbox mode cannot copy files from outside %USERPROFILE% into a Linux \
             sandbox. Move the file under your user profile and update your config \
             accordingly."
        );
    }

    Ok(host_str.to_string())
}

fn strip_host_home(path: &str, home: &str) -> Option<String> {
    let path_norm: String = path
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let home_norm: String = home
        .chars()
        .map(|c| if c == '\\' { '/' } else { c })
        .collect();
    let home_norm = home_norm.trim_end_matches('/');

    if home_norm.is_empty() || path_norm.len() <= home_norm.len() {
        return None;
    }

    let (head, tail) = path_norm.split_at(home_norm.len());
    if head != home_norm || !tail.starts_with('/') {
        return None;
    }

    Some(tail[1..].to_string())
}

fn sandbox_path_parent(linux_path: &str) -> Option<&str> {
    linux_path.rsplit_once('/').map(|(parent, _)| parent)
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
    debug!("sbx run --name {name} --kit {kit_str}");
    let status = Command::new(SBX_BINARY)
        .args(["run", "--name", name, "--kit", kit_str])
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

fn chown_agent_recursive(sandbox: &str, path: &str) -> Result<()> {
    let path_q = shell_words::quote(path);
    let cmd = format!("sudo chown -R agent:agent {path_q}");

    debug!("sbx exec {sandbox}: {cmd}");

    let status = Command::new(SBX_BINARY)
        .args(["exec", sandbox, "sh", "-c", &cmd])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to spawn `sbx exec` to chown copied files")?;

    if !status.success() {
        bail!("Chowning '{path}' in sandbox failed: sbx exec exited with {status}");
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

    #[test]
    fn build_create_args_emits_base_kit_before_mixins() {
        let kit = PathBuf::from("/cache/sbx-kit");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir_a = env::temp_dir().join(format!("coyote-mixin-a-{unique}"));
        let dir_b = env::temp_dir().join(format!("coyote-mixin-b-{unique}"));
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let mixins = vec![
            DiscoveredMixin {
                path: dir_a.clone(),
                label: "user".into(),
                install_count: 0,
                domain_count: 0,
            },
            DiscoveredMixin {
                path: dir_b.clone(),
                label: "sql".into(),
                install_count: 0,
                domain_count: 0,
            },
        ];

        let args = build_create_args("my-box", &kit, &mixins).unwrap();

        assert_eq!(
            args,
            vec![
                "create".to_string(),
                "--name".to_string(),
                "my-box".to_string(),
                "--kit".to_string(),
                "/cache/sbx-kit".to_string(),
                "--kit".to_string(),
                dir_a.display().to_string(),
                "--kit".to_string(),
                dir_b.display().to_string(),
                "coyote".to_string(),
                ".".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn build_create_args_with_no_mixins_omits_mixin_kits() {
        let kit = PathBuf::from("/cache/sbx-kit");
        let args = build_create_args("box", &kit, &[]).unwrap();
        assert_eq!(
            args,
            vec![
                "create".to_string(),
                "--name".to_string(),
                "box".to_string(),
                "--kit".to_string(),
                "/cache/sbx-kit".to_string(),
                "coyote".to_string(),
                ".".to_string(),
            ]
        );
    }

    mod vault_mixins {
        use super::*;
        use crate::utils::get_env_name;
        use gman::providers::aws_secrets_manager::AwsSecretsManagerProvider;
        use gman::providers::azure_key_vault::AzureKeyVaultProvider;
        use gman::providers::gcp_secret_manager::GcpSecretManagerProvider;
        use gman::providers::gopass::GopassProvider;
        use gman::providers::local::LocalProvider;
        use gman::providers::one_password::OnePasswordProvider;
        use serial_test::serial;
        use std::time::{SystemTime, UNIX_EPOCH};

        struct TestCacheDirGuard {
            key: String,
            previous: Option<std::ffi::OsString>,
            path: PathBuf,
        }

        impl TestCacheDirGuard {
            fn new() -> Self {
                let key = get_env_name("cache_dir");
                let previous = env::var_os(&key);
                let unique = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = env::temp_dir().join(format!("coyote-sandbox-vault-tests-{unique}"));
                fs::create_dir_all(&path).unwrap();
                unsafe {
                    env::set_var(&key, &path);
                }
                Self {
                    key,
                    previous,
                    path,
                }
            }
        }

        impl Drop for TestCacheDirGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.previous {
                        Some(v) => env::set_var(&self.key, v),
                        None => env::remove_var(&self.key),
                    }
                }
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        #[test]
        fn returns_none_for_local() {
            let p = SupportedProvider::Local {
                provider_def: LocalProvider::default(),
            };
            assert!(extract_vault_mixin(&p).unwrap().is_none());
        }

        #[test]
        #[serial]
        fn returns_some_for_aws() {
            let _guard = TestCacheDirGuard::new();
            let p = SupportedProvider::AwsSecretsManager {
                provider_def: AwsSecretsManagerProvider {
                    aws_profile: None,
                    aws_region: None,
                },
            };
            let m = extract_vault_mixin(&p)
                .unwrap()
                .expect("expected vault mixin");
            assert!(m.path.join("spec.yaml").exists());
            assert!(m.label.contains("aws_secrets_manager"));
        }

        #[test]
        #[serial]
        fn returns_some_for_gcp() {
            let _guard = TestCacheDirGuard::new();
            let p = SupportedProvider::GcpSecretManager {
                provider_def: GcpSecretManagerProvider {
                    gcp_project_id: None,
                },
            };
            let m = extract_vault_mixin(&p)
                .unwrap()
                .expect("expected vault mixin");
            assert!(m.path.join("spec.yaml").exists());
            assert!(m.label.contains("gcp_secret_manager"));
        }

        #[test]
        #[serial]
        fn returns_some_for_one_password() {
            let _guard = TestCacheDirGuard::new();
            let p = SupportedProvider::OnePassword {
                provider_def: OnePasswordProvider {
                    vault: None,
                    account: None,
                },
            };
            let m = extract_vault_mixin(&p)
                .unwrap()
                .expect("expected vault mixin");
            assert!(m.path.join("spec.yaml").exists());
            assert!(m.label.contains("one_password"));
        }

        #[test]
        #[serial]
        fn returns_some_for_azure() {
            let _guard = TestCacheDirGuard::new();
            let p = SupportedProvider::AzureKeyVault {
                provider_def: AzureKeyVaultProvider { vault_name: None },
            };
            let m = extract_vault_mixin(&p)
                .unwrap()
                .expect("expected vault mixin");
            assert!(m.path.join("spec.yaml").exists());
            assert!(m.label.contains("azure_key_vault"));
        }

        #[test]
        #[serial]
        fn returns_some_for_gopass() {
            let _guard = TestCacheDirGuard::new();
            let p = SupportedProvider::Gopass {
                provider_def: GopassProvider { store: None },
            };
            let m = extract_vault_mixin(&p)
                .unwrap()
                .expect("expected vault mixin");
            assert!(m.path.join("spec.yaml").exists());
            assert!(m.label.contains("gopass"));
        }

        #[test]
        fn hash_is_deterministic() {
            let h1 = compute_vault_mixins_hash().unwrap();
            let h2 = compute_vault_mixins_hash().unwrap();
            assert_eq!(h1, h2);
            assert_eq!(h1.len(), 64);
        }
    }

    mod host_to_sandbox_path_tests {
        use super::*;

        #[test]
        fn linux_under_home() {
            let dest = host_to_sandbox_path(
                Path::new("/home/atusa/.coyote_password"),
                Path::new("/home/atusa"),
                false,
            )
            .unwrap();

            assert_eq!(dest, "/home/agent/.coyote_password");
        }

        #[test]
        fn linux_nested_under_home() {
            let dest = host_to_sandbox_path(
                Path::new("/home/atusa/.config/coyote/.password"),
                Path::new("/home/atusa"),
                false,
            )
            .unwrap();

            assert_eq!(dest, "/home/agent/.config/coyote/.password");
        }

        #[test]
        fn linux_outside_home_returns_verbatim() {
            let dest = host_to_sandbox_path(
                Path::new("/etc/coyote/.password"),
                Path::new("/home/atusa"),
                false,
            )
            .unwrap();

            assert_eq!(dest, "/etc/coyote/.password");
        }

        #[test]
        fn macos_under_home_with_spaces() {
            let dest = host_to_sandbox_path(
                Path::new("/Users/atusa/Library/Application Support/coyote/.password"),
                Path::new("/Users/atusa"),
                false,
            )
            .unwrap();

            assert_eq!(
                dest,
                "/home/agent/Library/Application Support/coyote/.password"
            );
        }

        #[test]
        fn windows_under_home_converts_backslashes() {
            let dest = host_to_sandbox_path(
                Path::new(r"C:\Users\atusa\.coyote_password"),
                Path::new(r"C:\Users\atusa"),
                true,
            )
            .unwrap();

            assert_eq!(dest, "/home/agent/.coyote_password");
        }

        #[test]
        fn windows_nested_under_home() {
            let dest = host_to_sandbox_path(
                Path::new(r"C:\Users\atusa\Documents\my\vault.txt"),
                Path::new(r"C:\Users\atusa"),
                true,
            )
            .unwrap();

            assert_eq!(dest, "/home/agent/Documents/my/vault.txt");
        }

        #[test]
        fn windows_outside_home_bails_with_clear_error() {
            let err = host_to_sandbox_path(
                Path::new(r"C:\Program Files\Coyote\vault.txt"),
                Path::new(r"C:\Users\atusa"),
                true,
            )
            .unwrap_err();

            let msg = err.to_string();
            assert!(
                msg.contains("Program Files"),
                "error should name the offending path: {msg}"
            );
            assert!(
                msg.contains("user profile"),
                "error should explain the limitation: {msg}"
            );
        }

        #[test]
        fn windows_tolerates_trailing_slash_in_home() {
            let dest = host_to_sandbox_path(
                Path::new(r"C:\Users\atusa\foo"),
                Path::new(r"C:\Users\atusa\"),
                true,
            )
            .unwrap();

            assert_eq!(dest, "/home/agent/foo");
        }

        #[test]
        fn sandbox_path_parent_extracts_parent_for_nested() {
            assert_eq!(
                sandbox_path_parent("/home/agent/.coyote_password"),
                Some("/home/agent")
            );
            assert_eq!(
                sandbox_path_parent("/etc/coyote/.password"),
                Some("/etc/coyote")
            );
        }

        #[test]
        fn sandbox_path_parent_handles_edge_cases() {
            assert_eq!(sandbox_path_parent("/file"), Some(""));
            assert_eq!(sandbox_path_parent("noparent"), None);
        }
    }
}
