use anyhow::{Context, Result, anyhow, bail};
use rust_embed::RustEmbed;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, io};
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
    let registered = sbx_registered_secrets()?;
    inject_llm_secret(&config_content, &vault, &registered.services)?;
    let mut custom_plans: BTreeMap<String, CustomSecretPlan> = BTreeMap::new();
    if !fresh {
        collect_rag_custom_secrets(&mut custom_plans)?;
    }

    let credentials_mixin = if fresh {
        None
    } else {
        inject_mcp_secrets(&vault, &registered, &mut custom_plans)?
    };
    let new_custom_envs = provision_custom_secrets(&vault, &registered, custom_plans)?;

    let discovered = mixins::discover()?;

    if sandbox_exists(&name)? {
        info!("Re-attaching to existing sandbox '{name}'");
        if !fresh {
            warn_if_mixin_drifted(&name, credentials_mixin.as_deref());
            if !new_custom_envs.is_empty() {
                eprintln!(
                    "Custom secret env var(s) {} were just registered; restart sandbox \
                     '{name}' for them to appear in its environment.",
                    mcp_credentials::quoted_list(&new_custom_envs)
                );
            }
        }
    } else {
        mixins::log_discovery(&discovered, false);
        create_sandbox(&name, &kit_path, &discovered, credentials_mixin.as_deref())?;
        persist_mixin_hash(&name, credentials_mixin.as_deref());
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

/// The generated `coyote-mcp` mixin is baked into a sandbox at create time and
/// never re-applied on re-attach, so its hash is persisted per sandbox to
/// detect when the MCP config drifts from the rules the sandbox runs with.
/// An absent mixin hashes as the empty string, keeping the comparison total.
fn credentials_mixin_hash(mixin: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(mixin.unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())
}

fn persist_mixin_hash(name: &str, mixin: Option<&str>) {
    let path = paths::sandbox_mixin_hash_file(name);
    let write = |path: &Path| -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(path, credentials_mixin_hash(mixin))
    };

    if let Err(e) = write(&path) {
        eprintln!(
            "Warning: failed to record the sandbox mixin hash at {} ({e}); \
             stale-rule detection is disabled for sandbox '{name}'.",
            path.display()
        );
    }
}

fn warn_if_mixin_drifted(name: &str, mixin: Option<&str>) {
    let path = paths::sandbox_mixin_hash_file(name);
    let Ok(stored) = fs::read_to_string(&path) else {
        return;
    };

    if stored.trim() != credentials_mixin_hash(mixin) {
        eprintln!(
            "Warning: the MCP config changed since sandbox '{name}' was created; its \
             baked-in network and credential rules are stale. Remove and re-create \
             the sandbox to apply the new rules: sbx rm {name}"
        );
    }
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
///
/// Proxy-managed credentials go through `sbx secret set` and are declared in
/// the mixin. The rest (slot-conflicted or non-header secrets) go through
/// `sbx secret set-custom`; they are only accumulated into `custom_plans`
/// here. `provision_custom_secrets` registers each env var once with the
/// union of targets from every source (MCP and RAG) that needs it.
fn inject_mcp_secrets(
    vault: &Vault,
    registered: &SbxSecrets,
    custom_plans: &mut BTreeMap<String, CustomSecretPlan>,
) -> Result<Option<String>> {
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
        if credential.proxy_managed {
            if registered.services.contains(credential.service_id.as_str()) {
                eprintln!(
                    "Secret for '{}' already registered with sbx. \
                     To update it, run: sbx secret set --force {}",
                    credential.service_id, credential.service_id
                );
                continue;
            }

            let secret_value = vault
                .get_secret(&credential.secret_name, false)
                .with_context(|| mcp_secret_missing_hint(credential))?;

            sbx_secret_set(&credential.service_id, &secret_value)?;
            continue;
        }

        add_custom_secret_plan(
            custom_plans,
            &credential.secret_name,
            credential.custom_hosts.iter().cloned(),
            format!(
                "MCP server(s) {}",
                mcp_credentials::quoted_list(&credential.servers)
            ),
            true,
        );
    }

    Ok(Some(mcp_credentials::render_mixin_yaml(
        &credentials,
        &allow_entries,
    )?))
}

