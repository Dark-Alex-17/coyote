use crate::config::ensure_parent_exists;
use crate::events::Origin;
use crate::prompt::{
    PromptAction, PromptAnswer, PromptRequest, Prompter, ask, confirm, input, reask, select_closed,
};
use crate::sandbox::{SANDBOX_ENV_FLAG, sandbox_secret_env_var};
use crate::vault::{SECRET_RE, Vault};
use anyhow::{Context, Result, anyhow};
use gman::SecretError;
use gman::providers::SupportedProvider;
use gman::providers::aws_secrets_manager::AwsSecretsManagerProvider;
use gman::providers::azure_key_vault::AzureKeyVaultProvider;
use gman::providers::gcp_secret_manager::GcpSecretManagerProvider;
use gman::providers::gopass::GopassProvider;
use gman::providers::local::LocalProvider;
use gman::providers::one_password::OnePasswordProvider;
use indoc::formatdoc;
use log::debug;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn ensure_password_file_initialized(local_provider: &mut LocalProvider) -> Result<()> {
    let vault_password_file = local_provider
        .password_file
        .clone()
        .ok_or_else(|| anyhow!("Password file is not configured"))?;

    if vault_password_file.exists() {
        {
            let file_contents = std::fs::read_to_string(&vault_password_file)?;
            if !file_contents.trim().is_empty() {
                Ok(())
            } else {
                Err(anyhow!(
                    "The configured password file '{}' is empty. Please populate it with a password and try again.",
                    vault_password_file.display()
                ))
            }
        }
    } else {
        Err(anyhow!(
            "A password file is required to utilize the Coyote vault. Please configure a password file in your config file and try again."
        ))
    }
}

/// Asks `label` until the answer is non-empty; an empty one is asked
/// again with inquire's own wording in front of the label.
fn required_input(
    prompter: &dyn Prompter,
    label: &str,
    default: Option<&str>,
    help: Option<&str>,
) -> Result<String> {
    let mut question = label.to_string();
    loop {
        let answer = input(prompter, &question, default, help)?;
        if answer.is_empty() {
            question = reask("A response is required.", label);
            continue;
        }
        return Ok(answer);
    }
}

/// The password that encrypts the vault, asked until it is at least ten
/// characters; each shortfall is asked again with the reason as a
/// sentence in front of the label.
fn ask_vault_password(prompter: &dyn Prompter) -> Result<String> {
    let label = "Enter a password to encrypt all vault secrets:";
    let mut question = label.to_string();
    loop {
        let req = PromptRequest::new(PromptAction::Password, &question, Origin::detached());
        let password = ask(prompter, req).and_then(PromptAnswer::into_text)?;
        if password.is_empty() {
            question = reask("A response is required.", label);
            continue;
        }
        if password.chars().count() < 10 {
            question = reask("The length of the response should be at least 10.", label);
            continue;
        }
        return Ok(password);
    }
}

/// Where to create the password file, asked until the answer names a
/// file that does not exist in a directory that does.
fn ask_password_file_path(prompter: &dyn Prompter, default: &Path) -> Result<PathBuf> {
    let default = default.display().to_string();
    let label = "Enter the path to the password file to create:";
    let mut question = label.to_string();
    loop {
        let answer = input(prompter, &question, Some(&default), None)?;
        if answer.is_empty() {
            question = reask("Password file path is required.", label);
            continue;
        }
        let path = PathBuf::from(answer);
        if path.exists() {
            question = reask(
                "File already exists. Please choose a different path.",
                label,
            );
            continue;
        }
        if path.parent().is_some_and(|parent| !parent.exists()) {
            question = reask("Parent directory does not exist.", label);
            continue;
        }
        return Ok(path);
    }
}

