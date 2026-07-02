mod completer;

use crate::cli::completer::{
    ShellCompletion, agent_completer, macro_completer, mcp_server_completer, model_completer,
    rag_completer, role_completer, secrets_completer, session_completer,
};
use crate::config::{AssetCategory, InstallFilter, MemoryScope};
use anyhow::{Context, Result};
use clap::{ArgGroup, ValueHint};
use clap::{Parser, crate_authors, crate_description, crate_version};
use clap_complete::ArgValueCompleter;
use is_terminal::IsTerminal;
use std::collections::HashSet;
use std::io::{Read, stdin};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
#[command(
	name = "coyote",
	author = crate_authors!(),
	version = crate_version!(),
	about = crate_description!(),
	help_template = "\
{before-help}{name} {version}
{author-with-newline}
{about-with-newline}
{usage-heading} {usage}

{all-args}{after-help}
",
	group(
		ArgGroup::new("sbx-mode")
			.args(["sandbox", "fresh", "no_mixins"])
			.multiple(true)
			.conflicts_with_all([
				"model", "prompt", "role", "session", "agent", "rag", "rebuild_rag",
				"macro_name", "execute", "code", "file", "no_stream", "no_memory",
				"init_memory", "dry_run", "info", "build_tools", "install",
				"install_from", "sync_models", "list_models", "list_roles",
				"list_sessions", "list_agents", "list_rags", "list_macros",
				"list_skills", "skill", "tail_logs", "completions", "update",
			])
	),
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
    /// Disable memory for this invocation
    #[arg(long)]
    pub no_memory: bool,
    /// Bootstrap a memory marker so coyote begins loading memory next run
    #[arg(long, value_name = "SCOPE", value_enum)]
    pub init_memory: Option<MemoryScope>,
    /// Display the message without sending it
    #[arg(long)]
    pub dry_run: bool,
    /// Display information
    #[arg(long)]
    pub info: bool,
    /// Build all configured Bash tool scripts
    #[arg(long)]
    pub build_tools: bool,
    /// Reinstall bundled assets, overwriting any local changes
    #[arg(long, value_name = "CATEGORY", value_enum)]
    pub install: Option<AssetCategory>,
    /// Install assets from a remote git repository (URL may be suffixed with #<ref>)
    #[arg(long, value_name = "GIT_URL")]
    pub install_from: Option<String>,
    /// Restrict --install-from to a single asset category
    #[arg(long, value_name = "CATEGORY", value_enum, requires = "install_from")]
    pub filter: Option<InstallFilter>,
    /// Overwrite all conflicts without prompting (used with --install-from)
    #[arg(long, requires = "install_from")]
    pub install_force: bool,
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
    /// List all installed skills
    #[arg(long)]
    pub list_skills: bool,
    /// Pre-load an existing skill into the session (repeatable). If a single
    /// `--skill <NAME>` is given and the skill doesn't exist, opens $EDITOR
    /// with a scaffold to create it.
    #[arg(long, value_name = "NAME")]
    pub skill: Vec<String>,
    /// Input text
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,
    /// Tail logs
    #[arg(long)]
    pub tail_logs: bool,
    /// Disable colored log output
    #[arg(long, requires = "tail_logs")]
    pub disable_log_colors: bool,
    /// Add a secret to the Coyote vault
    #[arg(long, value_name = "SECRET_NAME", exclusive = true)]
    pub add_secret: Option<String>,
    /// Decrypt a secret from the Coyote vault and print the plaintext
    #[arg(long, value_name = "SECRET_NAME", exclusive = true, add = ArgValueCompleter::new(secrets_completer))]
    pub get_secret: Option<String>,
    /// Update an existing secret in the Coyote vault
    #[arg(long, value_name = "SECRET_NAME", exclusive = true, add = ArgValueCompleter::new(secrets_completer))]
    pub update_secret: Option<String>,
    /// Delete a secret from the Coyote vault
    #[arg(long, value_name = "SECRET_NAME", exclusive = true, add = ArgValueCompleter::new(secrets_completer))]
    pub delete_secret: Option<String>,
    /// List all secrets stored in the Coyote vault
    #[arg(long, exclusive = true)]
    pub list_secrets: bool,
    /// Authenticate with an LLM provider using OAuth (e.g., --authenticate client_name)
    #[arg(long, exclusive = true, value_name = "CLIENT_NAME")]
    pub authenticate: Option<Option<String>>,
    /// Authenticate with an OAuth-protected remote MCP server (e.g., --auth-mcp server_name)
    #[arg(long, exclusive = true, value_name = "SERVER_NAME", add = ArgValueCompleter::new(mcp_server_completer))]
    pub auth_mcp: Option<String>,
    /// Generate static shell completion scripts
    #[arg(long, value_name = "SHELL", value_enum)]
    pub completions: Option<ShellCompletion>,
    /// Update Coyote to the latest release, or to a specific version
    #[arg(long, value_name = "VERSION")]
    pub update: Option<Option<String>>,
    /// With --update, update even if Coyote was installed via a package manager
    #[arg(long, requires = "update")]
    pub force: bool,
    /// Launch Coyote inside a Docker sandbox (via `sbx`); name defaults to current directory basename
    #[arg(long, value_name = "NAME")]
    pub sandbox: Option<Option<String>>,
    /// Create the sandbox without bootstrapping the host config or vault password file
    #[arg(long, requires = "sandbox")]
    pub fresh: bool,
    /// Skip discovery and application of all sbx mixins (user and built-in)
    #[arg(long, requires = "sandbox")]
    pub no_mixins: bool,
}

