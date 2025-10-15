use crate::cli::Cli;
use crate::config::{ensure_parent_exists, Config};
use anyhow::{anyhow, Context, Result};
use clap_complete::CompletionCandidate;
use gman::providers::local::LocalProvider;
use gman::providers::SecretProvider;
use inquire::validator::Validation;
use inquire::{min_length, required, Confirm, Password, PasswordDisplayMode, Text};
use std::ffi::OsStr;
use std::io;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use tokio::runtime::Handle;

impl Cli {
    pub async fn handle_secret_flag(&self, mut config: Config) -> Result<()> {
        ensure_password_file_initialized(&mut config)?;

        let local_provider = match config.secrets_provider {
			Some(lc) => lc,
			None => {
				return Err(anyhow!(
					"Local secrets provider is not configured. Please ensure a password file is configured and try again."
				))
			}
		};

        if let Some(secret_name) = &self.add_secret {
            let plaintext =
                read_all_stdin().with_context(|| "unable to read plaintext from stdin")?;
            local_provider
                .set_secret(secret_name, plaintext.trim_end())
                .await?;
            println!("✓ Secret '{secret_name}' added to the vault.");
        }
        if let Some(secret_name) = &self.get_secret {
            let secret = local_provider.get_secret(secret_name).await?;
            println!("{}", secret);
        }
        if let Some(secret_name) = &self.update_secret {
            let plaintext =
                read_all_stdin().with_context(|| "unable to read plaintext from stdin")?;
            local_provider
                .update_secret(secret_name, plaintext.trim_end())
                .await?;
            println!("✓ Secret '{secret_name}' updated in the vault.");
        }
        if let Some(secret_name) = &self.delete_secret {
            local_provider.delete_secret(secret_name).await?;
            println!("✓ Secret '{secret_name}' deleted from the vault.");
        }
        if self.list_secrets {
            let secrets = local_provider.list_secrets().await?;
            if secrets.is_empty() {
                println!("The vault is empty.");
            } else {
                for key in &secrets {
                    println!("{}", key);
                }
            }
        }

        Ok(())
    }
}

fn ensure_password_file_initialized(config: &mut Config) -> Result<()> {
    let secrets_password_file = config.secrets_password_file();
    if secrets_password_file.exists() {
        {
            let file_contents = std::fs::read_to_string(&secrets_password_file)?;
            if !file_contents.trim().is_empty() {
                return Ok(());
            }
        }

        let ans = Confirm::new(
            format!(
                "The configured password file '{}' is empty. Create a password?",
                secrets_password_file.display()
            )
            .as_str(),
        )
        .with_default(true)
        .prompt()?;

        if !ans {
            return Err(anyhow!("The configured password file '{}' is empty. Please populate it with a password and try again.", secrets_password_file.display()));
        }

        let password = Password::new("Enter a password to encrypt all vault secrets:")
            .with_validator(required!())
            .with_validator(min_length!(10))
            .with_display_mode(PasswordDisplayMode::Masked)
            .prompt();

        match password {
            Ok(pw) => {
                std::fs::write(&secrets_password_file, pw.as_bytes())?;
                load_secrets_provider(config);
                println!(
                    "✓ Password file '{}' updated.",
                    secrets_password_file.display()
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
            return Err(anyhow!("A password file is required to utilize secrets. Please configure a password file in your config file and try again."));
        }

        let password_file: PathBuf = Text::new("Enter the path to the password file to create:")
            .with_default(&secrets_password_file.display().to_string())
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

        if password_file != secrets_password_file {
            println!("Note: The default password file path is '{}'. You have chosen to create a different path: '{}'. Please ensure your configuration is updated accordingly.", secrets_password_file.display(), password_file.display());
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
                config.password_file = Some(password_file);
                load_secrets_provider(config);
                println!(
                    "✓ Password file '{}' created.",
                    secrets_password_file.display()
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

fn load_secrets_provider(config: &mut Config) {
    let password_file = Some(config.secrets_password_file());
    config.secrets_provider = Some(LocalProvider {
        password_file,
        git_branch: None,
        ..LocalProvider::default()
    });
}

fn read_all_stdin() -> Result<String> {
    if io::stdin().is_terminal() {
        #[cfg(not(windows))]
        eprintln!("Enter the text to encrypt, then press Ctrl-D twice to finish input");
        #[cfg(windows)]
        eprintln!("Enter the text to encrypt, then press Ctrl-Z to finish input");
        io::stderr().flush()?;
    }
    let mut buf = String::new();
    let stdin_tty = io::stdin().is_terminal();
    let stdout_tty = io::stdout().is_terminal();
    io::stdin().read_to_string(&mut buf)?;

    if stdin_tty && stdout_tty && !buf.ends_with('\n') {
        let mut out = io::stdout().lock();
        out.write_all(b"\n")?;
        out.flush()?;
    }
    Ok(buf)
}

pub fn secrets_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let cur = current.to_string_lossy();
    match Config::init_bare() {
        Ok(config) => {
            let local_provider = match config.secrets_provider {
                Some(pc) => pc,
                None => return vec![],
            };
            let h = Handle::current();
            tokio::task::block_in_place(|| h.block_on(local_provider.list_secrets()))
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.starts_with(&*cur))
                .map(CompletionCandidate::new)
                .collect()
        }
        Err(_) => vec![],
    }
}
