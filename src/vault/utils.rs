use crate::config::ensure_parent_exists;
use crate::vault::{SECRET_RE, Vault};
use anyhow::Result;
use anyhow::anyhow;
use gman::SecretError;
use gman::providers::SupportedProvider;
use gman::providers::aws_secrets_manager::AwsSecretsManagerProvider;
use gman::providers::azure_key_vault::AzureKeyVaultProvider;
use gman::providers::gcp_secret_manager::GcpSecretManagerProvider;
use gman::providers::gopass::GopassProvider;
use gman::providers::local::LocalProvider;
use gman::providers::one_password::OnePasswordProvider;
use indoc::formatdoc;
use inquire::validator::Validation;
use inquire::{Confirm, Password, PasswordDisplayMode, Select, Text, min_length, required};
use log::debug;
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

pub fn create_vault_password_file(vault: &mut Vault) -> Result<()> {
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

        let ans = Confirm::new(
            format!(
                "The configured password file '{}' is empty. Create a password?",
                vault_password_file.display()
            )
            .as_str(),
        )
        .with_default(true)
        .prompt()?;

        if !ans {
            return Err(anyhow!(
                "The configured password file '{}' is empty. Please populate it with a password and try again.",
                vault_password_file.display()
            ));
        }

        let password = Password::new("Enter a password to encrypt all vault secrets:")
            .with_validator(required!())
            .with_validator(min_length!(10))
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt();

        match password {
            Ok(pw) => {
                std::fs::write(&vault_password_file, pw.as_bytes())?;
                set_password_file_permissions(&vault_password_file)?;
                println!(
                    "✓ Password file '{}' updated.",
                    vault_password_file.display()
                );
            }
            Err(_) => {
                return Err(anyhow!(
                    "Failed to read password from input. Password file not updated."
                ));
            }
        }
    } else {
        let ans = Confirm::new("No password file configured. Do you want to create one now?")
            .with_default(true)
            .prompt()?;

        if !ans {
            return Err(anyhow!(
                "A password file is required to utilize the Coyote vault. Please configure a password file in your config file and try again."
            ));
        }

        let password_file: PathBuf = Text::new("Enter the path to the password file to create:")
            .with_default(&vault_password_file.display().to_string())
            .with_validator(required!("Password file path is required"))
            .with_validator(|input: &str| {
                let path = PathBuf::from(input);
                if path.exists() {
                    Ok(Validation::Invalid(
                        "File already exists. Please choose a different path.".into(),
                    ))
                } else if let Some(parent) = path.parent() {
                    if !parent.exists() {
                        Ok(Validation::Invalid(
                            "Parent directory does not exist.".into(),
                        ))
                    } else {
                        Ok(Validation::Valid)
                    }
                } else {
                    Ok(Validation::Valid)
                }
            })
            .prompt()?
            .into();

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

        let password = Password::new("Enter a password to encrypt all vault secrets:")
            .with_display_mode(PasswordDisplayMode::Masked)
            .with_validator(required!())
            .with_validator(min_length!(10))
            .prompt();

        match password {
            Ok(pw) => {
                std::fs::write(&password_file, pw.as_bytes())?;
                set_password_file_permissions(&password_file)?;
                local_provider.password_file = Some(password_file);
                println!(
                    "✓ Password file '{}' created.",
                    vault_password_file.display()
                );
            }
            Err(_) => {
                return Err(anyhow!(
                    "Failed to read password from input. Password file not created."
                ));
            }
        }
    }

    Ok(())
}

