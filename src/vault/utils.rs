use crate::config::ensure_parent_exists;
use crate::vault::{SECRET_RE, Vault};
use anyhow::Result;
use anyhow::anyhow;
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
use std::path::PathBuf;
use std::process::Command;
use gman::SecretError;

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
            eprintln!(
                "    Setup will continue. Fix authentication before using --add-secret etc."
            );
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
                    let name = caps[1].trim();
                    match vault.get_secret(name, false) {
                        Ok(s) => s,
                        Err(e) => match e.downcast_ref::<SecretError>() {
                            Some(SecretError::NotFound { .. }) => {
                                missing_secrets.push(name.to_string());
                                String::new()
                            }
                            Some(SecretError::AuthFailed { .. }) => {
                                let base = format!(
                                    "Failed to fetch secret '{name}' from vault: {e}"
                                );
                                let msg = match vault.auth_hint() {
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
