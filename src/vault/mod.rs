mod utils;

use std::path::PathBuf;
pub use utils::create_vault_password_file;
pub use utils::interpolate_secrets;

use crate::cli::Cli;
use crate::config::AppConfig;
use crate::vault::utils::ensure_password_file_initialized;
use anyhow::{Context, Result};
use fancy_regex::Regex;
use gman::providers::SecretProvider;
use gman::providers::local::LocalProvider;
use inquire::{Password, PasswordDisplayMode, required};
use std::sync::{Arc, LazyLock};
use tokio::runtime::Handle;

pub static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{(.+)}}").unwrap());

#[derive(Debug, Default, Clone)]
pub struct Vault {
    local_provider: LocalProvider,
}

pub type GlobalVault = Arc<Vault>;

impl Vault {
    pub fn init_bare() -> Self {
        let vault_password_file = AppConfig::default().vault_password_file();
        let local_provider = LocalProvider {
            password_file: Some(vault_password_file),
            git_branch: None,
            ..LocalProvider::default()
        };

        Self { local_provider }
    }

    pub fn init(config: &AppConfig) -> Self {
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

    pub fn password_file(&self) -> Result<PathBuf> {
        self.local_provider
            .password_file
            .clone()
            .with_context(|| "A password file is required for the local provider")
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

    pub fn handle_vault_flags(cli: Cli, vault: &Vault) -> Result<()> {
        if let Some(secret_name) = cli.add_secret {
            vault.add_secret(&secret_name)?;
        }

        if let Some(secret_name) = cli.get_secret {
            vault.get_secret(&secret_name, true)?;
        }

        if let Some(secret_name) = cli.update_secret {
            vault.update_secret(&secret_name)?;
        }

        if let Some(secret_name) = cli.delete_secret {
            vault.delete_secret(&secret_name)?;
        }

        if cli.list_secrets {
            vault.list_secrets(true)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_re_matches_double_braces() {
        let captures = SECRET_RE.captures("{{MY_SECRET}}").unwrap().unwrap();
        assert_eq!(&captures[1], "MY_SECRET");
    }

    #[test]
    fn secret_re_matches_with_surrounding_text() {
        let text = "key={{API_KEY}} here";
        let captures = SECRET_RE.captures(text).unwrap().unwrap();
        assert_eq!(&captures[1], "API_KEY");
    }

    #[test]
    fn secret_re_no_match_single_braces() {
        let result = SECRET_RE.captures("{NOT_SECRET}").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn secret_re_no_match_plain_text() {
        let result = SECRET_RE.captures("just plain text").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn secret_re_matches_with_spaces() {
        let captures = SECRET_RE.captures("{{ SPACED }}").unwrap().unwrap();
        assert_eq!(&captures[1], " SPACED ");
    }

    #[test]
    fn vault_default_creates_instance() {
        let vault = Vault::default();
        assert!(vault.password_file().is_err());
    }
}