pub fn prompt_provider_choice() -> Result<Option<SupportedProvider>> {
    let choices = vec![
        "local - encrypted file on this machine",
        "aws_secrets_manager - AWS Secrets Manager",
        "gcp_secret_manager - Google Cloud Secret Manager",
        "azure_key_vault - Azure Key Vault",
        "gopass - gopass password manager (requires the `gopass` CLI)",
        "one_password - 1Password (requires the `op` CLI)",
    ];
    let choice = Select::new("Which secrets provider would you like to use?", choices)
        .with_starting_cursor(0)
        .prompt()?;

    if choice.starts_with("local") {
        return Ok(None);
    }

    let provider = if choice.starts_with("aws_secrets_manager") {
        prompt_aws_provider()?
    } else if choice.starts_with("gcp_secret_manager") {
        prompt_gcp_provider()?
    } else if choice.starts_with("azure_key_vault") {
        prompt_azure_provider()?
    } else if choice.starts_with("gopass") {
        prompt_gopass_provider()?
    } else if choice.starts_with("one_password") {
        prompt_one_password_provider()?
    } else {
        return Err(anyhow!("unexpected provider choice: {choice}"));
    };

    Ok(Some(provider))
}

fn prompt_aws_provider() -> Result<SupportedProvider> {
    let aws_profile = Text::new("AWS profile name:")
        .with_default("default")
        .with_validator(required!())
        .with_help_message("From your ~/.aws/config and ~/.aws/credentials")
        .prompt()?;
    let aws_region = Text::new("AWS region:")
        .with_default("us-east-1")
        .with_validator(required!())
        .with_help_message("Where your secrets live (e.g. us-east-1, eu-west-2)")
        .prompt()?;

    advisory_preflight(
        "AWS",
        "aws",
        &["sts", "get-caller-identity", "--profile", &aws_profile],
    );

    Ok(SupportedProvider::AwsSecretsManager {
        provider_def: AwsSecretsManagerProvider {
            aws_profile: Some(aws_profile),
            aws_region: Some(aws_region),
        },
    })
}

fn prompt_gcp_provider() -> Result<SupportedProvider> {
    let gcp_project_id = Text::new("GCP project ID:")
        .with_validator(required!())
        .with_help_message("The project that hosts your Secret Manager secrets")
        .prompt()?;

    advisory_preflight(
        "GCP",
        "gcloud",
        &["auth", "application-default", "print-access-token"],
    );

    Ok(SupportedProvider::GcpSecretManager {
        provider_def: GcpSecretManagerProvider {
            gcp_project_id: Some(gcp_project_id),
        },
    })
}

fn prompt_azure_provider() -> Result<SupportedProvider> {
    let vault_name = Text::new("Azure Key Vault name:")
        .with_validator(required!())
        .with_help_message("Just the vault name; the https endpoint is auto-derived")
        .prompt()?;

    advisory_preflight("Azure", "az", &["account", "show"]);

    Ok(SupportedProvider::AzureKeyVault {
        provider_def: AzureKeyVaultProvider {
            vault_name: Some(vault_name),
        },
    })
}

fn prompt_gopass_provider() -> Result<SupportedProvider> {
    let store_raw = Text::new("gopass store (leave blank for default):").prompt()?;
    let store = match store_raw.trim() {
        "" => None,
        s => Some(s.to_string()),
    };

    required_cli_preflight("gopass", "gopass", "https://www.gopass.pw/");

    Ok(SupportedProvider::Gopass {
        provider_def: GopassProvider { store },
    })
}

fn prompt_one_password_provider() -> Result<SupportedProvider> {
    let vault_raw = Text::new("1Password vault (leave blank for default):").prompt()?;
    let vault = match vault_raw.trim() {
        "" => None,
        s => Some(s.to_string()),
    };

    let account_raw = Text::new("1Password account (leave blank for default):").prompt()?;
    let account = match account_raw.trim() {
        "" => None,
        s => Some(s.to_string()),
    };

    required_cli_preflight(
        "1Password CLI",
        "op",
        "https://developer.1password.com/docs/cli/",
    );

    Ok(SupportedProvider::OnePassword {
        provider_def: OnePasswordProvider { vault, account },
    })
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
    use anyhow::Error;
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
}