impl Cli {
    pub fn skills(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(self.skill.len());
        for name in &self.skill {
            if seen.insert(name.clone()) {
                out.push(name.clone());
            }
        }

        out
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        let mut full_args = vec!["coyote"];
        full_args.extend_from_slice(args);
        Cli::try_parse_from(full_args).unwrap()
    }

    #[test]
    fn parse_no_args_defaults() {
        let cli = parse(&[]);
        assert!(cli.model.is_none());
        assert!(cli.role.is_none());
        assert!(cli.session.is_none());
        assert!(cli.agent.is_none());
        assert!(!cli.execute);
        assert!(!cli.code);
        assert!(!cli.no_stream);
        assert!(!cli.dry_run);
        assert!(!cli.info);
        assert!(!cli.build_tools);
        assert!(cli.file.is_empty());
        assert!(cli.text.is_empty());
    }

    #[test]
    fn parse_model_flag() {
        let cli = parse(&["--model", "gpt-4o"]);
        assert_eq!(cli.model, Some("gpt-4o".to_string()));
    }

    #[test]
    fn parse_model_short_flag() {
        let cli = parse(&["-m", "gpt-4o"]);
        assert_eq!(cli.model, Some("gpt-4o".to_string()));
    }

    #[test]
    fn parse_role_flag() {
        let cli = parse(&["--role", "coder"]);
        assert_eq!(cli.role, Some("coder".to_string()));
    }

    #[test]
    fn parse_session_with_name() {
        let cli = parse(&["--session", "my-session"]);
        assert_eq!(cli.session, Some(Some("my-session".to_string())));
    }

    #[test]
    fn parse_agent_flag() {
        let cli = parse(&["--agent", "sisyphus"]);
        assert_eq!(cli.agent, Some("sisyphus".to_string()));
    }

    #[test]
    fn parse_agent_short_flag() {
        let cli = parse(&["-a", "sisyphus"]);
        assert_eq!(cli.agent, Some("sisyphus".to_string()));
    }

    #[test]
    fn parse_execute_flag() {
        let cli = parse(&["-e", "list files"]);
        assert!(cli.execute);
    }

    #[test]
    fn parse_code_flag() {
        let cli = parse(&["-c", "hello world"]);
        assert!(cli.code);
    }

