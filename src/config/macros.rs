use crate::config::paths;
use crate::config::{RequestContext, RoleLike, ensure_parent_exists};
use crate::repl::{run_repl_command, split_args_text};
use crate::utils::{AbortSignal, multiline_text};
use anyhow::{Context, Result, anyhow};
use indexmap::IndexMap;
use rust_embed::Embed;
use serde::Deserialize;
use std::fs::{File, read_to_string};
use std::io::Write;
use std::sync::Arc;

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
    let macro_value = Macro::load(name)?;
    let (mut new_args, text) = split_args_text(args.unwrap_or_default(), cfg!(windows));
    if !text.is_empty() {
        new_args.push(text.to_string());
    }
    let variables = macro_value
        .resolve_variables(&new_args)
        .map_err(|err| anyhow!("{err}. Usage: {}", macro_value.usage(name)))?;
    let role = ctx.extract_role(ctx.app.config.as_ref())?;
    let mut app_config = (*ctx.app.config).clone();
    app_config.temperature = role.temperature();
    app_config.top_p = role.top_p();
    app_config.enabled_tools = role.enabled_tools();
    app_config.enabled_mcp_servers = role.enabled_mcp_servers();

    let mut app_state = (*ctx.app).clone();
    app_state.config = Arc::new(app_config);

    let mut macro_ctx = RequestContext::new(Arc::new(app_state), ctx.working_mode);
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
    pub fn load(name: &str) -> Result<Macro> {
        let path = paths::macro_file(name);
        let err = || format!("Failed to load macro '{name}' at '{}'", path.display());
        let content = read_to_string(&path).with_context(err)?;
        let value: Macro = serde_yaml::from_str(&content).with_context(err)?;
        Ok(value)
    }

    pub fn install_macros(force: bool) -> Result<()> {
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

            if file_path.exists() && !force {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, rest: bool, default: Option<&str>) -> MacroVariable {
        MacroVariable {
            name: name.to_string(),
            rest,
            default: default.map(String::from),
        }
    }

    fn macro_with_vars(vars: Vec<MacroVariable>) -> Macro {
        Macro {
            variables: vars,
            steps: vec![],
        }
    }

    #[test]
    fn resolve_no_variables() {
        let m = macro_with_vars(vec![]);
        let result = m.resolve_variables(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_required_variable_provided() {
        let m = macro_with_vars(vec![var("name", false, None)]);
        let result = m.resolve_variables(&["Alice".into()]).unwrap();
        assert_eq!(result["name"], "Alice");
    }

    #[test]
    fn resolve_required_variable_missing_errors() {
        let m = macro_with_vars(vec![var("name", false, None)]);
        let result = m.resolve_variables(&[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn resolve_default_variable_uses_default() {
        let m = macro_with_vars(vec![var("color", false, Some("blue"))]);
        let result = m.resolve_variables(&[]).unwrap();
        assert_eq!(result["color"], "blue");
    }

    #[test]
    fn resolve_default_variable_overridden() {
        let m = macro_with_vars(vec![var("color", false, Some("blue"))]);
        let result = m.resolve_variables(&["red".into()]).unwrap();
        assert_eq!(result["color"], "red");
    }

    #[test]
    fn resolve_rest_variable_captures_all_remaining() {
        let m = macro_with_vars(vec![var("first", false, None), var("rest", true, None)]);
        let result = m
            .resolve_variables(&["a".into(), "b".into(), "c".into()])
            .unwrap();
        assert_eq!(result["first"], "a");
        assert_eq!(result["rest"], "b c");
    }

    #[test]
    fn resolve_rest_variable_with_default() {
        let m = macro_with_vars(vec![var("args", true, Some("default text"))]);
        let result = m.resolve_variables(&[]).unwrap();
        assert_eq!(result["args"], "default text");
    }

    #[test]
    fn resolve_multiple_variables() {
        let m = macro_with_vars(vec![
            var("a", false, None),
            var("b", false, None),
            var("c", false, Some("default_c")),
        ]);
        let result = m.resolve_variables(&["x".into(), "y".into()]).unwrap();
        assert_eq!(result["a"], "x");
        assert_eq!(result["b"], "y");
        assert_eq!(result["c"], "default_c");
    }

    #[test]
    fn usage_no_variables() {
        let m = macro_with_vars(vec![]);
        assert_eq!(m.usage("my-macro"), "my-macro");
    }

    #[test]
    fn usage_required_variable() {
        let m = macro_with_vars(vec![var("name", false, None)]);
        assert_eq!(m.usage("greet"), "greet <name>");
    }

    #[test]
    fn usage_optional_variable() {
        let m = macro_with_vars(vec![var("color", false, Some("blue"))]);
        assert_eq!(m.usage("paint"), "paint [color]");
    }

    #[test]
    fn usage_rest_variable() {
        let m = macro_with_vars(vec![var("args", true, None)]);
        assert_eq!(m.usage("run"), "run <args>...");
    }

    #[test]
    fn usage_rest_with_default() {
        let m = macro_with_vars(vec![var("args", true, Some("default"))]);
        assert_eq!(m.usage("run"), "run [args]...");
    }

    #[test]
    fn usage_mixed_variables() {
        let m = macro_with_vars(vec![
            var("target", false, None),
            var("flags", true, Some("")),
        ]);
        assert_eq!(m.usage("build"), "build <target> [flags]...");
    }

    #[test]
    fn interpolate_replaces_variables() {
        let vars = IndexMap::from([("name".to_string(), "world".to_string())]);
        let result = Macro::interpolate_command("hello {{name}}", &vars);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn interpolate_multiple_variables() {
        let vars = IndexMap::from([
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]);
        let result = Macro::interpolate_command("{{a}} + {{b}}", &vars);
        assert_eq!(result, "1 + 2");
    }

    #[test]
    fn interpolate_no_variables_passthrough() {
        let vars = IndexMap::new();
        let result = Macro::interpolate_command("no vars here", &vars);
        assert_eq!(result, "no vars here");
    }

    #[test]
    fn interpolate_variable_not_found_left_as_is() {
        let vars = IndexMap::new();
        let result = Macro::interpolate_command("hello {{missing}}", &vars);
        assert_eq!(result, "hello {{missing}}");
    }

    #[test]
    fn deserialize_macro_from_yaml() {
        let yaml = r#"
steps:
  - ".role coder"
  - "write code for {{task}}"
variables:
  - name: task
"#;
        let m: Macro = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m.steps.len(), 2);
        assert_eq!(m.variables.len(), 1);
        assert_eq!(m.variables[0].name, "task");
        assert!(!m.variables[0].rest);
        assert!(m.variables[0].default.is_none());
    }

    #[test]
    fn deserialize_macro_with_defaults() {
        let yaml = r#"
steps:
  - "test"
variables:
  - name: mode
    default: "fast"
  - name: args
    rest: true
    default: "none"
"#;
        let m: Macro = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(m.variables[0].default, Some("fast".to_string()));
        assert!(m.variables[1].rest);
        assert_eq!(m.variables[1].default, Some("none".to_string()));
    }

    #[test]
    fn deserialize_macro_no_variables() {
        let yaml = r#"
steps:
  - ".help"
"#;
        let m: Macro = serde_yaml::from_str(yaml).unwrap();
        assert!(m.variables.is_empty());
        assert_eq!(m.steps.len(), 1);
    }
}
