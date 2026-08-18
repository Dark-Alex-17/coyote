mod utils;

use std::env;
use std::fs::read_to_string;
use std::path::PathBuf;

use crate::config::paths;
use crate::sandbox::SANDBOX_ENV_FLAG;
pub use utils::create_vault_password_file;
pub use utils::interpolate_secrets;
pub use utils::prompt_provider_choice;

use crate::cli::Cli;
use crate::config::AppConfig;
use crate::utils::drain_stale_tty_input;
use crate::vault::utils::ensure_password_file_initialized;
use anyhow::{Context, Result, anyhow, bail};
use fancy_regex::Regex;
use gman::providers::SecretProvider;
use gman::providers::SupportedProvider;
use gman::providers::local::LocalProvider;
use inquire::{Password, PasswordDisplayMode, required};
use log::warn;
use serde_yaml::Value;
use std::sync::{Arc, LazyLock};
use tokio::runtime::Handle;
use uuid::Uuid;

pub static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{([^{}]+)}}").unwrap());

#[derive(Debug, Default, Clone)]
pub struct Vault {
    pub(crate) provider: SupportedProvider,
    sandbox_mode: bool,
}

pub type GlobalVault = Arc<Vault>;

impl Vault {
    pub fn init_bare() -> Result<Self> {
        let config_path = paths::config_file();
        if !config_path.exists() {
            bail!(
                "Coyote config not found at {}. Run first-run setup before using the vault.",
                config_path.display()
            );
        }
        let content = read_to_string(&config_path)
            .with_context(|| format!("failed to read config at {}", config_path.display()))?;
        let value: Value = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse config at {}", config_path.display()))?;

        let provider = match value.get("secrets_provider") {
            Some(v) if !v.is_null() => serde_yaml::from_value::<SupportedProvider>(v.clone())
                .with_context(|| "failed to parse 'secrets_provider' from config")?,
            _ => {
                let password_file = value
                    .get("vault_password_file")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| AppConfig::default().vault_password_file());
                SupportedProvider::Local {
                    provider_def: LocalProvider {
                        password_file: Some(password_file),
                        git_branch: None,
                        ..LocalProvider::default()
                    },
                }
            }
        };