fn mcp_secret_missing_hint(credential: &mcp_credentials::CredentialSpec) -> String {
    format!(
        "Secret '{}' referenced by MCP server(s) {} not found \
         in vault. Add it with: coyote --add-secret {}",
        credential.secret_name,
        mcp_credentials::quoted_list(&credential.servers),
        credential.secret_name
    )
}

/// One custom secret to provision, accumulated across every source (RAG
/// driver configs, demoted MCP credentials) before anything is registered,
/// so an env var shared by several sources gets exactly one registration
/// with the union of their target hosts.
#[derive(Debug, PartialEq, Eq)]
struct CustomSecretPlan {
    secret_name: String,
    hosts: BTreeSet<String>,
    /// Human labels ("RAG 'docs'", "MCP server(s) 'kong'") for notices.
    sources: BTreeSet<String>,
    /// When false a missing vault secret only warns (RAG behavior); any
    /// strict source (MCP) upgrades the whole plan to a hard error.
    strict: bool,
}

fn add_custom_secret_plan(
    plans: &mut BTreeMap<String, CustomSecretPlan>,
    secret_name: &str,
    hosts: impl IntoIterator<Item = String>,
    source: String,
    strict: bool,
) {
    let plan = plans
        .entry(sandbox_secret_env_var(secret_name))
        .or_insert_with(|| CustomSecretPlan {
            secret_name: secret_name.to_string(),
            hosts: BTreeSet::new(),
            sources: BTreeSet::new(),
            strict: false,
        });
    plan.hosts.extend(hosts);
    plan.sources.insert(source);
    plan.strict |= strict;
}

/// Target hosts for a custom secret; falls back to the `'**'` wildcard (match
/// any host) when none could be derived, so the secret is still provisioned.
fn custom_secret_targets(plan: &CustomSecretPlan) -> Vec<String> {
    if plan.hosts.is_empty() {
        eprintln!(
            "Warning: no target host could be derived for secret '{}'; \
             registering its sandbox custom secret with the wildcard target '**', \
             so the proxy replaces its placeholder in headers sent to ANY host.",
            plan.secret_name
        );
        return vec!["**".to_string()];
    }

    plan.hosts.iter().cloned().collect()
}

#[derive(Debug, PartialEq, Eq)]
enum CustomSecretAction {
    /// No custom secret is registered for this env var yet.
    Register {
        targets: Vec<String>,
    },
    Covered,
    /// Targets drifted. sbx cannot update targets in place, so the existing
    /// registration is removed (by placeholder) and re-registered with the
    /// union of old and new targets. A union so that scope widened outside
    /// Coyote is never narrowed. Values are re-seeded from the vault, so
    /// this is an update, not a deletion.
    Replace {
        placeholder: String,
        targets: Vec<String>,
    },
}

fn plan_custom_secret_action(
    existing: Option<&CustomSecret>,
    wanted: &[String],
) -> CustomSecretAction {
    let Some(existing) = existing else {
        return CustomSecretAction::Register {
            targets: wanted.to_vec(),
        };
    };

    let covered =
        existing.targets.contains("**") || wanted.iter().all(|t| existing.targets.contains(t));
    if covered {
        return CustomSecretAction::Covered;
    }

    let targets: Vec<String> = existing
        .targets
        .iter()
        .chain(wanted.iter())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    CustomSecretAction::Replace {
        placeholder: existing.placeholder.clone(),
        targets,
    }
}

