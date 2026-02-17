mod cli;
mod client;
mod config;
mod function;
mod rag;
mod render;
mod repl;
#[macro_use]
mod utils;
mod mcp;
mod parsers;
mod supervisor;
mod vault;

#[macro_use]
extern crate log;

use crate::client::{
    ModelType, call_chat_completions, call_chat_completions_streaming, list_models,
};
use crate::config::{
    Agent, CODE_ROLE, Config, EXPLAIN_SHELL_ROLE, GlobalConfig, Input, SHELL_ROLE,
    TEMP_SESSION_NAME, WorkingMode, ensure_parent_exists, list_agents, load_env_file,
    macro_execute,
};
use crate::render::render_error;
use crate::repl::Repl;
use crate::utils::*;

use crate::cli::Cli;
use crate::vault::Vault;
use anyhow::{Result, bail};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use inquire::Text;
use log::LevelFilter;
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::{env, mem, process, sync::Arc};

#[tokio::main]
async fn main() -> Result<()> {
    load_env_file()?;
    CompleteEnv::with_factory(Cli::command).complete();
    let cli = Cli::parse();

    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        shell.generate_completions(&mut cmd);
        return Ok(());
    }
    if cli.tail_logs {
        tail_logs(cli.disable_log_colors).await;
        return Ok(());
    }

    let text = cli.text()?;
    let working_mode = if text.is_none() && cli.file.is_empty() {
        WorkingMode::Repl
    } else {
        WorkingMode::Cmd
    };

    let info_flag = cli.info
        || cli.sync_models
        || cli.list_models
        || cli.list_roles
        || cli.list_agents
        || cli.list_rags
        || cli.list_macros
        || cli.list_sessions;
    let vault_flags = cli.add_secret.is_some()
        || cli.get_secret.is_some()
        || cli.update_secret.is_some()
        || cli.delete_secret.is_some()
        || cli.list_secrets;

    let log_path = setup_logger()?;

    if vault_flags {
        return Vault::handle_vault_flags(cli, Config::init_bare()?);
    }

    let abort_signal = create_abort_signal();
    let start_mcp_servers = cli.agent.is_none() && cli.role.is_none();
    let config = Arc::new(RwLock::new(
        Config::init(
            working_mode,
            info_flag,
            start_mcp_servers,
            log_path,
            abort_signal.clone(),
        )
        .await?,
    ));
    if let Err(err) = run(config, cli, text, abort_signal).await {
        render_error(err);
        process::exit(1);
    }
    Ok(())
}

async fn run(
    config: GlobalConfig,
    cli: Cli,
    text: Option<String>,
    abort_signal: AbortSignal,
) -> Result<()> {
    if cli.sync_models {
        let url = config.read().sync_models_url();
        return Config::sync_models(&url, abort_signal.clone()).await;
    }

    if cli.list_models {
        for model in list_models(&config.read(), ModelType::Chat) {
            println!("{}", model.id());
        }
        return Ok(());
    }
    if cli.list_roles {
        let roles = Config::list_roles(true).join("\n");
        println!("{roles}");
        return Ok(());
    }
    if cli.list_agents {
        let agents = list_agents().join("\n");
        println!("{agents}");
        return Ok(());
    }
    if cli.list_rags {
        let rags = Config::list_rags().join("\n");
        println!("{rags}");
        return Ok(());
    }
    if cli.list_macros {
        let macros = Config::list_macros().join("\n");
        println!("{macros}");
        return Ok(());
    }

    if cli.dry_run {
        config.write().dry_run = true;
    }

    if let Some(agent) = &cli.agent {
        if cli.build_tools {
            info!("Building tools for agent '{agent}'...");
            Agent::init(&config, agent, abort_signal.clone()).await?;
            return Ok(());
        }

        let session = cli.session.as_ref().map(|v| match v {
            Some(v) => v.as_str(),
            None => TEMP_SESSION_NAME,
        });
        if !cli.agent_variable.is_empty() {
            config.write().agent_variables = Some(
                cli.agent_variable
                    .chunks(2)
                    .map(|v| (v[0].to_string(), v[1].to_string()))
                    .collect(),
            );
        }

        let ret = Config::use_agent(&config, agent, session, abort_signal.clone()).await;
        config.write().agent_variables = None;
        ret?;
    } else {
        if let Some(prompt) = &cli.prompt {
            config.write().use_prompt(prompt)?;
        } else if let Some(name) = &cli.role {
            Config::use_role_safely(&config, name, abort_signal.clone()).await?;
        } else if cli.execute {
            Config::use_role_safely(&config, SHELL_ROLE, abort_signal.clone()).await?;
        } else if cli.code {
            Config::use_role_safely(&config, CODE_ROLE, abort_signal.clone()).await?;
        }
        if let Some(session) = &cli.session {
            Config::use_session_safely(
                &config,
                session.as_ref().map(|v| v.as_str()),
                abort_signal.clone(),
            )
            .await?;
        }
        if let Some(rag) = &cli.rag {
            Config::use_rag(&config, Some(rag), abort_signal.clone()).await?;
        }
    }

    if cli.build_tools {
        return Ok(());
    }

    if cli.list_sessions {
        let sessions = config.read().list_sessions().join("\n");
        println!("{sessions}");
        return Ok(());
    }
    if let Some(model_id) = &cli.model {
        config.write().set_model(model_id)?;
    }
    if cli.no_stream {
        config.write().stream = false;
    }
    if cli.empty_session {
        config.write().empty_session()?;
    }
    if cli.save_session {
        config.write().set_save_session_this_time()?;
    }
    if cli.info {
        let info = config.read().info()?;
        println!("{info}");
        return Ok(());
    }
    let is_repl = config.read().working_mode.is_repl();
    if cli.rebuild_rag {
        Config::rebuild_rag(&config, abort_signal.clone()).await?;
        if is_repl {
            return Ok(());
        }
    }
    if let Some(name) = &cli.macro_name {
        macro_execute(&config, name, text.as_deref(), abort_signal.clone()).await?;
        return Ok(());
    }
    if cli.execute && !is_repl {
        let input = create_input(&config, text, &cli.file, abort_signal.clone()).await?;
        shell_execute(&config, &SHELL, input, abort_signal.clone()).await?;
        return Ok(());
    }

    apply_prelude_safely(&config, abort_signal.clone()).await?;

    match is_repl {
        false => {
            let mut input = create_input(&config, text, &cli.file, abort_signal.clone()).await?;
            input.use_embeddings(abort_signal.clone()).await?;
            start_directive(&config, input, cli.code, abort_signal).await
        }
        true => {
            if !*IS_STDOUT_TERMINAL {
                bail!("No TTY for REPL")
            }
            start_interactive(&config).await
        }
    }
}

