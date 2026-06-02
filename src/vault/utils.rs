use crate::config::ensure_parent_exists;
use crate::vault::{SECRET_RE, Vault};
use anyhow::Result;
use anyhow::anyhow;
use gman::providers::SupportedProvider;
use gman::providers::local::LocalProvider;
use indoc::formatdoc;
use inquire::validator::Validation;
use inquire::{Confirm, Password, PasswordDisplayMode, Text, min_length, required};
use std::path::PathBuf;
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