pub fn create_vault_password_file(vault: &mut Vault, prompter: &dyn Prompter) -> Result<()> {
    let SupportedProvider::Local {
        provider_def: local_provider,
    } = &mut vault.provider
    else {
        return Ok(());
    };

    let vault_password_file = local_provider
        .password_file
        .clone()
        .ok_or_else(|| anyhow!("Password file is not configured"))?;

    if vault_password_file.exists() {
        {
            let file_contents = std::fs::read_to_string(&vault_password_file)?;
            if !file_contents.trim().is_empty() {
                debug!(
                    "create_vault_password_file was called but the password file already exists and is non-empty"
                );
                return Ok(());
            }
        }

        let ans = confirm(
            prompter,
            format!(
                "The configured password file '{}' is empty. Create a password?",
                vault_password_file.display()
            ),
            true,
        )?;

        if !ans {
            return Err(anyhow!(
                "The configured password file '{}' is empty. Please populate it with a password and try again.",
                vault_password_file.display()
            ));
        }

        let password = ask_vault_password(prompter)
            .context("Failed to read password from input. Password file not updated.")?;
        std::fs::write(&vault_password_file, password.as_bytes())?;
        set_password_file_permissions(&vault_password_file)?;
        println!(
            "✓ Password file '{}' updated.",
            vault_password_file.display()
        );
    } else {
        let ans = confirm(
            prompter,
            "No password file configured. Do you want to create one now?".to_string(),
            true,
        )?;

        if !ans {
            return Err(anyhow!(
                "A password file is required to utilize the Coyote vault. Please configure a password file in your config file and try again."
            ));
        }

        let password_file = ask_password_file_path(prompter, &vault_password_file)?;

        if password_file != vault_password_file {
            debug!(
                "{}",
                formatdoc!(
                    "
										The default password file path is '{}'.
										User chose to create file at a different path: '{}'.
										",
                    vault_password_file.display(),
                    password_file.display()
                )
            );
        }

        ensure_parent_exists(&password_file)?;

        let password = ask_vault_password(prompter)
            .context("Failed to read password from input. Password file not created.")?;
        std::fs::write(&password_file, password.as_bytes())?;
        set_password_file_permissions(&password_file)?;
        println!("✓ Password file '{}' created.", password_file.display());
        local_provider.password_file = Some(password_file);
    }

    Ok(())
}

pub fn prompt_provider_choice(prompter: &dyn Prompter) -> Result<Option<SupportedProvider>> {
    let options = [
        "local - encrypted file on this machine",
        "aws_secrets_manager - AWS Secrets Manager",
        "gcp_secret_manager - Google Cloud Secret Manager",
        "azure_key_vault - Azure Key Vault",
        "gopass - gopass password manager (requires the `gopass` CLI)",
        "one_password - 1Password (requires the `op` CLI)",
    ]
    .map(str::to_string)
    .to_vec();
    let choice = select_closed(
        prompter,
        "Which secrets provider would you like to use?",
        options,
    )?;

    if choice.starts_with("local") {
        return Ok(None);
    }

    let provider = if choice.starts_with("aws_secrets_manager") {
        prompt_aws_provider(prompter)?
    } else if choice.starts_with("gcp_secret_manager") {
        prompt_gcp_provider(prompter)?
    } else if choice.starts_with("azure_key_vault") {
        prompt_azure_provider(prompter)?
    } else if choice.starts_with("gopass") {
        prompt_gopass_provider(prompter)?
    } else if choice.starts_with("one_password") {
        prompt_one_password_provider(prompter)?
    } else {
        unreachable!("select_closed only hands back an offered option");
    };
    preflight(&provider);

    Ok(Some(provider))
}

fn prompt_aws_provider(prompter: &dyn Prompter) -> Result<SupportedProvider> {
    let aws_profile = required_input(
        prompter,
        "AWS profile name:",
        Some("default"),
        Some("From your ~/.aws/config and ~/.aws/credentials"),
    )?;
    let aws_region = required_input(
        prompter,
        "AWS region:",
        Some("us-east-1"),
        Some("Where your secrets live (e.g. us-east-1, eu-west-2)"),
    )?;

    Ok(SupportedProvider::AwsSecretsManager {
        provider_def: AwsSecretsManagerProvider {
            aws_profile: Some(aws_profile),
            aws_region: Some(aws_region),
        },
    })
}

fn prompt_gcp_provider(prompter: &dyn Prompter) -> Result<SupportedProvider> {
    let gcp_project_id = required_input(
        prompter,
        "GCP project ID:",
        None,
        Some("The project that hosts your Secret Manager secrets"),
    )?;

    Ok(SupportedProvider::GcpSecretManager {
        provider_def: GcpSecretManagerProvider {
            gcp_project_id: Some(gcp_project_id),
        },
    })
}

