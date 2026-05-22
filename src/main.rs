mod cli;
mod client;
mod config;
mod function;
mod graph;
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

use crate::cli::Cli;
use crate::client::{
    ModelType, call_chat_completions, call_chat_completions_streaming, list_models, oauth,
};
use crate::config::paths;
use crate::config::{
    Agent, AppConfig, AppState, CODE_ROLE, Config, EXPLAIN_SHELL_ROLE, Input, RequestContext,
    SHELL_ROLE, TEMP_SESSION_NAME, WorkingMode, ensure_parent_exists, install_builtins,
    list_agents, load_env_file, macro_execute, sync_models,
};
use crate::render::{prompt_theme, render_error};
use crate::repl::Repl;
use crate::utils::*;
use crate::vault::Vault;
use anyhow::{Result, anyhow, bail};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use client::ClientConfig;
use inquire::{Select, Text, set_global_render_config};
use log::LevelFilter;
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;
use oauth::OAuthProvider;
use std::path::PathBuf;
use std::{env, process, sync::Arc};

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

    if let Some(version) = &cli.update {
        let version = version.clone();
        let force = cli.force;
        return tokio::task::spawn_blocking(move || config::run_self_update(version, force))
            .await?;
    }

    install_builtins()?;

    if let Some(client_arg) = &cli.authenticate {
        let cfg = Config::load_with_interpolation(true).await?;
        let app_config = AppConfig::from_config(cfg)?;
        let (client_name, provider) =
            resolve_oauth_client(client_arg.as_deref(), &app_config.clients)?;
        oauth::run_oauth_flow(&*provider, &client_name).await?;
        return Ok(());
    }

    if vault_flags {
        let cfg = Config::load_with_interpolation(true).await?;
        let app_config = AppConfig::from_config(cfg)?;
        let vault = Vault::init(&app_config);
        return Vault::handle_vault_flags(cli, &vault);
    }

    let abort_signal = create_abort_signal();
    let start_mcp_servers = cli.agent.is_none() && cli.role.is_none();
    let cfg = Config::load_with_interpolation(info_flag).await?;
    let app_config: Arc<AppConfig> = Arc::new(AppConfig::from_config(cfg)?);
    let app_state: Arc<AppState> = Arc::new(
        AppState::init(
            app_config,
            log_path,
            start_mcp_servers,
            abort_signal.clone(),
        )
        .await?,
    );
    let ctx = RequestContext::bootstrap(app_state, working_mode, info_flag)?;

    {
        let app = &*ctx.app.config;
        if app.highlight {
            set_global_render_config(prompt_theme(app.render_options()?)?)
        }
    }

    if let Err(err) = run(ctx, cli, text, abort_signal).await {
        render_error(err);
        process::exit(1);
    }
    Ok(())
}

fn update_app_config(ctx: &mut RequestContext, update: impl FnOnce(&mut AppConfig)) {
    let mut app_config = (*ctx.app.config).clone();
    update(&mut app_config);

    let mut app_state = (*ctx.app).clone();
    app_state.config = Arc::new(app_config);
    ctx.app = Arc::new(app_state);
}