        Ok(Self {
            provider,
            sandbox_mode: false,
        })
    }

    pub fn default_local() -> Self {
        Self {
            provider: SupportedProvider::Local {
                provider_def: LocalProvider {
                    password_file: Some(AppConfig::default().vault_password_file()),
                    git_branch: None,
                    ..LocalProvider::default()
                },
            },
            sandbox_mode: false,
        }
    }

    pub fn from_provider(provider: SupportedProvider) -> Self {
        Self {
            provider,
            sandbox_mode: false,
        }
    }

    pub fn init(config: &AppConfig) -> Result<Self> {
        if env::var_os(SANDBOX_ENV_FLAG).is_some() {
            return Ok(Self {
                sandbox_mode: true,
                ..Self::default()
            });
        }

        let mut provider = match &config.secrets_provider {
            Some(p) => p.clone(),
            None => SupportedProvider::Local {
                provider_def: LocalProvider {
                    password_file: Some(config.vault_password_file()),
                    ..LocalProvider::default()
                },
            },
        };

        if let SupportedProvider::Local { provider_def } = &mut provider {
            ensure_password_file_initialized(provider_def)?;
        }

        Ok(Self {
            provider,
            sandbox_mode: false,
        })
    }

    pub fn local_password_file(&self) -> Result<PathBuf> {
        match &self.provider {
            SupportedProvider::Local { provider_def } => provider_def
                .password_file
                .clone()
                .with_context(|| "A password file is required for the local provider"),
            _ => Err(anyhow!(
                "password_file is only available for the local provider"
            )),
        }
    }

    fn provider_ref(&self) -> &dyn SecretProvider {
        match &self.provider {
            SupportedProvider::Local { provider_def } => provider_def,
            SupportedProvider::AwsSecretsManager { provider_def } => provider_def,
            SupportedProvider::GcpSecretManager { provider_def } => provider_def,
            SupportedProvider::AzureKeyVault { provider_def } => provider_def,
            SupportedProvider::Gopass { provider_def } => provider_def,
            SupportedProvider::OnePassword { provider_def } => provider_def,
        }
    }

    pub fn add_secret(&self, secret_name: &str) -> Result<()> {
        if self.sandbox_mode {
            bail!(
                "Vault management is disabled in sandbox mode. Use `coyote --add-secret` on your host."
            );
        }
        drain_stale_tty_input();
        let secret_value = Password::new("Enter the secret value:")
            .with_validator(required!())
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt()
            .with_context(|| "unable to read secret from input")?;

        let h = Handle::current();
        tokio::task::block_in_place(|| {
            h.block_on(self.provider_ref().set_secret(secret_name, &secret_value))
        })?;
        println!("✓ Secret '{secret_name}' added to the vault.");

        Ok(())
    }

    pub fn get_secret(&self, secret_name: &str, display_output: bool) -> Result<String> {
        if self.sandbox_mode {
            bail!(
                "Vault management is disabled in sandbox mode. Use `coyote --add-secret` on your host."
            );
        }
        let h = Handle::current();
        let secret = tokio::task::block_in_place(|| {
            h.block_on(self.provider_ref().get_secret(secret_name))
        })?;

        if display_output {
            println!("{}", secret);
        }

        Ok(secret)
    }

    pub fn update_secret(&self, secret_name: &str) -> Result<()> {
        if self.sandbox_mode {
            bail!(
                "Vault management is disabled in sandbox mode. Use `coyote --add-secret` on your host."
            );
        }
        drain_stale_tty_input();
        let secret_value = Password::new("Enter the secret value:")
            .with_validator(required!())
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt()
            .with_context(|| "unable to read secret from input")?;
        let h = Handle::current();
        tokio::task::block_in_place(|| {
            h.block_on(
                self.provider_ref()
                    .update_secret(secret_name, &secret_value),
            )
        })?;
        println!("✓ Secret '{secret_name}' updated in the vault.");

        Ok(())
    }

    pub fn delete_secret(&self, secret_name: &str) -> Result<()> {
        if self.sandbox_mode {
            bail!(
                "Vault management is disabled in sandbox mode. Use `coyote --add-secret` on your host."
            );
        }
        let h = Handle::current();
        tokio::task::block_in_place(|| h.block_on(self.provider_ref().delete_secret(secret_name)))?;
        println!("✓ Secret '{secret_name}' deleted from the vault.");

        Ok(())
    }

    pub fn list_secrets(&self, display_output: bool) -> Result<Vec<String>> {
        if self.sandbox_mode {
            bail!(
                "Vault management is disabled in sandbox mode. Use `coyote --add-secret` on your host."
            );
        }
        let h = Handle::current();
        let secrets =
            tokio::task::block_in_place(|| h.block_on(self.provider_ref().list_secrets()))?;

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

    pub fn auth_hint(&self) -> Option<&'static str> {
        match &self.provider {
            SupportedProvider::AwsSecretsManager { .. } => Some(
                "Try `aws sso login` (for SSO setups) or `aws configure` (for static keys), then retry.",
            ),
            SupportedProvider::GcpSecretManager { .. } => {
                Some("Try `gcloud auth application-default login`, then retry.")
            }
            SupportedProvider::AzureKeyVault { .. } => Some("Try `az login`, then retry."),
            SupportedProvider::Gopass { .. } => {
                Some("Make sure `gopass init` has been run and `gopass` is on your PATH.")
            }
            SupportedProvider::OnePassword { .. } => Some("Try `op signin`, then retry."),
            SupportedProvider::Local { .. } => None,
        }
    }

    pub fn validate_round_trip(&self) -> Result<()> {
        const PROBE_VALUE: &str = "ok";
        let probe_key = format!("coyote-setup-probe-{}", Uuid::new_v4().simple());

        let h = Handle::current();
        let result: Result<()> = tokio::task::block_in_place(|| {
            h.block_on(async {
                self.provider_ref()
                    .set_secret(&probe_key, PROBE_VALUE)
                    .await
                    .with_context(|| "vault write probe failed")?;
                let got = self
                    .provider_ref()
                    .get_secret(&probe_key)
                    .await
                    .with_context(|| "vault read probe failed")?;
                if got != PROBE_VALUE {
                    if let Err(cleanup_err) = self.provider_ref().delete_secret(&probe_key).await {
                        warn!("vault probe cleanup failed for key '{probe_key}': {cleanup_err}");
                    }
                    bail!("vault read probe returned an unexpected value");
                }

                self.provider_ref()
                    .delete_secret(&probe_key)
                    .await
                    .with_context(|| "vault delete probe failed")?;

                Ok(())
            })
        });

        result.with_context(|| {
            let base = "Vault validation failed. Check that your credentials have permission to create, read, and delete secrets in the configured backend.";
            match self.auth_hint() {
                Some(hint) => format!("{base}\n\nHint: {hint}"),
                None => base.to_string(),
            }
        })?;

        println!("✓ Vault validation succeeded.");
        Ok(())
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
        assert!(vault.local_password_file().is_err());
    }

    #[test]
    fn vault_disabled_in_sandbox() {
        let prev = std::env::var_os("IS_SANDBOX");
        unsafe {
            std::env::set_var("IS_SANDBOX", "1");
        }

        let vault = Vault::init(&AppConfig::default()).unwrap();

        let check = |err: anyhow::Error| {
            let msg = err.to_string();
            assert!(
                msg.contains("Vault management is disabled in sandbox mode"),
                "expected vault-disabled error, got: {msg}"
            );
        };

        check(vault.add_secret("test").unwrap_err());
        check(vault.get_secret("test", false).unwrap_err());
        check(vault.update_secret("test").unwrap_err());
        check(vault.delete_secret("test").unwrap_err());
        check(vault.list_secrets(false).unwrap_err());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("IS_SANDBOX", v),
                None => std::env::remove_var("IS_SANDBOX"),
            }
        }
    }
}