/// Registers every accumulated custom secret with sbx and returns the env
/// vars that were newly (re-)registered. Never re-registers on a value
/// change (values are write-once here) only on target drift.
fn provision_custom_secrets(
    vault: &Vault,
    registered: &SbxSecrets,
    plans: BTreeMap<String, CustomSecretPlan>,
) -> Result<Vec<String>> {
    let mut new_envs = Vec::new();
    for (env_var, plan) in plans {
        let targets = custom_secret_targets(&plan);
        let sources = plan.sources.iter().cloned().collect::<Vec<_>>().join(", ");
        eprintln!(
            "Secret '{}' (used by {sources}) resolves to a proxy placeholder inside \
             the sandbox (env var {env_var}); the real value is only injected into \
             HTTP(S) request headers sent to: {}.",
            plan.secret_name,
            targets.join(", ")
        );

        let action = plan_custom_secret_action(registered.custom.get(&env_var), &targets);
        if let CustomSecretAction::Covered = action {
            eprintln!("Custom secret '{env_var}' already registered with sbx.");
            let existing = &registered.custom[&env_var];
            if existing.targets.contains("**") && targets != ["**"] {
                eprintln!(
                    "Note: the existing registration targets the wildcard '**', wider \
                     than the derived host(s) {}. To re-scope it, remove it with \
                     `sbx secret rm --placeholder {} -f` and re-launch.",
                    mcp_credentials::quoted_list(&targets),
                    existing.placeholder
                );
            }
            continue;
        }

        // Resolve the value BEFORE any removal so a missing vault secret
        // never destroys an existing registration.
        let secret_value = match vault.get_secret(&plan.secret_name, false) {
            Ok(value) => value,
            Err(e) if !plan.strict => {
                eprintln!(
                    "Warning: could not load secret '{}' (used by {sources}): {e}. \
                     Requests that need it will fail inside the sandbox. \
                     Run `coyote --add-secret {}` to fix.",
                    plan.secret_name, plan.secret_name
                );
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Secret '{}' (used by {sources}) not found in vault. \
                         Add it with: coyote --add-secret {}",
                        plan.secret_name, plan.secret_name
                    )
                });
            }
        };

        let (targets, replaced) = match action {
            CustomSecretAction::Register { targets } => (targets, false),
            CustomSecretAction::Replace {
                placeholder,
                targets,
            } => {
                eprintln!(
                    "Updating the sbx custom secret for '{env_var}' to cover target \
                     host(s) {}.",
                    mcp_credentials::quoted_list(&targets)
                );

                if !sbx_secret_rm_custom(&placeholder)? {
                    continue;
                }

                (targets, true)
            }
            CustomSecretAction::Covered => unreachable!("handled above"),
        };

        if sbx_secret_set_custom(&env_var, &targets, &secret_value)? {
            new_envs.push(env_var);
        } else if replaced {
            eprintln!(
                "Warning: the old registration for '{env_var}' was removed but \
                 re-registering it failed; it will be re-registered from the vault \
                 on the next launch."
            );
        }
    }

    Ok(new_envs)
}

/// Accumulates every attached RAG's driver_config secrets into `custom_plans`
/// (bound to `COYOTE_SECRET_<NAME>`, the env var `interpolate_secrets`
/// resolves inside the sandbox). Non-strict: a missing vault secret warns at
/// provisioning time instead of failing the launch, meaning only that RAG's
/// queries would fail.
fn collect_rag_custom_secrets(custom_plans: &mut BTreeMap<String, CustomSecretPlan>) -> Result<()> {
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
        if !data.attached {
            continue;
        }
        let secret_names = driver_config_secret_names(&data);
        if secret_names.is_empty() {
            continue;
        }

        let hosts = rag_driver_hosts(&data);
        for secret_name in &secret_names {
            add_custom_secret_plan(
                custom_plans,
                secret_name,
                hosts.iter().cloned(),
                format!("RAG '{stem}'"),
                false,
            );
        }
    }

    Ok(())
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

/// Derives custom-secret target hosts from a RAG's driver_config: any http(s)
/// URL in a value contributes its host, and the `host`/`url` keys also accept
/// a bare `host[:port]` value (e.g. `qdrant.example.com:6333`). Ports are
/// stripped (sbx custom-secret targets are host-only).
fn rag_driver_hosts(data: &RagData) -> Vec<String> {
    let mut hosts: BTreeSet<String> = BTreeSet::new();
    for (key, value) in &data.driver_config {
        let trimmed = value.trim();
        hosts.extend(mcp_credentials::hosts_in_text(trimmed));
        if (key == "host" || key == "url")
            && let Some(host) = bare_host(trimmed)
        {
            hosts.insert(host);
        }
    }

    hosts.into_iter().collect()
}