async fn apply_prelude_safely(config: &RwLock<Config>, abort_signal: AbortSignal) -> Result<()> {
    let mut cfg = {
        let mut guard = config.write();
        mem::take(&mut *guard)
    };

    cfg.apply_prelude(abort_signal.clone()).await?;

    {
        let mut guard = config.write();
        *guard = cfg;
    }

    Ok(())
}

#[async_recursion::async_recursion]
async fn start_directive(
    config: &GlobalConfig,
    input: Input,
    code_mode: bool,
    abort_signal: AbortSignal,
) -> Result<()> {
    let client = input.create_client()?;
    let extract_code = !*IS_STDOUT_TERMINAL && code_mode;
    config.write().before_chat_completion(&input)?;
    let (output, tool_results) = if !input.stream() || extract_code {
        call_chat_completions(
            &input,
            true,
            extract_code,
            client.as_ref(),
            abort_signal.clone(),
        )
        .await?
    } else {
        call_chat_completions_streaming(&input, client.as_ref(), abort_signal.clone()).await?
    };
    config
        .write()
        .after_chat_completion(&input, &output, &tool_results)?;

    if !tool_results.is_empty() {
        start_directive(
            config,
            input.merge_tool_results(output, tool_results),
            code_mode,
            abort_signal,
        )
        .await?;
    }

    config.write().exit_session()?;
    Ok(())
}

async fn start_interactive(config: &GlobalConfig) -> Result<()> {
    let mut repl: Repl = Repl::init(config)?;
    repl.run().await
}