    #[test]
    fn parse_no_stream_flag() {
        let cli = parse(&["-S", "test"]);
        assert!(cli.no_stream);
    }

    #[test]
    fn parse_dry_run_flag() {
        let cli = parse(&["--dry-run", "test"]);
        assert!(cli.dry_run);
    }

    #[test]
    fn parse_info_flag() {
        let cli = parse(&["--info"]);
        assert!(cli.info);
    }

    #[test]
    fn parse_list_flags() {
        assert!(parse(&["--list-models"]).list_models);
        assert!(parse(&["--list-roles"]).list_roles);
        assert!(parse(&["--list-sessions"]).list_sessions);
        assert!(parse(&["--list-agents"]).list_agents);
        assert!(parse(&["--list-rags"]).list_rags);
        assert!(parse(&["--list-macros"]).list_macros);
        assert!(parse(&["--list-skills"]).list_skills);
    }

    #[test]
    fn parse_skill_flag_takes_name() {
        assert_eq!(parse(&["--skill", "git-master"]).skill, vec!["git-master"]);
        assert!(parse(&[]).skill.is_empty());
    }

    #[test]
    fn parse_multiple_skill_flags_preserves_order() {
        assert_eq!(
            parse(&["--skill", "alpha", "--skill", "beta", "--skill", "gamma"]).skill,
            vec!["alpha", "beta", "gamma"]
        );
    }