async fn run(
    mut ctx: RequestContext,
    cli: Cli,
    text: Option<String>,
    abort_signal: AbortSignal,
) -> Result<()> {
    if let Some(category) = cli.install {
        return config::install_assets(category);
    }

    if let Some(url) = cli.install_from.as_deref() {
        return config::install_remote(url, cli.filter, cli.install_force);
    }

    if cli.sync_models {
        let url = ctx.app.config.sync_models_url();
        return sync_models(&url, abort_signal.clone()).await;
    }

    if cli.list_models {
        for model in list_models(ctx.app.config.as_ref(), ModelType::Chat) {
            println!("{}", model.id());
        }
        return Ok(());
    }
    if cli.list_roles {
        let roles = paths::list_roles(true).join("\n");
        println!("{roles}");
        return Ok(());
    }
    if cli.list_agents {
        let agents = list_agents().join("\n");
        println!("{agents}");
        return Ok(());
    }
    if cli.list_rags {
        let rags = paths::list_rags().join("\n");
        println!("{rags}");
        return Ok(());
    }
    if cli.list_macros {
        let macros = paths::list_macros().join("\n");
        println!("{macros}");
        return Ok(());
    }

    if cli.dry_run {
        update_app_config(&mut ctx, |app| app.dry_run = true);
    }

    if let Some(agent) = &cli.agent {
        if cli.build_tools {
            info!("Building tools for agent '{agent}'...");
            Agent::init(
                &ctx.app.config,
                &ctx.app,
                &ctx.model,
                ctx.info_flag,
                agent,
                abort_signal.clone(),
            )
            .await?;
            return Ok(());
        }

        let session = cli.session.as_ref().map(|v| match v {
            Some(v) => v.as_str(),
            None => TEMP_SESSION_NAME,
        });
        if !cli.agent_variable.is_empty() {
            ctx.agent_variables = Some(
                cli.agent_variable
                    .chunks(2)
                    .map(|v| (v[0].to_string(), v[1].to_string()))
                    .collect(),
            );
        }
        let app = Arc::clone(&ctx.app.config);
        ctx.use_agent(app.as_ref(), agent, session, abort_signal.clone())
            .await?;
    } else {
        let app: Arc<AppConfig> = Arc::clone(&ctx.app.config);
        if let Some(prompt) = &cli.prompt {
            ctx.use_prompt(app.as_ref(), prompt)?;
        } else if let Some(name) = &cli.role {
            ctx.use_role(app.as_ref(), name, abort_signal.clone())
                .await?;
        } else if cli.execute {
            ctx.use_role(app.as_ref(), SHELL_ROLE, abort_signal.clone())
                .await?;
        } else if cli.code {
            ctx.use_role(app.as_ref(), CODE_ROLE, abort_signal.clone())
                .await?;
        }
        if let Some(session) = &cli.session {
            ctx.use_session(
                app.as_ref(),
                session.as_ref().map(|v| v.as_str()),
                abort_signal.clone(),
            )
            .await?;
        }
        if let Some(rag) = &cli.rag {
            ctx.use_rag(Some(rag), abort_signal.clone()).await?;
        }
    }

    if cli.build_tools {
        return Ok(());
    }

    if cli.list_sessions {
        let sessions = ctx.list_sessions().join("\n");
        println!("{sessions}");
        return Ok(());
    }
    if let Some(model_id) = &cli.model {
        let app: Arc<AppConfig> = Arc::clone(&ctx.app.config);
        ctx.set_model_on_role_like(app.as_ref(), model_id)?;
    }
    if cli.no_stream {
        update_app_config(&mut ctx, |app| app.stream = false);
    }
    if cli.empty_session {
        ctx.empty_session()?;
    }
    if cli.save_session {
        ctx.set_save_session_this_time()?;
    }
    if cli.info {
        let app: Arc<AppConfig> = Arc::clone(&ctx.app.config);
        let info = ctx.info(app.as_ref())?;
        println!("{info}");
        return Ok(());
    }
    let is_repl = ctx.working_mode.is_repl();
    if cli.rebuild_rag {
        ctx.rebuild_rag(abort_signal.clone()).await?;
        if is_repl {
            return Ok(());
        }
    }
    if let Some(name) = &cli.macro_name {
        macro_execute(&mut ctx, name, text.as_deref(), abort_signal.clone()).await?;
        return Ok(());
    }
    if cli.execute && !is_repl {
        let input = create_input(&ctx, text, &cli.file, abort_signal.clone()).await?;
        shell_execute(&mut ctx, &SHELL, input, abort_signal.clone()).await?;
        return Ok(());
    }

    {
        let app: Arc<AppConfig> = Arc::clone(&ctx.app.config);
        ctx.apply_prelude(app.as_ref(), abort_signal.clone())
            .await?;
    }

    match is_repl {
        false => {
            let mut input = create_input(&ctx, text, &cli.file, abort_signal.clone()).await?;
            input.use_embeddings(abort_signal.clone()).await?;
            start_directive(&mut ctx, input, cli.code, abort_signal).await
        }
        true => {
            if !*IS_STDOUT_TERMINAL {
                bail!("No TTY for REPL")
            }
            start_interactive(ctx).await
        }
    }
}