#[async_recursion::async_recursion]
async fn shell_execute(
    config: &GlobalConfig,
    shell: &Shell,
    mut input: Input,
    abort_signal: AbortSignal,
) -> Result<()> {
    let client = input.create_client()?;
    config.write().before_chat_completion(&input)?;
    let (eval_str, _) =
        call_chat_completions(&input, false, true, client.as_ref(), abort_signal.clone()).await?;

    config
        .write()
        .after_chat_completion(&input, &eval_str, &[])?;
    if eval_str.is_empty() {
        bail!("No command generated");
    }
    if config.read().dry_run {
        config.read().print_markdown(&eval_str)?;
        return Ok(());
    }
    if *IS_STDOUT_TERMINAL {
        let options = ["execute", "revise", "describe", "copy", "quit"];
        let command = color_text(eval_str.trim(), nu_ansi_term::Color::Rgb(255, 165, 0));
        let first_letter_color = nu_ansi_term::Color::Cyan;
        let prompt_text = options
            .iter()
            .map(|v| format!("{}{}", color_text(&v[0..1], first_letter_color), &v[1..]))
            .collect::<Vec<String>>()
            .join(&dimmed_text(" | "));
        loop {
            println!("{command}");
            let answer_char =
                read_single_key(&['e', 'r', 'd', 'c', 'q'], 'e', &format!("{prompt_text}: "))?;

            match answer_char {
                'e' => {
                    debug!("{} {:?}", shell.cmd, &[&shell.arg, &eval_str]);
                    let code = run_command(&shell.cmd, &[&shell.arg, &eval_str], None)?;
                    if code == 0 && config.read().save_shell_history {
                        let _ = append_to_shell_history(&shell.name, &eval_str, code);
                    }
                    process::exit(code);
                }
                'r' => {
                    let revision = Text::new("Enter your revision:").prompt()?;
                    let text = format!("{}\n{revision}", input.text());
                    input.set_text(text);
                    return shell_execute(config, shell, input, abort_signal.clone()).await;
                }
                'd' => {
                    let role = config.read().retrieve_role(EXPLAIN_SHELL_ROLE)?;
                    let input = Input::from_str(config, &eval_str, Some(role));
                    if input.stream() {
                        call_chat_completions_streaming(
                            &input,
                            client.as_ref(),
                            abort_signal.clone(),
                        )
                        .await?;
                    } else {
                        call_chat_completions(
                            &input,
                            true,
                            false,
                            client.as_ref(),
                            abort_signal.clone(),
                        )
                        .await?;
                    }
                    println!();
                    continue;
                }
                'c' => {
                    set_text(&eval_str)?;
                    println!("{}", dimmed_text("✓ Copied the command."));
                }
                _ => {}
            }
            break;
        }
    } else {
        println!("{eval_str}");
    }
    Ok(())
}

async fn create_input(
    config: &GlobalConfig,
    text: Option<String>,
    file: &[String],
    abort_signal: AbortSignal,
) -> Result<Input> {
    let input = if file.is_empty() {
        Input::from_str(config, &text.unwrap_or_default(), None)
    } else {
        Input::from_files_with_spinner(
            config,
            &text.unwrap_or_default(),
            file.to_vec(),
            None,
            abort_signal,
        )
        .await?
    };
    if input.is_empty() {
        bail!("No input");
    }
    Ok(input)
}

fn setup_logger() -> Result<Option<PathBuf>> {
    let (log_level, log_path) = Config::log_config()?;
    if log_level == LevelFilter::Off {
        return Ok(None);
    }
    let encoder = Box::new(PatternEncoder::new(
        "{d(%Y-%m-%d %H:%M:%S%.3f)(utc)} <{i}> [{l}] {f}:{L} - {m}{n}",
    ));
    let log_filter = env::var(get_env_name("log_filter")).ok();
    match log_path.clone() {
        None => {
            let console_appender = ConsoleAppender::builder().encoder(encoder).build();
            log4rs::init_config(init_console_logger(log_level, log_filter, console_appender))?;
        }
        Some(path) => {
            ensure_parent_exists(&path)?;
            let file_appender = FileAppender::builder().encoder(encoder.clone()).build(path);

            match file_appender {
                Ok(appender) => {
                    log4rs::init_config(init_file_logger(log_level, log_filter, appender))?
                }
                Err(_) => {
                    let console_appender = ConsoleAppender::builder().encoder(encoder).build();
                    log4rs::init_config(init_console_logger(
                        log_level,
                        log_filter,
                        console_appender,
                    ))?
                }
            };
        }
    }
    Ok(log_path)
}

fn init_file_logger(
    log_level: LevelFilter,
    log_filter: Option<String>,
    file_appender: FileAppender,
) -> log4rs::Config {
    let root_log_level = if log_filter.is_some() {
        LevelFilter::Off
    } else {
        log_level
    };
    let mut config_builder = log4rs::Config::builder()
        .appender(Appender::builder().build("logfile", Box::new(file_appender)));

    if let Some(filter) = log_filter {
        config_builder = config_builder.logger(Logger::builder().build(filter, log_level));
    }

    config_builder
        .build(Root::builder().appender("logfile").build(root_log_level))
        .unwrap()
}

fn init_console_logger(
    log_level: LevelFilter,
    log_filter: Option<String>,
    console_appender: ConsoleAppender,
) -> log4rs::Config {
    let root_log_level = if log_filter.is_some() {
        LevelFilter::Off
    } else {
        log_level
    };
    let mut config_builder = log4rs::Config::builder()
        .appender(Appender::builder().build("console", Box::new(console_appender)));

    if let Some(filter) = log_filter {
        config_builder = config_builder.logger(Logger::builder().build(filter, log_level));
    }

    config_builder
        .build(Root::builder().appender("console").build(root_log_level))
        .unwrap()
}
