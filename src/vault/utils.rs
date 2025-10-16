use crate::config::ensure_parent_exists;
use crate::vault::{Vault, SECRET_RE};
use anyhow::anyhow;
use anyhow::Result;
use gman::providers::local::LocalProvider;
use indoc::formatdoc;
use inquire::validator::Validation;
use inquire::{min_length, required, Confirm, Password, PasswordDisplayMode, Text};
use std::borrow::Cow;
use std::path::PathBuf;

pub fn ensure_password_file_initialized(local_provider: &mut LocalProvider) -> Result<()> {
	let vault_password_file = local_provider
		.password_file
		.clone()
		.ok_or_else(|| anyhow!("Password file is not configured"))?;

	if vault_password_file.exists() {
		{
			let file_contents = std::fs::read_to_string(&vault_password_file)?;
			if !file_contents.trim().is_empty() {
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
			return Err(anyhow!("The configured password file '{}' is empty. Please populate it with a password and try again.", vault_password_file.display()));
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
			return Err(anyhow!("A password file is required to utilize the Loki vault. Please configure a password file in your config file and try again."));
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
			println!(
				"{}",
				formatdoc!(
                    "
										Note: The default password file path is '{}'.
										You have chosen to create a different path: '{}'.
										Please ensure your configuration is updated accordingly.
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

pub fn interpolate_secrets<'a>(content: &'a str, vault: &Vault) -> (Cow<'a, str>, Vec<String>) {
	let mut missing_secrets = vec![];
	let parsed_content = SECRET_RE.replace_all(content, |caps: &fancy_regex::Captures<'_>| {
		let secret = vault.get_secret(caps[1].trim(), false);
		match secret {
			Ok(s) => s,
			Err(_) => {
				missing_secrets.push(caps[1].to_string());
				"".to_string()
			}
		}
	});

	(parsed_content, missing_secrets)
}