#[async_recursion::async_recursion]
async fn start_directive(
    ctx: &mut RequestContext,
    input: Input,
    code_mode: bool,
    abort_signal: AbortSignal,
) -> Result<()> {
    let app: Arc<AppConfig> = Arc::clone(&ctx.app.config);

    if graph::active_agent_graph_name(ctx).is_some() {
        ctx.before_chat_completion(&input)?;
        let output =
            graph::run_active_agent_graph(ctx, &input.text(), abort_signal.clone()).await?;
        app.print_markdown(&output)?;
        ctx.after_chat_completion(app.as_ref(), &input, &output, &[])?;
        ctx.exit_session()?;
        return Ok(());
    }

    let client = input.create_client()?;
    let extract_code = !*IS_STDOUT_TERMINAL && code_mode;
    ctx.before_chat_completion(&input)?;
    let (output, tool_results) = if !input.stream() || extract_code {
        call_chat_completions(
            &input,
            true,
            extract_code,
            client.as_ref(),
            ctx,
            abort_signal.clone(),
        )
        .await?
    } else {
        call_chat_completions_streaming(&input, client.as_ref(), ctx, abort_signal.clone()).await?
    };
    ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;

    if !tool_results.is_empty() {
        start_directive(
            ctx,
            input.merge_tool_results(output, tool_results),
            code_mode,
            abort_signal,
        )
        .await?;
    }

    ctx.exit_session()?;
    Ok(())
}

async fn start_interactive(ctx: RequestContext) -> Result<()> {
    let mut repl: Repl = Repl::init(ctx)?;
    repl.run().await
}

#[async_recursion::async_recursion]
async fn shell_execute(
    ctx: &mut RequestContext,
    shell: &Shell,
    mut input: Input,
    abort_signal: AbortSignal,
) -> Result<()> {
    let app: Arc<AppConfig> = Arc::clone(&ctx.app.config);
    let client = input.create_client()?;
    ctx.before_chat_completion(&input)?;
    let (eval_str, _) = call_chat_completions(
        &input,
        false,
        true,
        client.as_ref(),
        ctx,
        abort_signal.clone(),
    )
    .await?;

    ctx.after_chat_completion(app.as_ref(), &input, &eval_str, &[])?;
    if eval_str.is_empty() {
        bail!("No command generated");
    }
    if app.dry_run {
        app.print_markdown(&eval_str)?;
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
                    if code == 0 && app.save_shell_history {
                        let _ = append_to_shell_history(&shell.name, &eval_str, code);
                    }
                    process::exit(code);
                }
                'r' => {
                    let revision = Text::new("Enter your revision:").prompt()?;
                    let text = format!("{}\n{revision}", input.text());
                    input.set_text(text);
                    return shell_execute(ctx, shell, input, abort_signal.clone()).await;
                }
                'd' => {
                    let role = ctx.retrieve_role(app.as_ref(), EXPLAIN_SHELL_ROLE)?;
                    let input = Input::from_str(ctx, &eval_str, Some(role));
                    if input.stream() {
                        call_chat_completions_streaming(
                            &input,
                            client.as_ref(),
                            ctx,
                            abort_signal.clone(),
                        )
                        .await?;
                    } else {
                        call_chat_completions(
                            &input,
                            true,
                            false,
                            client.as_ref(),
                            ctx,
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
    ctx: &RequestContext,
    text: Option<String>,
    file: &[String],
    abort_signal: AbortSignal,
) -> Result<Input> {
    let text = text.unwrap_or_default();
    let input = if file.is_empty() {
        Input::from_str(ctx, &text, None)
    } else {
        Input::from_files_with_spinner(ctx, &text, file.to_vec(), None, abort_signal).await?
    };
    if input.is_empty() {
        bail!("No input");
    }
    Ok(input)
}

fn setup_logger() -> Result<Option<PathBuf>> {
    let (log_level, log_path) = paths::log_config()?;
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

fn resolve_oauth_client(
    explicit: Option<&str>,
    clients: &[ClientConfig],
) -> Result<(String, Box<dyn OAuthProvider>)> {
    if let Some(name) = explicit {
        let provider_type = oauth::resolve_provider_type(name, clients)
            .ok_or_else(|| anyhow!("Client '{name}' not found or doesn't support OAuth"))?;
        let provider = oauth::get_oauth_provider(provider_type).unwrap();
        return Ok((name.to_string(), provider));
    }

    let candidates = oauth::list_oauth_capable_clients(clients);
    match candidates.len() {
        0 => bail!("No OAuth-capable clients configured."),
        1 => {
            let name = &candidates[0];
            let provider_type = oauth::resolve_provider_type(name, clients).unwrap();
            let provider = oauth::get_oauth_provider(provider_type).unwrap();
            Ok((name.clone(), provider))
        }
        _ => {
            let choice =
                Select::new("Select a client to authenticate:", candidates.clone()).prompt()?;
            let provider_type = oauth::resolve_provider_type(&choice, clients).unwrap();
            let provider = oauth::get_oauth_provider(provider_type).unwrap();
            Ok((choice, provider))
        }
    }
}