    #[test]
    fn skills_method_dedupes_preserving_first_occurrence() {
        let cli = parse(&[
            "--skill", "alpha", "--skill", "beta", "--skill", "alpha", "--skill", "gamma",
            "--skill", "beta",
        ]);

        assert_eq!(cli.skills(), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn skills_method_returns_empty_when_no_flags() {
        assert!(parse(&[]).skills().is_empty());
    }

    #[test]
    fn parse_file_flag_single() {
        let cli = parse(&["-f", "file.txt", "question"]);
        assert_eq!(cli.file, vec!["file.txt"]);
    }

    #[test]
    fn parse_file_flag_multiple() {
        let cli = parse(&["-f", "a.txt", "-f", "b.txt", "question"]);
        assert_eq!(cli.file, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn parse_trailing_text() {
        let cli = parse(&["hello", "world"]);
        assert_eq!(cli.text, vec!["hello", "world"]);
    }

    #[test]
    fn parse_prompt_flag() {
        let cli = parse(&["--prompt", "be a pirate"]);
        assert_eq!(cli.prompt, Some("be a pirate".to_string()));
    }

    #[test]
    fn parse_empty_session_flag() {
        let cli = parse(&["--session", "s", "--empty-session"]);
        assert!(cli.empty_session);
    }

    #[test]
    fn parse_save_session_flag() {
        let cli = parse(&["--session", "s", "--save-session"]);
        assert!(cli.save_session);
    }

    #[test]
    fn parse_build_tools_flag() {
        let cli = parse(&["--build-tools"]);
        assert!(cli.build_tools);
    }

    #[test]
    fn parse_sync_models_flag() {
        let cli = parse(&["--sync-models"]);
        assert!(cli.sync_models);
    }

    #[test]
    fn parse_model_with_role() {
        let cli = parse(&["-m", "gpt-4o", "-r", "coder"]);
        assert_eq!(cli.model, Some("gpt-4o".to_string()));
        assert_eq!(cli.role, Some("coder".to_string()));
    }

    #[test]
    fn parse_agent_with_file_and_text() {
        let cli = parse(&["-a", "sisyphus", "-f", "code.rs", "explain", "this"]);
        assert_eq!(cli.agent, Some("sisyphus".to_string()));
        assert_eq!(cli.file, vec!["code.rs"]);
        assert_eq!(cli.text, vec!["explain", "this"]);
    }

    #[test]
    fn parse_role_with_session() {
        let cli = parse(&["-r", "coder", "-s", "dev-session"]);
        assert_eq!(cli.role, Some("coder".to_string()));
        assert_eq!(cli.session, Some(Some("dev-session".to_string())));
    }

    #[test]
    fn cli_text_returns_none_when_no_text_no_stdin() {
        let cli = parse(&[]);
        assert!(cli.text().unwrap().is_none());
    }

    #[test]
    fn cli_text_joins_trailing_args() {
        let cli = parse(&["hello", "world"]);
        assert_eq!(cli.text().unwrap(), Some("hello world".to_string()));
    }

    #[test]
    fn parse_add_secret_flag() {
        let cli = parse(&["--add-secret", "MY_KEY"]);
        assert_eq!(cli.add_secret, Some("MY_KEY".to_string()));
    }

    #[test]
    fn parse_get_secret_flag() {
        let cli = parse(&["--get-secret", "MY_KEY"]);
        assert_eq!(cli.get_secret, Some("MY_KEY".to_string()));
    }

    #[test]
    fn parse_list_secrets_flag() {
        let cli = parse(&["--list-secrets"]);
        assert!(cli.list_secrets);
    }

    #[test]
    fn parse_rag_flag() {
        let cli = parse(&["--rag", "my-rag"]);
        assert_eq!(cli.rag, Some("my-rag".to_string()));
    }

    #[test]
    fn parse_macro_flag() {
        let cli = parse(&["--macro", "my-macro"]);
        assert_eq!(cli.macro_name, Some("my-macro".to_string()));
    }

    #[test]
    fn parse_update_flag_no_value() {
        let cli = parse(&["--update"]);

        assert_eq!(cli.update, Some(None));
    }

    #[test]
    fn parse_update_flag_with_version() {
        let cli = parse(&["--update", "v0.4.0"]);

        assert_eq!(cli.update, Some(Some("v0.4.0".to_string())));
    }

    #[test]
    fn parse_update_with_force() {
        let cli = parse(&["--update", "--force"]);

        assert_eq!(cli.update, Some(None));
        assert!(cli.force);
    }

    #[test]
    fn parse_force_without_update_fails() {
        assert!(Cli::try_parse_from(["coyote", "--force"]).is_err());
    }

    #[test]
    fn parse_sandbox_flag_no_value() {
        let cli = parse(&["--sandbox"]);
        assert_eq!(cli.sandbox, Some(None));
    }

    #[test]
    fn parse_sandbox_flag_with_name() {
        let cli = parse(&["--sandbox", "my-box"]);
        assert_eq!(cli.sandbox, Some(Some("my-box".to_string())));
    }

    #[test]
    fn parse_sandbox_is_exclusive() {
        assert!(Cli::try_parse_from(["coyote", "--sandbox", "--agent", "foo"]).is_err());
    }

    #[test]
    fn parse_fresh_flag_requires_sandbox() {
        assert!(Cli::try_parse_from(["coyote", "--fresh"]).is_err());
    }

    #[test]
    fn parse_fresh_flag_with_sandbox() {
        let cli = parse(&["--sandbox", "--fresh"]);
        assert_eq!(cli.sandbox, Some(None));
        assert!(cli.fresh);
    }

    #[test]
    fn parse_fresh_flag_with_named_sandbox() {
        let cli = parse(&["--sandbox", "foo", "--fresh"]);
        assert_eq!(cli.sandbox, Some(Some("foo".to_string())));
        assert!(cli.fresh);
    }

    #[test]
    fn parse_no_mixins_requires_sandbox() {
        assert!(Cli::try_parse_from(["coyote", "--no-mixins"]).is_err());
    }

    #[test]
    fn parse_no_mixins_with_sandbox() {
        let cli = parse(&["--sandbox", "--no-mixins"]);
        assert!(cli.no_mixins);
    }

    #[test]
    fn parse_sandbox_with_fresh_and_no_mixins() {
        let cli = parse(&["--sandbox", "foo", "--fresh", "--no-mixins"]);
        assert_eq!(cli.sandbox, Some(Some("foo".to_string())));
        assert!(cli.fresh);
        assert!(cli.no_mixins);
    }
}