fn prompt_azure_provider(prompter: &dyn Prompter) -> Result<SupportedProvider> {
    let vault_name = required_input(
        prompter,
        "Azure Key Vault name:",
        None,
        Some("Just the vault name; the https endpoint is auto-derived"),
    )?;

    Ok(SupportedProvider::AzureKeyVault {
        provider_def: AzureKeyVaultProvider {
            vault_name: Some(vault_name),
        },
    })
}

fn prompt_gopass_provider(prompter: &dyn Prompter) -> Result<SupportedProvider> {
    let store_raw = input(
        prompter,
        "gopass store (leave blank for default):",
        None,
        None,
    )?;
    let store = match store_raw.trim() {
        "" => None,
        s => Some(s.to_string()),
    };

    Ok(SupportedProvider::Gopass {
        provider_def: GopassProvider { store },
    })
}

fn prompt_one_password_provider(prompter: &dyn Prompter) -> Result<SupportedProvider> {
    let vault_raw = input(
        prompter,
        "1Password vault (leave blank for default):",
        None,
        None,
    )?;
    let vault = match vault_raw.trim() {
        "" => None,
        s => Some(s.to_string()),
    };

    let account_raw = input(
        prompter,
        "1Password account (leave blank for default):",
        None,
        None,
    )?;
    let account = match account_raw.trim() {
        "" => None,
        s => Some(s.to_string()),
    };

    Ok(SupportedProvider::OnePassword {
        provider_def: OnePasswordProvider { vault, account },
    })
}

/// Checks the chosen backend's CLI once the answers are in: advisory for
/// the clouds, whose SDKs have other credential paths, required for the
/// managers that are nothing without theirs.
fn preflight(provider: &SupportedProvider) {
    match provider {
        SupportedProvider::AwsSecretsManager { provider_def } => advisory_preflight(
            "AWS",
            "aws",
            &[
                "sts",
                "get-caller-identity",
                "--profile",
                provider_def.aws_profile.as_deref().unwrap_or("default"),
            ],
        ),
        SupportedProvider::GcpSecretManager { .. } => advisory_preflight(
            "GCP",
            "gcloud",
            &["auth", "application-default", "print-access-token"],
        ),
        SupportedProvider::AzureKeyVault { .. } => {
            advisory_preflight("Azure", "az", &["account", "show"]);
        }
        SupportedProvider::Gopass { .. } => {
            required_cli_preflight("gopass", "gopass", "https://www.gopass.pw/");
        }
        SupportedProvider::OnePassword { .. } => required_cli_preflight(
            "1Password CLI",
            "op",
            "https://developer.1password.com/docs/cli/",
        ),
        SupportedProvider::Local { .. } => {}
    }
}

fn advisory_preflight(label: &str, cli: &str, args: &[&str]) {
    match Command::new(cli).args(args).output() {
        Ok(out) if out.status.success() => {
            println!("✓ {label} authentication check succeeded.");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprintln!("⚠️  {label} preflight returned non-zero:");
            if !stderr.trim().is_empty() {
                eprintln!("    {}", stderr.trim());
            }
            eprintln!("    Setup will continue. Fix authentication before using --add-secret etc.");
        }
        Err(_) => {
            eprintln!(
                "⚠️  `{cli}` CLI not found on PATH. Coyote will still try the {label} SDK directly via standard credentials (env vars, instance metadata, service-account JSON, etc.)."
            );
        }
    }
}

fn required_cli_preflight(label: &str, cli: &str, install_url: &str) {
    match Command::new(cli).arg("--version").output() {
        Ok(out) if out.status.success() => {
            println!("✓ {label} is installed and reachable.");
        }
        Ok(_) => {
            eprintln!(
                "⚠️  `{cli} --version` returned non-zero. Your {label} install may be broken — verify before using the vault."
            );
        }
        Err(_) => {
            eprintln!("⚠️  `{cli}` not found on PATH.");
            eprintln!(
                "    The {label} secrets provider requires it. Install from {install_url} before running --add-secret etc."
            );
        }
    }
}

