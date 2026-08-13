use anyhow::{Context, Result, anyhow, bail};
use rust_embed::RustEmbed;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use which::which;

pub(crate) mod mcp_credentials;
mod mixins;

pub(crate) use mcp_credentials::sandbox_secret_env_var;

use crate::config::AppConfig;
use crate::config::Config;
use crate::config::VAULT_DATA_FILE_NAME;
use crate::config::paths;
use crate::rag::RagData;
use crate::sandbox::mcp_credentials::MCP_MIXIN_NAME;
use crate::sandbox::mixins::DiscoveredMixin;
use crate::utils::run_command_with_output;
use crate::vault::SECRET_RE;
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

    let config_path = paths::config_file();
    if !config_path.exists() {
        bail!("No Coyote config found. Run `coyote` on your host to complete setup first.");
    }
    let (config, config_content) = Config::load_from_file(&config_path)?;
    let bootstrap = AppConfig {
        vault_password_file: config.vault_password_file.clone(),
        secrets_provider: config.secrets_provider.clone(),
        ..AppConfig::default()
    };
    let vault = Vault::init(&bootstrap)?;
    let registered = sbx_registered_services()?;
    inject_llm_secret(&config_content, &vault, &registered)?;
    if !fresh {
        inject_rag_secrets(&vault, &registered)?;
    }

    let credentials_mixin = if fresh {
        None
    } else {
        inject_mcp_secrets(&vault, &registered)?
    };

    let discovered = mixins::discover()?;

    if sandbox_exists(&name)? {
        info!("Re-attaching to existing sandbox '{name}'");
    } else {
        mixins::log_discovery(&discovered, false);
        create_sandbox(&name, &kit_path, &discovered, credentials_mixin.as_deref())?;
        if !fresh {
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

fn inject_llm_secret(
    config_content: &str,
    vault: &Vault,
    registered: &HashSet<String>,
) -> Result<()> {
    let value: serde_yaml::Value = serde_yaml::from_str(config_content)
        .context("Failed to parse config for LLM secret injection")?;

    let Some(clients) = value.get("clients").and_then(|v| v.as_sequence()) else {
        return Ok(());
    };

    for client in clients {
        let Some(api_key) = client.get("api_key").and_then(|v| v.as_str()) else {
            continue;
        };

        let Some(caps) = SECRET_RE.captures(api_key)? else {
            continue;
        };
        let secret_name = caps[1].to_string();

        let client_type = client.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let client_name = client.get("name").and_then(|v| v.as_str());
        let service = provider_to_sbx_service(client_type, client_name);

        if registered.contains(&service) {
            eprintln!(
                "Secret for '{service}' already registered with sbx. \
                 To update it, run: sbx secret set --force {service}"
            );
            continue;
        }

        let secret_value = vault
            .get_secret(&secret_name, false)
            .with_context(|| format!("Failed to decrypt LLM api_key secret '{secret_name}'"))?;

        sbx_secret_set(&service, &secret_value)?;
    }

    Ok(())
}

/// Registers one sbx secret per distinct `{{placeholder}}` in the MCP config
/// and returns the generated schema-v2 `coyote-mcp` mixin (network egress for
/// every remote MCP server + credential declarations), or `None` when the MCP
/// config references no remote servers and no secrets.
fn inject_mcp_secrets(vault: &Vault, registered: &HashSet<String>) -> Result<Option<String>> {
    let mcp_path = paths::mcp_config_file();
    if !mcp_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(&mcp_path)
        .with_context(|| format!("Failed to read {}", mcp_path.display()))?;
    let mcp: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", mcp_path.display()))?;

    let Some(servers) = mcp.get("mcpServers").and_then(|v| v.as_object()) else {
        return Ok(None);
    };

    let credentials = mcp_credentials::collect_credentials(servers)?;
    let allow_entries = mcp_credentials::collect_server_allow_entries(servers);
    if credentials.is_empty() && allow_entries.is_empty() {
        return Ok(None);
    }

    for credential in &credentials {
        if registered.contains(credential.service_id.as_str()) {
            eprintln!(
                "Secret for '{}' already registered with sbx. \
                 To update it, run: sbx secret set --force {}",
                credential.service_id, credential.service_id
            );
            continue;
        }

        let secret_value = vault
            .get_secret(&credential.secret_name, false)
            .with_context(|| {
                format!(
                    "Secret '{}' referenced by MCP server(s) {} not found \
                     in vault. Add it with: coyote --add-secret {}",
                    credential.secret_name,
                    mcp_credentials::quoted_list(&credential.servers),
                    credential.secret_name
                )
            })?;

        sbx_secret_set(&credential.service_id, &secret_value)?;
    }

    Ok(Some(mcp_credentials::render_mixin_yaml(
        &credentials,
        &allow_entries,
    )?))
}

fn inject_rag_secrets(vault: &Vault, registered: &HashSet<String>) -> Result<()> {
    let rags_dir = paths::rags_dir();
    if !rags_dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(&rags_dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !paths::is_rag_sidecar_name(s) => s.to_string(),
            _ => continue,
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(data) = serde_yaml::from_str::<RagData>(&raw) else {
            continue;
        };
        if !rag_needs_sandbox_secrets(&data) {
            continue;
        }
        let secret_names = driver_config_secret_names(&data);
        let Some((primary, extra)) = secret_names.split_first() else {
            continue;
        };

        let service_id = mcp_credentials::secret_service_id(&stem);
        if !service_id.is_empty() && !registered.contains(&service_id) {
            bind_rag_secret(vault, &service_id, primary, &stem)?;
        }

        for name in extra {
            let id = mcp_credentials::secret_service_id(name);
            if !id.is_empty() && !registered.contains(&id) {
                bind_rag_secret(vault, &id, name, &stem)?;
            }
        }
    }

    Ok(())
}

/// Whether this RAG needs its credential provisioned into the sandbox.
///
/// The question is answered by the driver, not by `attached`. What matters is
/// whether the store sits behind a network hop the sandbox blocks by default
/// and a credential the proxy has to inject; who wrote the documents is
/// irrelevant to both. A Coyote-owned Qdrant RAG dials exactly the same remote
/// host as an attached one, so testing ownership here left its key
/// unprovisioned — queries then failed silently inside the sandbox while
/// working perfectly on the host. yaml and duckdb stores are local files and
/// need neither a credential nor an allow-list entry.
fn rag_needs_sandbox_secrets(data: &RagData) -> bool {
    data.driver == "qdrant"
}

fn driver_config_secret_names(data: &RagData) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for value in data.driver_config.values() {
        let trimmed = value.trim();
        let Ok(Some(caps)) = SECRET_RE.captures(trimmed) else {
            continue;
        };
        if caps.get(0).map(|m| m.as_str()) != Some(trimmed) {
            continue;
        }
        let Some(name) = caps.get(1).map(|m| m.as_str().trim()) else {
            continue;
        };
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

fn bind_rag_secret(vault: &Vault, service_id: &str, secret_name: &str, stem: &str) -> Result<()> {
    match vault.get_secret(secret_name, false) {
        Ok(secret_value) => {
            sbx_secret_set(service_id, &secret_value)
                .context("Failed to register RAG secret with sbx")?;
        }
        Err(e) => {
            eprintln!(
                "Warning: could not load secret '{secret_name}' for RAG '{stem}': {e}. \
                 Queries to this RAG will fail inside the sandbox. \
                 Run `coyote --add-secret {secret_name}` to fix."
            );
        }
    }
    Ok(())
}

fn provider_to_sbx_service(provider_type: &str, client_name: Option<&str>) -> String {
    match provider_type {
        "claude" => "anthropic".to_string(),
        "openai" => "openai".to_string(),
        "gemini" | "vertexai" => "gemini".to_string(),
        "openai-compatible" => client_name.unwrap_or("openai-compatible").to_string(),
        other => client_name.unwrap_or(other).to_string(),
    }
}

fn sbx_registered_services() -> Result<HashSet<String>> {
    let (success, stdout, _) = run_command_with_output(SBX_BINARY, &["secret", "ls"], None)
        .context("Failed to run `sbx secret ls`")?;

    if !success {
        return Ok(HashSet::new());
    }

    Ok(stdout
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let scope = parts.next()?;
            let _kind = parts.next()?;
            let name = parts.next()?;
            if scope == "(global)" {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect())
}

fn sbx_secret_set(service: &str, secret_value: &str) -> Result<()> {
    let mut child = Command::new(SBX_BINARY)
        .args(["secret", "set", service])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn `sbx secret set`")?;

    if let Some(mut stdin_handle) = child.stdin.take() {
        stdin_handle
            .write_all(secret_value.as_bytes())
            .context("Failed to write secret to `sbx secret set` stdin")?;
    }

    let status = child
        .wait()
        .context("Failed to wait for `sbx secret set`")?;

    if !status.success() {
        eprintln!(
            "Warning: failed to register sbx secret '{service}' \
             (`sbx secret set {service}` exited with {status}). \
             Set it manually with: echo '<value>' | sbx secret set {service}"
        );
    }

    Ok(())
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

fn create_sandbox(
    name: &str,
    kit_path: &Path,
    mixins: &[DiscoveredMixin],
    credentials_mixin: Option<&str>,
) -> Result<()> {
    info!("Creating sandbox '{name}'");
    let credentials_kit = credentials_mixin
        .map(|yaml| mixins::wrap_mixin_bytes_as_kit(yaml.as_bytes(), MCP_MIXIN_NAME))
        .transpose()?;
    let args = build_create_args(name, kit_path, mixins, credentials_kit.as_deref())?;
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
    credentials_kit: Option<&Path>,
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

    if let Some(kit) = credentials_kit {
        let cred_str = kit
            .to_str()
            .ok_or_else(|| anyhow!("Credentials kit path is not valid UTF-8: {}", kit.display()))?
            .to_string();
        args.push("--kit".to_string());
        args.push(cred_str);
    }

    args.push(SANDBOX_AGENT.to_string());
    args.push(".".to_string());

    Ok(args)
}

fn copy_host_files(name: &str) -> Result<()> {
    let config_dir = paths::config_dir();

    if config_dir.exists() {
        let sandbox_config_dir = "/home/agent/.config/coyote";
        ensure_sandbox_dir(name, sandbox_config_dir)?;
        let dest = format!("{name}:{sandbox_config_dir}/");
        for entry in fs::read_dir(&config_dir)
            .with_context(|| format!("Failed to read {}", config_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == VAULT_DATA_FILE_NAME) {
                continue;
            }
            sbx_cp(&path.display().to_string(), &dest)?;
        }
        chown_agent_recursive(name, sandbox_config_dir)?;
    } else {
        debug!(
            "Skipping config copy: {} does not exist",
            config_dir.display()
        );
    }

    let oauth_tokens_dir = paths::oauth_tokens_dir();
    if oauth_tokens_dir.exists() {
        let sandbox_cache_dir = "/home/agent/.cache";
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
        chown_agent_recursive(name, sandbox_cache_dir)?;
    } else {
        debug!(
            "Skipping OAuth token copy: {} does not exist",
            oauth_tokens_dir.display()
        );
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

    fn rag_shaped(driver: &str, attached: bool, driver_config: &[(&str, &str)]) -> RagData {
        let mut data = RagData::new("m".into(), 1024, 50, None, 5, None, Default::default());
        data.driver = driver.to_string();
        data.attached = attached;
        for (k, v) in driver_config {
            data.driver_config.insert(k.to_string(), v.to_string());
        }
        data
    }

    fn rag_with(driver_config: &[(&str, &str)]) -> RagData {
        rag_shaped("qdrant", true, driver_config)
    }

    /// The whole point of the driver test. An owned RAG is `attached: false`,
    /// but its collection lives on the same remote host as an attached one, so
    /// filtering on ownership left its key unprovisioned — and the failure is
    /// invisible until someone actually runs a query inside sbx.
    #[test]
    fn an_owned_qdrant_rag_is_provisioned_like_an_attached_one() {
        let owned = rag_shaped("qdrant", false, &[("api_key", "{{QDRANT_KEY}}")]);

        assert!(rag_needs_sandbox_secrets(&owned));
        assert_eq!(
            driver_config_secret_names(&owned),
            vec!["QDRANT_KEY"],
            "passing the filter is worthless if the name it binds is not found"
        );
    }

    #[test]
    fn an_attached_qdrant_rag_is_still_provisioned() {
        let attached = rag_shaped("qdrant", true, &[("api_key", "{{QDRANT_KEY}}")]);

        assert!(rag_needs_sandbox_secrets(&attached));
    }

    /// yaml and duckdb stores are local files: no host to allow, no credential
    /// to inject. The `attached` flag must not change that either way — it no
    /// longer takes part in the decision.
    #[test]
    fn a_local_store_is_never_provisioned() {
        for driver in ["yaml", "duckdb"] {
            for attached in [false, true] {
                let data = rag_shaped(driver, attached, &[("api_key", "{{SOME_KEY}}")]);

                assert!(
                    !rag_needs_sandbox_secrets(&data),
                    "{driver} (attached: {attached}) reaches no network host"
                );
            }
        }
    }

    #[test]
    fn secret_names_are_found_whatever_the_field_is_called() {
        let data = rag_with(&[
            ("host", "qdrant.example.com:6333"),
            ("collection", "docs"),
            ("token", "{{SOME_TOKEN}}"),
        ]);

        assert_eq!(driver_config_secret_names(&data), vec!["SOME_TOKEN"]);
    }

    #[test]
    fn a_literal_credential_is_not_treated_as_a_secret_name() {
        let data = rag_with(&[("api_key", "sk-a-real-looking-key")]);

        assert!(driver_config_secret_names(&data).is_empty());
    }

    #[test]
    fn plain_values_are_never_mistaken_for_secrets() {
        let data = rag_with(&[("host", "localhost:6333"), ("collection", "docs")]);

        assert!(driver_config_secret_names(&data).is_empty());
    }

    #[test]
    fn a_partial_placeholder_is_not_a_credential() {
        let data = rag_with(&[("api_key", "Bearer {{KEY}}")]);

        assert!(driver_config_secret_names(&data).is_empty());
    }

    #[test]
    fn several_secrets_are_all_found_and_deduped() {
        let data = rag_with(&[
            ("api_key", "{{QDRANT_KEY}}"),
            ("host", "localhost:6333"),
            ("token", "{{ OTHER_TOKEN }}"),
            ("fallback_key", "{{QDRANT_KEY}}"),
        ]);

        assert_eq!(
            driver_config_secret_names(&data),
            vec!["QDRANT_KEY", "OTHER_TOKEN"],
            "order follows driver_config, and a repeat is not registered twice"
        );
    }
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
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

        let args = build_create_args("my-box", &kit, &mixins, None).unwrap();

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

        let args = build_create_args("box", &kit, &[], None).unwrap();

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

    #[test]
    fn build_create_args_appends_credentials_kit_after_mixins() {
        let kit = PathBuf::from("/cache/sbx-kit");
        let credentials_kit = PathBuf::from("/cache/sbx-mixin-kits/abc123");

        let args = build_create_args("box", &kit, &[], Some(&credentials_kit)).unwrap();

        assert_eq!(
            args,
            vec![
                "create".to_string(),
                "--name".to_string(),
                "box".to_string(),
                "--kit".to_string(),
                "/cache/sbx-kit".to_string(),
                "--kit".to_string(),
                "/cache/sbx-mixin-kits/abc123".to_string(),
                "coyote".to_string(),
                ".".to_string(),
            ]
        );
    }

    #[test]
    fn build_create_args_orders_base_kit_then_mixins_then_credentials_kit() {
        let kit = PathBuf::from("/cache/sbx-kit");
        let credentials_kit = PathBuf::from("/cache/sbx-mixin-kits/abc123");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("coyote-mixin-cred-{unique}"));
        fs::create_dir_all(&dir).unwrap();

        let mixins = vec![DiscoveredMixin {
            path: dir.clone(),
            label: "user".into(),
            install_count: 0,
            domain_count: 0,
        }];

        let args = build_create_args("box", &kit, &mixins, Some(&credentials_kit)).unwrap();

        assert_eq!(
            args,
            vec![
                "create".to_string(),
                "--name".to_string(),
                "box".to_string(),
                "--kit".to_string(),
                "/cache/sbx-kit".to_string(),
                "--kit".to_string(),
                dir.display().to_string(),
                "--kit".to_string(),
                "/cache/sbx-mixin-kits/abc123".to_string(),
                "coyote".to_string(),
                ".".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_to_sbx_service_maps_gemini_family_to_gemini() {
        assert_eq!(provider_to_sbx_service("gemini", None), "gemini");
        assert_eq!(provider_to_sbx_service("vertexai", None), "gemini");
    }

    #[test]
    fn provider_to_sbx_service_maps_known_providers() {
        assert_eq!(provider_to_sbx_service("claude", None), "anthropic");
        assert_eq!(provider_to_sbx_service("openai", None), "openai");
    }
}
