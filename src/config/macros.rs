use crate::config::{Config, GlobalConfig, RoleLike};
use crate::repl::{run_repl_command, split_args_text};
use crate::utils::{multiline_text, AbortSignal};
use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::Deserialize;
use std::sync::Arc;

#[async_recursion::async_recursion]
pub async fn macro_execute(
    config: &GlobalConfig,
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
    let role = config.read().extract_role();
    let mut config = config.read().clone();
    config.temperature = role.temperature();
    config.top_p = role.top_p();
    config.enabled_tools = role.enabled_tools().clone();
    config.enabled_mcp_servers = role.enabled_mcp_servers().clone();
    config.macro_flag = true;
    config.model = role.model().clone();
    config.role = None;
    config.session = None;
    config.rag = None;
    config.agent = None;
    config.discontinuous_last_message();
    let config = Arc::new(RwLock::new(config));
    config.write().macro_flag = true;
    for step in &macro_value.steps {
        let command = Macro::interpolate_command(step, &variables);
        println!(">> {}", multiline_text(&command));
        run_repl_command(&config, abort_signal.clone(), &command).await?;
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