pub fn interpolate_secrets(content: &str, vault: &Vault) -> Result<(String, Vec<String>)> {
    if env::var_os(SANDBOX_ENV_FLAG).is_some() {
        let (parsed, missing) = interpolate_secrets_with(content, None, |name| {
            env::var(sandbox_secret_env_var(name)).map_err(|_| {
                anyhow!(SecretError::NotFound {
                    key: name.to_string(),
                    provider: "sandbox environment",
                })
            })
        })?;

        if !missing.is_empty() {
            let mut env_vars: Vec<String> = missing
                .iter()
                .map(|name| sandbox_secret_env_var(name))
                .collect();
            env_vars.sort();
            env_vars.dedup();
            eprintln!(
                "Config references secrets that are not available inside this sandbox \
                 (expected env vars: {}). Sandbox secrets are provisioned at creation \
                 from the host; add the missing secrets on the host, then re-create \
                 the sandbox.",
                env_vars.join(", ")
            );
        }

        return Ok((parsed, missing));
    }
    interpolate_secrets_with(content, vault.auth_hint(), |name| {
        vault.get_secret(name, false)
    })
}

fn interpolate_secrets_with<F>(
    content: &str,
    auth_hint: Option<&'static str>,
    mut get_secret: F,
) -> Result<(String, Vec<String>)>
where
    F: FnMut(&str) -> Result<String>,
{
    let mut missing_secrets = vec![];
    let mut fatal_error: Option<anyhow::Error> = None;

    let parsed_content: String = content
        .lines()
        .map(|line| {
            if line.trim_start().starts_with('#') || fatal_error.is_some() {
                return line.to_string();
            }

            SECRET_RE
                .replace_all(line, |caps: &fancy_regex::Captures<'_>| {
                    if fatal_error.is_some() {
                        return String::new();
                    }

                    let name = caps[1].trim();
                    match get_secret(name) {
                        Ok(s) => s,
                        Err(e) => match e.downcast_ref::<SecretError>() {
                            Some(SecretError::NotFound { .. }) => {
                                missing_secrets.push(name.to_string());
                                String::new()
                            }
                            Some(SecretError::AuthFailed { .. }) => {
                                let base =
                                    format!("Failed to fetch secret '{name}' from vault: {e}");
                                let msg = match auth_hint {
                                    Some(hint) => format!("{base}\n\nHint: {hint}"),
                                    None => base,
                                };
                                fatal_error = Some(anyhow!("{msg}"));
                                String::new()
                            }
                            _ => {
                                fatal_error = Some(anyhow!(
                                    "Failed to fetch secret '{name}' from vault: {e}"
                                ));
                                String::new()
                            }
                        },
                    }
                })
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(err) = fatal_error {
        return Err(err);
    }

    Ok((parsed_content, missing_secrets))
}

#[cfg(unix)]
fn set_password_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
        anyhow!(
            "Failed to set 0600 permissions on '{}': {e}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn set_password_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::{HeadlessPrompter, NeedsHuman, ScriptedPrompter};
    use crate::testing::AtomicFlagGuard;
    use anyhow::Error;
    use serde_json::json;
    use serial_test::serial;
    use std::cell::RefCell;

    fn not_found(name: &str) -> Error {
        Error::new(SecretError::NotFound {
            key: name.to_string(),
            provider: "test",
        })
    }

    fn auth_failed() -> Error {
        Error::new(SecretError::AuthFailed {
            provider: "test",
            source: anyhow!("auth failure"),
        })
    }

    struct Calls(RefCell<Vec<String>>);

    impl Calls {
        fn new() -> Self {
            Self(RefCell::new(Vec::new()))
        }

        fn record(&self, name: &str) {
            self.0.borrow_mut().push(name.to_string());
        }

        fn snapshot(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    #[test]
    fn interpolates_single_secret_per_line() {
        let (out, missing) =
            interpolate_secrets_with("api_key={{API_KEY}}", None, |name| match name {
                "API_KEY" => Ok("sk-12345".to_string()),
                other => panic!("unexpected lookup: {other}"),
            })
            .unwrap();

        assert_eq!(out, "api_key=sk-12345");
        assert!(missing.is_empty());
    }

    #[test]
    fn regex_matches_each_secret_independently_when_one_per_line() {
        let calls = Calls::new();
        let (out, missing) = interpolate_secrets_with("{{ONE}}\nmiddle\n{{TWO}}", None, |name| {
            calls.record(name);
            Ok(name.to_lowercase())
        })
        .unwrap();

        assert_eq!(calls.snapshot(), vec!["ONE".to_string(), "TWO".to_string()]);
        assert_eq!(out, "one\nmiddle\ntwo");
        assert!(missing.is_empty());
    }

    #[test]
    fn skips_comment_lines() {
        let calls = Calls::new();

        let (out, missing) =
            interpolate_secrets_with("# api_key={{NEVER_FETCHED}}\nreal={{S}}", None, |name| {
                calls.record(name);
                Ok("v".to_string())
            })
            .unwrap();

        assert_eq!(out, "# api_key={{NEVER_FETCHED}}\nreal=v");
        assert!(missing.is_empty());
        assert_eq!(calls.snapshot(), vec!["S".to_string()]);
    }

    #[test]
    fn missing_secrets_become_empty_strings_and_are_reported() {
        let (out, missing) = interpolate_secrets_with(
            "a={{HAVE}}\nb={{MISSING_1}}\nc={{MISSING_2}}",
            None,
            |name| match name {
                "HAVE" => Ok("present".to_string()),
                missing => Err(not_found(missing)),
            },
        )
        .unwrap();

        assert_eq!(out, "a=present\nb=\nc=");
        assert_eq!(
            missing,
            vec!["MISSING_1".to_string(), "MISSING_2".to_string()]
        );
    }

    #[test]
    fn interpolates_multiple_secrets_on_same_line() {
        let calls = Calls::new();

        let (out, missing) = interpolate_secrets_with("url={{URL}} key={{KEY}}", None, |name| {
            calls.record(name);
            match name {
                "URL" => Ok("https://example.test".to_string()),
                "KEY" => Ok("sk-12345".to_string()),
                other => panic!("unexpected lookup: {other}"),
            }
        })
        .unwrap();

        assert_eq!(calls.snapshot(), vec!["URL".to_string(), "KEY".to_string()]);
        assert_eq!(out, "url=https://example.test key=sk-12345");
        assert!(missing.is_empty());
    }

    #[test]
    fn regex_rejects_braces_in_secret_names() {
        let calls = Calls::new();

        let (out, missing) =
            interpolate_secrets_with("literal {{ {NOT_A_NAME} }} text", None, |name| {
                calls.record(name);
                Ok(format!("got-{name}"))
            })
            .unwrap();

        assert!(
            calls.snapshot().is_empty(),
            "name with embedded braces must not match"
        );
        assert_eq!(out, "literal {{ {NOT_A_NAME} }} text");
        assert!(missing.is_empty());
    }

    #[test]
    fn fatal_failure_short_circuits_remaining_lines() {
        let calls = Calls::new();

        let result =
            interpolate_secrets_with("a={{S1}}\nb={{S2}}\nc={{S3}}\nd={{S4}}", None, |name| {
                calls.record(name);
                match name {
                    "S1" => Ok("first".to_string()),
                    "S2" => Err(auth_failed()),
                    other => Ok(format!("late-{other}")),
                }
            });

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("S2"),
            "error should name the offending secret, got: {err}"
        );
        assert_eq!(
            calls.snapshot(),
            vec!["S1".to_string(), "S2".to_string()],
            "lookups must stop at the failing secret - S3 and S4 should never be fetched"
        );
    }

    #[test]
    fn auth_failure_appends_hint_when_provided() {
        let result = interpolate_secrets_with(
            "k={{K}}",
            Some("run `coyote --authenticate` to reauth"),
            |_| Err(auth_failed()),
        );

        let err = result.unwrap_err().to_string();

        assert!(err.contains("Hint:"), "expected hint in error, got: {err}");
        assert!(
            err.contains("coyote --authenticate"),
            "expected hint contents, got: {err}"
        );
    }

    fn text(value: &str) -> PromptAnswer {
        PromptAnswer::Text(value.to_string())
    }

    fn questions(prompter: &ScriptedPrompter) -> Vec<String> {
        prompter
            .requests()
            .into_iter()
            .map(|req| req.question)
            .collect()
    }

    /// A fresh temp dir that is removed with the test.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = env::temp_dir().join(format!("coyote-vault-password-{label}-{unique}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn local_vault(&self, password_file: &str) -> Vault {
            Vault::from_provider(SupportedProvider::Local {
                provider_def: LocalProvider {
                    password_file: Some(self.0.join(password_file)),
                    git_branch: None,
                    ..LocalProvider::default()
                },
            })
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The provider wizard asks through the prompter, so under the
    /// full-screen UI its choice is a closed select and its fields are
    /// inputs carrying the defaults and help the inquire widgets showed;
    /// the answers land in the provider it returns.
    #[test]
    #[serial]
    fn provider_prompts_under_the_tui_ask_through_the_prompter() {
        let _tui_on = AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);

        let choice = ScriptedPrompter::new(vec![PromptAnswer::Selected(
            "local - encrypted file on this machine".to_string(),
        )]);
        assert!(prompt_provider_choice(&choice).unwrap().is_none());
        let [req] = choice.requests().try_into().unwrap();
        assert_eq!(req.action, PromptAction::Select);
        assert_eq!(
            req.question,
            "Which secrets provider would you like to use?"
        );
        assert!(req.closed);
        assert_eq!(req.options.len(), 6);
        assert_eq!(req.options[0], "local - encrypted file on this machine");
        assert_eq!(
            req.options[5],
            "one_password - 1Password (requires the `op` CLI)"
        );

        let aws = ScriptedPrompter::new(vec![text("work"), text("eu-west-2")]);
        let SupportedProvider::AwsSecretsManager { provider_def } =
            prompt_aws_provider(&aws).unwrap()
        else {
            panic!("not an AWS provider");
        };
        assert_eq!(provider_def.aws_profile.as_deref(), Some("work"));
        assert_eq!(provider_def.aws_region.as_deref(), Some("eu-west-2"));
        let [profile, region] = aws.requests().try_into().unwrap();
        assert_eq!(profile.action, PromptAction::Input);
        assert_eq!(profile.question, "AWS profile name:");
        assert_eq!(profile.default, Some(json!("default")));
        assert_eq!(
            profile.help.as_deref(),
            Some("From your ~/.aws/config and ~/.aws/credentials")
        );
        assert_eq!(region.action, PromptAction::Input);
        assert_eq!(region.question, "AWS region:");
        assert_eq!(region.default, Some(json!("us-east-1")));
        assert_eq!(
            region.help.as_deref(),
            Some("Where your secrets live (e.g. us-east-1, eu-west-2)")
        );

        let gcp = ScriptedPrompter::new(vec![text("my-project")]);
        let SupportedProvider::GcpSecretManager { provider_def } =
            prompt_gcp_provider(&gcp).unwrap()
        else {
            panic!("not a GCP provider");
        };
        assert_eq!(provider_def.gcp_project_id.as_deref(), Some("my-project"));
        let [project] = gcp.requests().try_into().unwrap();
        assert_eq!(project.action, PromptAction::Input);
        assert_eq!(project.question, "GCP project ID:");
        assert_eq!(project.default, None);
        assert_eq!(
            project.help.as_deref(),
            Some("The project that hosts your Secret Manager secrets")
        );

        let azure = ScriptedPrompter::new(vec![text("kv-prod")]);
        let SupportedProvider::AzureKeyVault { provider_def } =
            prompt_azure_provider(&azure).unwrap()
        else {
            panic!("not an Azure provider");
        };
        assert_eq!(provider_def.vault_name.as_deref(), Some("kv-prod"));
        let [name] = azure.requests().try_into().unwrap();
        assert_eq!(name.action, PromptAction::Input);
        assert_eq!(name.question, "Azure Key Vault name:");
        assert_eq!(
            name.help.as_deref(),
            Some("Just the vault name; the https endpoint is auto-derived")
        );

        let gopass = ScriptedPrompter::new(vec![text("  ")]);
        let SupportedProvider::Gopass { provider_def } = prompt_gopass_provider(&gopass).unwrap()
        else {
            panic!("not a gopass provider");
        };
        assert_eq!(provider_def.store, None);
        let [store] = gopass.requests().try_into().unwrap();
        assert_eq!(store.action, PromptAction::Input);
        assert_eq!(store.question, "gopass store (leave blank for default):");
        assert_eq!(store.default, None);
        assert_eq!(store.help, None);

        let one_password = ScriptedPrompter::new(vec![text("Private"), text("")]);
        let SupportedProvider::OnePassword { provider_def } =
            prompt_one_password_provider(&one_password).unwrap()
        else {
            panic!("not a 1Password provider");
        };
        assert_eq!(provider_def.vault.as_deref(), Some("Private"));
        assert_eq!(provider_def.account, None);
        assert_eq!(
            questions(&one_password),
            vec![
                "1Password vault (leave blank for default):",
                "1Password account (leave blank for default):"
            ]
        );
    }

    /// A required field left empty is asked again with inquire's own
    /// wording in front of its label; the first non-empty answer lands.
    #[test]
    #[serial]
    fn an_empty_required_field_is_asked_again() {
        let prompter = ScriptedPrompter::new(vec![text(""), text("proj")]);
        let SupportedProvider::GcpSecretManager { provider_def } =
            prompt_gcp_provider(&prompter).unwrap()
        else {
            panic!("not a GCP provider");
        };
        assert_eq!(provider_def.gcp_project_id.as_deref(), Some("proj"));
        assert_eq!(
            questions(&prompter),
            vec!["GCP project ID:", "A response is required. GCP project ID:"]
        );
    }

    /// A choice outside the offered providers is rejected with the shared
    /// closed-select message, not a crash.
    #[test]
    #[serial]
    fn an_unknown_provider_choice_is_an_error() {
        let prompter =
            ScriptedPrompter::new(vec![PromptAnswer::Selected("vaultwarden".to_string())]);
        let err = prompt_provider_choice(&prompter).unwrap_err();
        assert_eq!(
            err.to_string(),
            "'vaultwarden' is not one of the offered options"
        );
    }

    /// A missing password file is created where the user says, with the
    /// configured path offered as the default and the password the
    /// user typed; the provider is pointed at the new file.
    #[test]
    #[serial]
    fn a_missing_password_file_is_created_through_the_prompter() {
        let _tui_on = AtomicFlagGuard::set(&crate::tui::TUI_ACTIVE, true);
        let dir = TempDir::new("create");
        let mut vault = dir.local_vault("configured");
        let chosen = dir.0.join("chosen");
        let prompter = ScriptedPrompter::new(vec![
            PromptAnswer::Confirmed(true),
            text(&chosen.display().to_string()),
            text("correct horse battery"),
        ]);

        create_vault_password_file(&mut vault, &prompter).unwrap();

        assert_eq!(
            std::fs::read_to_string(&chosen).unwrap(),
            "correct horse battery"
        );
        assert_eq!(vault.local_password_file().unwrap(), chosen);
        let [ask_create, ask_path, ask_password] = prompter.requests().try_into().unwrap();
        assert_eq!(ask_create.action, PromptAction::Confirm);
        assert_eq!(
            ask_create.question,
            "No password file configured. Do you want to create one now?"
        );
        assert_eq!(ask_create.default, Some(json!(true)));
        assert_eq!(ask_path.action, PromptAction::Input);
        assert_eq!(
            ask_path.question,
            "Enter the path to the password file to create:"
        );
        assert_eq!(
            ask_path.default,
            Some(json!(dir.0.join("configured").display().to_string()))
        );
        assert_eq!(ask_password.action, PromptAction::Password);
        assert_eq!(
            ask_password.question,
            "Enter a password to encrypt all vault secrets:"
        );
    }

    /// An empty configured password file is filled through the
    /// prompter after a confirm that defaults to yes.
    #[test]
    #[serial]
    fn an_empty_password_file_is_filled_through_the_prompter() {
        let dir = TempDir::new("fill");
        let configured = dir.0.join("configured");
        std::fs::write(&configured, "  \n").unwrap();
        let mut vault = dir.local_vault("configured");
        let prompter = ScriptedPrompter::new(vec![
            PromptAnswer::Confirmed(true),
            text("correct horse battery"),
        ]);

        create_vault_password_file(&mut vault, &prompter).unwrap();

        assert_eq!(
            std::fs::read_to_string(&configured).unwrap(),
            "correct horse battery"
        );
        let [ask_create, ask_password] = prompter.requests().try_into().unwrap();
        assert_eq!(ask_create.action, PromptAction::Confirm);
        assert_eq!(
            ask_create.question,
            format!(
                "The configured password file '{}' is empty. Create a password?",
                configured.display()
            )
        );
        assert_eq!(ask_create.default, Some(json!(true)));
        assert_eq!(ask_password.action, PromptAction::Password);
    }

    /// Each path the inquire validators rejected is asked again with the
    /// validator's own wording in front of the label, and so is a
    /// password that is empty or shorter than ten characters.
    #[test]
    #[serial]
    fn a_rejected_path_or_short_password_is_asked_again() {
        let dir = TempDir::new("re-ask");
        let taken = dir.0.join("taken");
        std::fs::write(&taken, "x").unwrap();
        let chosen = dir.0.join("chosen");
        let orphan = dir.0.join("missing-parent").join("file");
        let mut vault = dir.local_vault("configured");
        let prompter = ScriptedPrompter::new(vec![
            PromptAnswer::Confirmed(true),
            text(""),
            text(&taken.display().to_string()),
            text(&orphan.display().to_string()),
            text(&chosen.display().to_string()),
            text(""),
            text("short"),
            text("long enough"),
        ]);

        create_vault_password_file(&mut vault, &prompter).unwrap();

        assert_eq!(std::fs::read_to_string(&chosen).unwrap(), "long enough");
        assert_eq!(
            questions(&prompter)[1..],
            [
                "Enter the path to the password file to create:",
                "Password file path is required. Enter the path to the password file to create:",
                "File already exists. Please choose a different path. Enter the path to the password file to create:",
                "Parent directory does not exist. Enter the path to the password file to create:",
                "Enter a password to encrypt all vault secrets:",
                "A response is required. Enter a password to encrypt all vault secrets:",
                "The length of the response should be at least 10. Enter a password to encrypt all vault secrets:",
            ]
        );
    }

    /// Declining leaves the file alone with the same errors as before; a
    /// cancelled password keeps the wording the inquire failure had.
    #[test]
    #[serial]
    fn declining_or_cancelling_writes_no_password_file() {
        let dir = TempDir::new("decline");
        let configured = dir.0.join("configured");

        let mut vault = dir.local_vault("configured");
        let declined = ScriptedPrompter::new(vec![PromptAnswer::Confirmed(false)]);
        let err = create_vault_password_file(&mut vault, &declined).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("A password file is required to utilize the Coyote vault."),
            "{err}"
        );

        let mut vault = dir.local_vault("configured");
        let cancelled = ScriptedPrompter::new(vec![
            PromptAnswer::Confirmed(true),
            text(&configured.display().to_string()),
            PromptAnswer::Cancelled,
        ]);
        let err = create_vault_password_file(&mut vault, &cancelled).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Failed to read password from input. Password file not created."
        );
        assert!(!configured.exists());

        std::fs::write(&configured, "").unwrap();
        let mut vault = dir.local_vault("configured");
        let cancelled =
            ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true), PromptAnswer::Cancelled]);
        let err = create_vault_password_file(&mut vault, &cancelled).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Failed to read password from input. Password file not updated."
        );
        assert_eq!(std::fs::read_to_string(&configured).unwrap(), "");
    }

    /// With no human to answer, every prompt of the wizard is refused
    /// and nothing is written or chosen.
    #[test]
    #[serial]
    fn provider_and_password_prompts_without_a_human_are_refused() {
        let dir = TempDir::new("headless");
        let mut vault = dir.local_vault("configured");
        let err = create_vault_password_file(&mut vault, &HeadlessPrompter).unwrap_err();
        assert!(NeedsHuman::caused(&err), "{err:#}");
        assert!(!dir.0.join("configured").exists());
        assert_eq!(
            vault.local_password_file().unwrap(),
            dir.0.join("configured")
        );

        let errors = [
            prompt_provider_choice(&HeadlessPrompter)
                .map(|_| ())
                .unwrap_err(),
            prompt_aws_provider(&HeadlessPrompter)
                .map(|_| ())
                .unwrap_err(),
            prompt_gcp_provider(&HeadlessPrompter)
                .map(|_| ())
                .unwrap_err(),
            prompt_azure_provider(&HeadlessPrompter)
                .map(|_| ())
                .unwrap_err(),
            prompt_gopass_provider(&HeadlessPrompter)
                .map(|_| ())
                .unwrap_err(),
            prompt_one_password_provider(&HeadlessPrompter)
                .map(|_| ())
                .unwrap_err(),
        ];
        for err in errors {
            assert!(NeedsHuman::caused(&err), "{err:#}");
        }
    }
}