fn bare_host(value: &str) -> Option<String> {
    if value.is_empty()
        || value.contains("://")
        || value.contains("{{")
        || value.contains('/')
        || value.contains(char::is_whitespace)
    {
        return None;
    }

    let host = match value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        Some(_) => value,
        None => value,
    };

    if host.is_empty() || host.contains(':') || host.starts_with('[') {
        return None;
    }

    Some(host.to_string())
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

#[derive(Debug, Default)]
struct SbxSecrets {
    services: HashSet<String>,
    custom: HashMap<String, CustomSecret>,
}

#[derive(Debug)]
struct CustomSecret {
    targets: BTreeSet<String>,
    placeholder: String,
}

fn sbx_registered_secrets() -> Result<SbxSecrets> {
    let (success, stdout, stderr) = run_command_with_output(SBX_BINARY, &["secret", "ls"], None)
        .context("Failed to run `sbx secret ls`")?;

    if !success {
        eprintln!(
            "Warning: `sbx secret ls` failed ({}); Coyote cannot tell which secrets \
             are already registered and may attempt to re-register existing ones.",
            stderr.trim()
        );
        return Ok(SbxSecrets::default());
    }

    Ok(parse_sbx_secret_ls(&stdout))
}

fn parse_sbx_secret_ls(stdout: &str) -> SbxSecrets {
    let mut secrets = SbxSecrets::default();
    let mut in_custom = false;
    let mut in_header = true;
    let mut custom_body_lines = 0usize;
    let mut custom_rows = 0usize;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "CUSTOM SECRETS" {
            in_custom = true;
            in_header = true;
            continue;
        }
        if in_header {
            in_header = false;
            continue;
        }

        if in_custom {
            custom_body_lines += 1;
            let cols = split_columns(line);
            let [scope, targets, env, placeholder, ..] = cols.as_slice() else {
                continue;
            };
            custom_rows += 1;
            if *scope != "(global)" {
                continue;
            }
            secrets.custom.insert(
                (*env).to_string(),
                CustomSecret {
                    targets: targets.split(',').map(|t| t.trim().to_string()).collect(),
                    placeholder: (*placeholder).to_string(),
                },
            );
        } else {
            let mut parts = line.split_whitespace();
            let (Some(scope), Some(_kind), Some(name)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if scope == "(global)" {
                secrets.services.insert(name.to_string());
            }
        }
    }

    if custom_body_lines > 0 && custom_rows == 0 {
        eprintln!(
            "Warning: no rows could be parsed from the CUSTOM SECRETS section of \
             `sbx secret ls`; its output format may have changed. Custom-secret \
             idempotency checks are disabled for this launch."
        );
    }

    secrets
}

fn split_columns(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line.trim();
    while !rest.is_empty() {
        match rest.find("  ") {
            Some(idx) => {
                out.push(&rest[..idx]);
                rest = rest[idx..].trim_start();
            }
            None => {
                out.push(rest);
                break;
            }
        }
    }

    out
}

fn sbx_secret_set(service: &str, secret_value: &str) -> Result<()> {
    let mut child = Command::new(SBX_BINARY)
        .args(["secret", "set", service])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn `sbx secret set`")?;

    if let Some(mut stdin_handle) = child.stdin.take()
        && let Err(e) = stdin_handle.write_all(secret_value.as_bytes())
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(anyhow!(e).context("Failed to write secret to `sbx secret set` stdin"));
    }

    let status = child
        .wait()
        .context("Failed to wait for `sbx secret set`")?;

    if !status.success() {
        eprintln!(
            "Warning: failed to register sbx secret '{service}' \
             (`sbx secret set {service}` exited with {status}). \
             Set it manually with: sbx secret set {service} \
             (the value is read from the prompt)"
        );
    }

    Ok(())
}

