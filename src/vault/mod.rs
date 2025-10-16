mod utils;

pub use utils::interpolate_secrets;

use crate::cli::Cli;
use crate::config::Config;
use crate::vault::utils::ensure_password_file_initialized;
use anyhow::{Context, Result};
use fancy_regex::Regex;
use gman::providers::local::LocalProvider;
use gman::providers::SecretProvider;
use inquire::{required, Password, PasswordDisplayMode};
use std::sync::LazyLock;
use tokio::runtime::Handle;

static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{(.+)}}").unwrap());

#[derive(Debug, Default, Clone)]
pub struct Vault {
    local_provider: LocalProvider,
}

impl Vault {
    pub fn init(config: &Config) -> Self {
        let vault_password_file = config.vault_password_file();
        let mut local_provider = LocalProvider {
            password_file: Some(vault_password_file),
            git_branch: None,
            ..LocalProvider::default()
        };

        ensure_password_file_initialized(&mut local_provider)
            .expect("Failed to initialize password file");

        Self { local_provider }
    }

    pub fn add_secret(&self, secret_name: &str) -> Result<()> {
        let secret_value = Password::new("Enter the secret value:")
            .with_validator(required!())
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt()
            .with_context(|| "unable to read secret from input")?;

        let h = Handle::current();
        tokio::task::block_in_place(|| {
            h.block_on(self.local_provider.set_secret(secret_name, &secret_value))
        })?;
        println!("✓ Secret '{secret_name}' added to the vault.");

        Ok(())
    }

    pub fn get_secret(&self, secret_name: &str, display_output: bool) -> Result<String> {
        let h = Handle::current();
        let secret = tokio::task::block_in_place(|| {
            h.block_on(self.local_provider.get_secret(secret_name))
        })?;

        if display_output {
            println!("{}", secret);
        }

        Ok(secret)
    }

    pub fn update_secret(&self, secret_name: &str) -> Result<()> {
        let secret_value = Password::new("Enter the secret value:")
            .with_validator(required!())
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt()
            .with_context(|| "unable to read secret from input")?;
        let h = Handle::current();
        tokio::task::block_in_place(|| {
            h.block_on(
                self.local_provider
                    .update_secret(secret_name, &secret_value),
            )
        })?;
        println!("✓ Secret '{secret_name}' updated in the vault.");

        Ok(())
    }

    pub fn delete_secret(&self, secret_name: &str) -> Result<()> {
        let h = Handle::current();
        tokio::task::block_in_place(|| h.block_on(self.local_provider.delete_secret(secret_name)))?;
        println!("✓ Secret '{secret_name}' deleted from the vault.");

        Ok(())
    }

    pub fn list_secrets(&self, display_output: bool) -> Result<Vec<String>> {
        let h = Handle::current();
        let secrets =
            tokio::task::block_in_place(|| h.block_on(self.local_provider.list_secrets()))?;

        if display_output {
            if secrets.is_empty() {
                println!("The vault is empty.");
            } else {
                for key in &secrets {
                    println!("{}", key);
                }
            }
        }

        Ok(secrets)
    }

    pub fn handle_vault_flags(cli: Cli, config: Config) -> Result<()> {
        if let Some(secret_name) = cli.add_secret {
            config.vault.add_secret(&secret_name)?;
        }

        if let Some(secret_name) = cli.get_secret {
            config.vault.get_secret(&secret_name, true)?;
        }

        if let Some(secret_name) = cli.update_secret {
            config.vault.update_secret(&secret_name)?;
        }

        if let Some(secret_name) = cli.delete_secret {
            config.vault.delete_secret(&secret_name)?;
        }

        if cli.list_secrets {
            config.vault.list_secrets(true)?;
        }

        Ok(())
    }
}
