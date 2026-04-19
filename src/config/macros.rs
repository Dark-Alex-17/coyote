use crate::config::paths;
use crate::config::{Config, RequestContext, RoleLike, ensure_parent_exists};
use crate::repl::{run_repl_command, split_args_text};
use crate::utils::{AbortSignal, multiline_text};
use anyhow::{Result, anyhow};
use indexmap::IndexMap;
use rust_embed::Embed;
use serde::Deserialize;
use std::fs::File;
use std::io::Write;

#[derive(Embed)]
#[folder = "assets/macros"]
struct MacroAssets;

#[async_recursion::async_recursion]
pub async fn macro_execute(
    ctx: &mut RequestContext,
    name: &str,
    args: Option<&str>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let macro_value = Config::load_macro(name)?;
    let (mut new_args, text) = split_args_text(args.unwrap_or_default(), cfg!(windows));
    if !text.is_empty() {
        new_args.push(text.to_string());
    }
    let variables = macro_value
        .resolve_variables(&new_args)
        .map_err(|err| anyhow!("{err}. Usage: {}", macro_value.usage(name)))?;
    let role = ctx.extract_role(ctx.app.config.as_ref());
    let mut app_config = (*ctx.app.config).clone();
    app_config.temperature = role.temperature();
    app_config.top_p = role.top_p();
    app_config.enabled_tools = role.enabled_tools().clone();
    app_config.enabled_mcp_servers = role.enabled_mcp_servers().clone();

    let mut app_state = (*ctx.app).clone();
    app_state.config = std::sync::Arc::new(app_config);

    let mut macro_ctx = RequestContext::new(std::sync::Arc::new(app_state), ctx.working_mode);
    macro_ctx.macro_flag = true;
    macro_ctx.info_flag = ctx.info_flag;
    macro_ctx.model = role.model().clone();
    macro_ctx.agent_variables = ctx.agent_variables.clone();
    macro_ctx.last_message = ctx.last_message.clone();
    macro_ctx.supervisor = ctx.supervisor.clone();
    macro_ctx.parent_supervisor = ctx.parent_supervisor.clone();
    macro_ctx.self_agent_id = ctx.self_agent_id.clone();
    macro_ctx.inbox = ctx.inbox.clone();
    macro_ctx.escalation_queue = ctx.escalation_queue.clone();
    macro_ctx.current_depth = ctx.current_depth;
    macro_ctx.auto_continue_count = ctx.auto_continue_count;
    macro_ctx.todo_list = ctx.todo_list.clone();
    macro_ctx.tool_scope.tool_tracker = ctx.tool_scope.tool_tracker.clone();
    macro_ctx.discontinuous_last_message();

    let app = macro_ctx.app.config.clone();
    macro_ctx
        .bootstrap_tools(app.as_ref(), true, abort_signal.clone())
        .await?;

    for step in &macro_value.steps {
        let command = Macro::interpolate_command(step, &variables);
        println!(">> {}", multiline_text(&command));
        run_repl_command(&mut macro_ctx, abort_signal.clone(), &command).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Macro {
    #[serde(default)]
    pub variables: Vec<MacroVariable>,
    pub steps: Vec<String>,
}

impl Macro {
    pub fn install_macros() -> Result<()> {
        info!(
            "Installing built-in macros in {}",
            paths::macros_dir().display()
        );

        for file in MacroAssets::iter() {
            debug!("Processing macro file: {}", file.as_ref());
            let embedded_file = MacroAssets::get(&file)
                .ok_or_else(|| anyhow!("Failed to load embedded macro file: {}", file.as_ref()))?;
            let content = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
            let file_path = paths::macros_dir().join(file.as_ref());

            if file_path.exists() {
                debug!(
                    "Macro file already exists, skipping: {}",
                    file_path.display()
                );
                continue;
            }

            ensure_parent_exists(&file_path)?;
            info!("Creating macro file: {}", file_path.display());
            let mut macro_file = File::create(&file_path)?;
            macro_file.write_all(content.as_bytes())?;
        }

        Ok(())
    }

    pub fn resolve_variables(&self, args: &[String]) -> Result<IndexMap<String, String>> {
        let mut output = IndexMap::new();
        for (i, variable) in self.variables.iter().enumerate() {
            let value = if variable.rest && i == self.variables.len() - 1 {
                if args.len() > i {
                    Some(args[i..].join(" "))
                } else {
                    variable.default.clone()
                }
            } else {
                args.get(i)
                    .map(|v| v.to_string())
                    .or_else(|| variable.default.clone())
            };
            let value =
                value.ok_or_else(|| anyhow!("Missing value for variable '{}'", variable.name))?;
            output.insert(variable.name.clone(), value);
        }
        Ok(output)
    }

    pub fn usage(&self, name: &str) -> String {
        let mut parts = vec![name.to_string()];
        for (i, variable) in self.variables.iter().enumerate() {
            let part = match (
                variable.rest && i == self.variables.len() - 1,
                variable.default.is_some(),
            ) {
                (true, true) => format!("[{}]...", variable.name),
                (true, false) => format!("<{}>...", variable.name),
                (false, true) => format!("[{}]", variable.name),
                (false, false) => format!("<{}>", variable.name),
            };
            parts.push(part);
        }
        parts.join(" ")
    }

    pub fn interpolate_command(command: &str, variables: &IndexMap<String, String>) -> String {
        let mut output = command.to_string();
        for (key, value) in variables {
            output = output.replace(&format!("{{{{{key}}}}}"), value);
        }
        output
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MacroVariable {
    pub name: String,
    #[serde(default)]
    pub rest: bool,
    pub default: Option<String>,
}