fn sbx_secret_set_custom(env_var: &str, targets: &[String], secret_value: &str) -> Result<bool> {
    let mut args: Vec<&str> = vec!["secret", "set-custom", "--env", env_var];
    for target in targets {
        args.push("--host");
        args.push(target);
    }

    debug!(
        "sbx secret set-custom --env {env_var} (targets: {})",
        targets.join(", ")
    );

    let mut child = Command::new(SBX_BINARY)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn `sbx secret set-custom`")?;

    if let Some(mut stdin_handle) = child.stdin.take()
        && let Err(e) = stdin_handle.write_all(secret_value.as_bytes())
        && e.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(anyhow!(e).context("Failed to write secret to `sbx secret set-custom` stdin"));
    }

    let status = child
        .wait()
        .context("Failed to wait for `sbx secret set-custom`")?;

    if !status.success() {
        let host_flags: String = targets.iter().map(|t| format!(" --host '{t}'")).collect();
        eprintln!(
            "Warning: failed to register sbx custom secret '{env_var}' \
             (`sbx secret set-custom` exited with {status}). Set it manually with: \
             sbx secret set-custom --env {env_var}{host_flags} \
             (the value is read from the prompt)"
        );
    }

    Ok(status.success())
}

fn sbx_secret_rm_custom(placeholder: &str) -> Result<bool> {
    debug!("sbx secret rm --placeholder {placeholder} -f");
    let status = Command::new(SBX_BINARY)
        .args(["secret", "rm", "--placeholder", placeholder, "-f"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to spawn `sbx secret rm`")?;

    if !status.success() {
        eprintln!(
            "Warning: failed to remove the outdated sbx custom secret \
             (`sbx secret rm --placeholder {placeholder} -f` exited with {status}); \
             its targets were left unchanged."
        );
    }

    Ok(status.success())
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

    fn rag_with(driver_config: &[(&str, &str)]) -> RagData {
        let mut data = RagData::new("m".into(), 1024, 50, None, 5, None, Default::default());
        data.driver = "qdrant".to_string();
        data.attached = true;
        for (k, v) in driver_config {
            data.driver_config.insert(k.to_string(), v.to_string());
        }
        data
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

    /// Pinned to the `sbx secret ls` output shape of sbx v0.38.0.
    const SECRET_LS_SAMPLE: &str = "\
SCOPE      TYPE      NAME               SECRET
(global)   service   github             (stored)
(global)   service   kong-prod-pat      (stored)
(global)   service   anthropic          (oauth configured)

CUSTOM SECRETS
SCOPE      TARGETS          ENV              PLACEHOLDER               SECRET
(global)   api.stripe.com   STRIPE_API_KEY   sbx-cs-97BPiO11AS93Tlo5   rk_liv******...******JT6n
";

    #[test]
    fn parse_sbx_secret_ls_reads_both_sections() {
        let secrets = parse_sbx_secret_ls(SECRET_LS_SAMPLE);

        assert_eq!(
            secrets.services,
            HashSet::from([
                "github".to_string(),
                "kong-prod-pat".to_string(),
                "anthropic".to_string()
            ])
        );
        let custom = secrets.custom.get("STRIPE_API_KEY").unwrap();
        assert_eq!(
            custom.targets,
            BTreeSet::from(["api.stripe.com".to_string()])
        );
        assert_eq!(custom.placeholder, "sbx-cs-97BPiO11AS93Tlo5");
    }

    #[test]
    fn parse_sbx_secret_ls_splits_comma_separated_targets() {
        // Pinned to a live capture: multiple --host targets render as one
        // comma+space-separated TARGETS field, columns padded to 2+ spaces.
        let output = "\
SCOPE      TYPE      NAME     SECRET
(global)   service   github   (stored)

CUSTOM SECRETS
SCOPE      TARGETS                                                                             ENV                 PLACEHOLDER               SECRET
(global)   probe.invalid, other.probe.invalid, a-quite-long-hostname.subdomain.probe.invalid   COYOTE_TEST_PROBE   sbx-cs-TdaC76ZA3MYAfNAt   probe-*******
";

        let secrets = parse_sbx_secret_ls(output);

        let custom = secrets.custom.get("COYOTE_TEST_PROBE").unwrap();
        assert_eq!(
            custom.targets,
            BTreeSet::from([
                "probe.invalid".to_string(),
                "other.probe.invalid".to_string(),
                "a-quite-long-hostname.subdomain.probe.invalid".to_string()
            ])
        );
        assert_eq!(custom.placeholder, "sbx-cs-TdaC76ZA3MYAfNAt");
    }

    #[test]
    fn parse_sbx_secret_ls_without_custom_section() {
        let output = "\
SCOPE      TYPE      NAME     SECRET
(global)   service   github   (stored)
";

        let secrets = parse_sbx_secret_ls(output);

        assert_eq!(secrets.services, HashSet::from(["github".to_string()]));
        assert!(secrets.custom.is_empty());
    }

    #[test]
    fn parse_sbx_secret_ls_ignores_non_global_rows() {
        let output = "\
SCOPE      TYPE      NAME     SECRET
my-box     service   github   (stored)

CUSTOM SECRETS
SCOPE    TARGETS          ENV       PLACEHOLDER    SECRET
my-box   api.stripe.com   API_KEY   sbx-cs-abc12   sk-***
";

        let secrets = parse_sbx_secret_ls(output);

        assert!(secrets.services.is_empty());
        assert!(secrets.custom.is_empty());
    }

    #[test]
    fn parse_sbx_secret_ls_handles_empty_output() {
        let secrets = parse_sbx_secret_ls("");

        assert!(secrets.services.is_empty());
        assert!(secrets.custom.is_empty());
    }

    fn plan_for(secret_name: &str, hosts: &[&str]) -> CustomSecretPlan {
        CustomSecretPlan {
            secret_name: secret_name.to_string(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            sources: BTreeSet::from(["test".to_string()]),
            strict: true,
        }
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn custom_secret_targets_falls_back_to_wildcard() {
        assert_eq!(
            custom_secret_targets(&plan_for("KEY", &[])),
            vec!["**".to_string()]
        );
        assert_eq!(
            custom_secret_targets(&plan_for("KEY", &["api.example.com"])),
            vec!["api.example.com".to_string()]
        );
    }

    #[test]
    fn plan_action_registers_when_no_secret_exists() {
        assert_eq!(
            plan_custom_secret_action(None, &strs(&["api.example.com"])),
            CustomSecretAction::Register {
                targets: strs(&["api.example.com"])
            }
        );
    }

    #[test]
    fn plan_action_skips_a_covering_registration() {
        let existing = CustomSecret {
            targets: BTreeSet::from(["a.example.com".to_string(), "b.example.com".to_string()]),
            placeholder: "sbx-cs-x".to_string(),
        };

        assert_eq!(
            plan_custom_secret_action(Some(&existing), &strs(&["a.example.com"])),
            CustomSecretAction::Covered
        );
        assert_eq!(
            plan_custom_secret_action(Some(&existing), &strs(&["a.example.com", "b.example.com"])),
            CustomSecretAction::Covered
        );
    }

    #[test]
    fn plan_action_treats_wildcard_as_covering_everything() {
        let existing = CustomSecret {
            targets: BTreeSet::from(["**".to_string()]),
            placeholder: "sbx-cs-x".to_string(),
        };

        assert_eq!(
            plan_custom_secret_action(Some(&existing), &strs(&["any.example.com"])),
            CustomSecretAction::Covered,
            "a wildcard registration is never narrowed, only noted"
        );
    }

    #[test]
    fn plan_action_replaces_on_target_drift_with_the_union() {
        let existing = CustomSecret {
            targets: BTreeSet::from(["a.example.com".to_string()]),
            placeholder: "sbx-cs-x".to_string(),
        };

        assert_eq!(
            plan_custom_secret_action(Some(&existing), &strs(&["b.example.com"])),
            CustomSecretAction::Replace {
                placeholder: "sbx-cs-x".to_string(),
                targets: strs(&["a.example.com", "b.example.com"]),
            },
            "drift removes the old registration (by placeholder) and re-registers \
             with the union so scope widened outside Coyote is never narrowed"
        );
    }

    #[test]
    fn shared_rag_and_mcp_secret_accumulates_one_plan_with_unioned_hosts() {
        let mut plans: BTreeMap<String, CustomSecretPlan> = BTreeMap::new();

        add_custom_secret_plan(
            &mut plans,
            "OPENAI_KEY",
            strs(&["qdrant.example.com"]),
            "RAG 'docs'".to_string(),
            false,
        );
        add_custom_secret_plan(
            &mut plans,
            "OPENAI_KEY",
            strs(&["api.openai.com"]),
            "MCP server(s) 'assistant'".to_string(),
            true,
        );

        assert_eq!(plans.len(), 1, "one env var must yield one registration");
        let plan = &plans["COYOTE_SECRET_OPENAI_KEY"];
        assert_eq!(
            plan.hosts,
            BTreeSet::from([
                "api.openai.com".to_string(),
                "qdrant.example.com".to_string()
            ]),
            "targets from every source are unioned, not last-writer-wins"
        );
        assert_eq!(
            plan.sources,
            BTreeSet::from([
                "MCP server(s) 'assistant'".to_string(),
                "RAG 'docs'".to_string()
            ])
        );
        assert!(
            plan.strict,
            "any strict source upgrades the whole plan to a hard error on a missing secret"
        );
    }

    #[test]
    fn rag_driver_hosts_strips_port_from_bare_host() {
        let data = rag_with(&[("host", "qdrant.example.com:6333"), ("collection", "docs")]);

        assert_eq!(rag_driver_hosts(&data), vec!["qdrant.example.com"]);
    }

    #[test]
    fn rag_driver_hosts_scrapes_urls_and_bare_url_key() {
        let data = rag_with(&[
            ("url", "https://qdrant.example.com:6333"),
            ("proxy", "endpoint http://edge.example.com/v1"),
        ]);

        assert_eq!(
            rag_driver_hosts(&data),
            vec!["edge.example.com", "qdrant.example.com"]
        );
    }

    #[test]
    fn rag_driver_hosts_ignores_placeholders_and_non_host_values() {
        let data = rag_with(&[
            ("host", "{{QDRANT_HOST}}"),
            ("api_key", "{{QDRANT_KEY}}"),
            ("collection", "docs"),
        ]);

        assert!(rag_driver_hosts(&data).is_empty());
    }

    #[test]
    fn bare_host_accepts_host_and_host_port_only() {
        assert_eq!(
            bare_host("qdrant.example.com"),
            Some("qdrant.example.com".to_string())
        );
        assert_eq!(bare_host("localhost:6333"), Some("localhost".to_string()));
        assert_eq!(bare_host("127.0.0.1:6333"), Some("127.0.0.1".to_string()));
        assert_eq!(bare_host("https://a.example.com"), None);
        assert_eq!(bare_host("{{HOST}}"), None);
        assert_eq!(bare_host("host/path"), None);
        assert_eq!(bare_host("two words"), None);
        assert_eq!(bare_host("::1"), None);
        assert_eq!(bare_host("[::1]:6333"), None);
        assert_eq!(bare_host(""), None);
    }

    #[test]
    fn credentials_mixin_hash_distinguishes_content_and_absence() {
        assert_eq!(
            credentials_mixin_hash(Some("kind: mixin\n")),
            credentials_mixin_hash(Some("kind: mixin\n"))
        );
        assert_eq!(
            credentials_mixin_hash(None),
            credentials_mixin_hash(Some(""))
        );
        assert_ne!(
            credentials_mixin_hash(None),
            credentials_mixin_hash(Some("kind: mixin\n"))
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
