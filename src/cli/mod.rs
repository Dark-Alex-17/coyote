mod completer;

use crate::cli::completer::{
    ShellCompletion, agent_completer, macro_completer, model_completer, rag_completer,
    role_completer, secrets_completer, session_completer,
};
use anyhow::{Context, Result};
use clap::ValueHint;
use clap::{Parser, crate_authors, crate_description, crate_name, crate_version};
use clap_complete::ArgValueCompleter;
use is_terminal::IsTerminal;
use std::io::{Read, stdin};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(
	name = crate_name!(),
	author = crate_authors!(),
	version = crate_version!(),
	about = crate_description!(),
	help_template = "\
{before-help}{name} {version}
{author-with-newline}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}
"
)]
pub struct Cli {
    /// Select a LLM model
    #[arg(short, long, add = ArgValueCompleter::new(model_completer))]
    pub model: Option<String>,
    /// Use the system prompt
    #[arg(long)]
    pub prompt: Option<String>,
    /// Select a role
    #[arg(short, long, add = ArgValueCompleter::new(role_completer))]
    pub role: Option<String>,
    /// Start or join a session
    #[arg(short = 's', long, add = ArgValueCompleter::new(session_completer))]
    pub session: Option<Option<String>>,
    /// Ensure the session is empty
    #[arg(long)]
    pub empty_session: bool,
    /// Ensure the new conversation is saved to the session
    #[arg(long)]
    pub save_session: bool,
    /// Start an agent
    #[arg(short = 'a', long, add = ArgValueCompleter::new(agent_completer))]
    pub agent: Option<String>,
    /// Set agent variables
    #[arg(long, value_names = ["NAME", "VALUE"], num_args = 2)]
    pub agent_variable: Vec<String>,
    /// Start a RAG
    #[arg(long, add = ArgValueCompleter::new(rag_completer))]
    pub rag: Option<String>,
    /// Rebuild the RAG to sync document changes
    #[arg(long)]
    pub rebuild_rag: bool,
    /// Execute a macro
    #[arg(long = "macro", value_name = "MACRO", add = ArgValueCompleter::new(macro_completer))]
    pub macro_name: Option<String>,
    /// Execute commands in natural language
    #[arg(short = 'e', long)]
    pub execute: bool,
    /// Output code only
    #[arg(short = 'c', long)]
    pub code: bool,
    /// Include files, directories, or URLs
    #[arg(short = 'f', long, value_name = "FILE|URL", value_hint = ValueHint::AnyPath)]
    pub file: Vec<String>,
    /// Turn off stream mode
    #[arg(short = 'S', long)]
    pub no_stream: bool,
    /// Display the message without sending it
    #[arg(long)]
    pub dry_run: bool,
    /// Display information
    #[arg(long)]
    pub info: bool,
    /// Build all configured Bash tool scripts
    #[arg(long)]
    pub build_tools: bool,
    /// Sync models updates
    #[arg(long)]
    pub sync_models: bool,
    /// List all available chat models
    #[arg(long)]
    pub list_models: bool,
    /// List all roles
    #[arg(long)]
    pub list_roles: bool,
    /// List all sessions
    #[arg(long)]
    pub list_sessions: bool,
    /// List all agents
    #[arg(long)]
    pub list_agents: bool,
    /// List all RAGs
    #[arg(long)]
    pub list_rags: bool,
    /// List all macros
    #[arg(long)]
    pub list_macros: bool,
    /// Input text
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,
    /// Tail logs
    #[arg(long)]
    pub tail_logs: bool,
    /// Disable colored log output
    #[arg(long, requires = "tail_logs")]
    pub disable_log_colors: bool,
    /// Add a secret to the Loki vault
    #[arg(long, value_name = "SECRET_NAME", exclusive = true)]
    pub add_secret: Option<String>,
    /// Decrypt a secret from the Loki vault and print the plaintext
    #[arg(long, value_name = "SECRET_NAME", exclusive = true, add = ArgValueCompleter::new(secrets_completer))]
    pub get_secret: Option<String>,
    /// Update an existing secret in the Loki vault
    #[arg(long, value_name = "SECRET_NAME", exclusive = true, add = ArgValueCompleter::new(secrets_completer))]
    pub update_secret: Option<String>,
    /// Delete a secret from the Loki vault
    #[arg(long, value_name = "SECRET_NAME", exclusive = true, add = ArgValueCompleter::new(secrets_completer))]
    pub delete_secret: Option<String>,
    /// List all secrets stored in the Loki vault
    #[arg(long, exclusive = true)]
    pub list_secrets: bool,
    /// Generate static shell completion scripts
    #[arg(long, value_name = "SHELL", value_enum)]
    pub completions: Option<ShellCompletion>,
}

impl Cli {
    pub fn text(&self) -> Result<Option<String>> {
        let mut stdin_text = String::new();
        if !stdin().is_terminal() {
            let _ = stdin()
                .read_to_string(&mut stdin_text)
                .context("Invalid stdin pipe")?;
        };
        match self.text.is_empty() {
            true => {
                if stdin_text.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(stdin_text))
                }
            }
            false => {
                if self.macro_name.is_some() {
                    let text = self
                        .text
                        .iter()
                        .map(|v| shell_words::quote(v))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if stdin_text.is_empty() {
                        Ok(Some(text))
                    } else {
                        Ok(Some(format!("{text} -- {stdin_text}")))
                    }
                } else {
                    let text = self.text.join(" ");
                    if stdin_text.is_empty() {
                        Ok(Some(text))
                    } else {
                        Ok(Some(format!("{text}\n{stdin_text}")))
                    }
                }
            }
        }
    }
}
