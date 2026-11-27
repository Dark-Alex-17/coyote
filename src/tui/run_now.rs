use crate::config::RequestContext;
use crate::repl::{
    REASONING_USAGE, SET_USAGE, edit_skill_output, info_output, list_output, parse_command,
    repl_help_text, unfence,
};
use crate::utils::{AbortSignal, create_abort_signal};
use anyhow::{Result, anyhow, bail};

/// What a command run beside a turn leaves to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandOutput {
    /// What the REPL prints for it.
    Text(String),
    /// A setting the snapshot took, which the live context takes when
    /// the turn ends.
    Deferred,
}

/// Runs a line the timing table marks `Now` or `Deferred` on the context
/// the turn started with. A `Now` line returns what the REPL prints for
/// it; a `Deferred` one changes the snapshot only, which checks it the
/// way an idle run would. Nothing here may print: while a turn runs, fds
/// 1 and 2 feed that turn's capture. `abort` is the command's own, which
/// Esc and a quit trip.
pub(super) async fn run_now(
    ctx: &mut RequestContext,
    line: &str,
    abort: AbortSignal,
) -> Result<CommandOutput> {
    let prefix = ctx.command_prefix();
    let Some((cmd, args)) = parse_command(unfence(line).trim(), prefix) else {
        bail!("Not a command: {line}");
    };
    if let Some(usage) = setting_usage(&cmd) {
        return match setting_args(&cmd, args) {
            Some(args) => {
                ctx.update(&args, abort).await?;
                Ok(CommandOutput::Deferred)
            }
            None => Ok(CommandOutput::Text(format!(
                "{}\n",
                ctx.render_hints(usage)
            ))),
        };
    }
    let ctx = &*ctx;
    let text = match cmd.as_ref() {
        ".help" => format!("{}\n", repl_help_text(prefix)),
        ".info" => info_output(ctx, args).await?,
        ".list" => list_output(ctx, args).await?,
        ".sources" => match args {
            Some("rag") => format!("{}\n", ctx.rag_sources()?),
            _ => format!("{}\n", ctx.render_hints("Usage: .sources rag")),
        },
        ".skill" if args.and_then(|args| args.split_whitespace().next()) == Some("loaded") => {
            ctx.loaded_skills_listing()
        }
        ".edit" if ctx.macro_flag => {
            bail!("Cannot perform this operation because you are in a macro")
        }
        ".edit" => match args {
            Some("config") => ctx.edit_config()?,
            Some("mcp-config") => ctx.edit_mcp_config()?,
            Some("agent-config") => ctx.edit_agent_config(&ctx.app.config)?,
            Some(args) if args == "skill" || args.starts_with("skill ") => {
                edit_skill_output(ctx, args)?
            }
            _ => bail!("{} cannot run during a turn", line.trim()),
        },
        _ => bail!("{} cannot run during a turn", line.trim()),
    };
    Ok(CommandOutput::Text(text))
}

/// Applies settings taken during a turn to the context the turn handed
/// back, in the order they were typed and by the path an idle `.set`
/// takes. Returns each line that failed with its error; a line that no
/// longer reads as a setting (the prefix changed under it) fails too.
pub(super) async fn replay_deferred(
    ctx: &mut RequestContext,
    lines: Vec<String>,
) -> Vec<(String, anyhow::Error)> {
    let mut failed = Vec::new();
    for line in lines {
        let parsed = parse_command(unfence(&line).trim(), ctx.command_prefix());
        let Some(args) = parsed.and_then(|(cmd, args)| setting_args(&cmd, args)) else {
            failed.push((
                line,
                anyhow!("not a setting under the current command prefix"),
            ));
            continue;
        };
        if let Err(err) = ctx.update(&args, create_abort_signal()).await {
            failed.push((line, err));
        }
    }
    failed
}

/// The usage hint the REPL prints for a settings command given no
/// arguments, or `None` for any other command.
fn setting_usage(cmd: &str) -> Option<&'static str> {
    match cmd {
        ".set" => Some(SET_USAGE),
        ".reasoning" => Some(REASONING_USAGE),
        _ => None,
    }
}

/// The `.set` arguments a `.set` or `.reasoning` line stands for.
fn setting_args(cmd: &str, args: Option<&str>) -> Option<String> {
    match (cmd, args) {
        (".set", Some(args)) => Some(args.to_string()),
        (".reasoning", Some(level)) => Some(format!("reasoning_effort {level}")),
        _ => None,
    }
}
