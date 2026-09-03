use crate::config::paths;
use crate::config::{RequestContext, RoleLike, ensure_parent_exists};
use crate::repl::{run_repl_command, split_args_text};
use crate::utils::{AbortSignal, multiline_text};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::fs::{File, read_to_string};
use std::io::Write;
use std::ops::{Deref, DerefMut};
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
    if ctx.in_non_isolated_macro() {
        bail!("nested macros not allowed in non-isolated mode");
    }
    let macro_value = Macro::load(name, ctx.app.config.no_workspace_macros)?;
    let (new_args, text) = split_args_text(args.unwrap_or_default(), cfg!(windows));
    let variables = macro_value
        .resolve_variables(&new_args, text)
        .map_err(|err| {
            let kv_hint = if macro_value.variables.is_empty() {
                ""
            } else {
                " (variables can also be set by name: name=value, before any positional args)"
            };
            anyhow!("{err}. Usage: {}{kv_hint}", macro_value.usage(name))
        })?;

    if !macro_value.isolated {
        let mut live = MacroModeGuard::new(ctx);
        for step in &macro_value.steps {
            let command = Macro::interpolate_command(step, &variables);
            println!(">> {}", multiline_text(&command));
            run_repl_command(&mut live, abort_signal.clone(), &command).await?;
        }

        return Ok(());
    }

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
    macro_ctx.parent_inbox = ctx.parent_inbox.clone();
    macro_ctx.escalation_queue = ctx.escalation_queue.clone();
    macro_ctx.current_depth = ctx.current_depth;
    macro_ctx.auto_continue_count = ctx.auto_continue_count;
    macro_ctx.todo_list = ctx.todo_list.clone();
    macro_ctx.auto_continue_paused = ctx.auto_continue_paused.clone();
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

struct MacroModeGuard<'a> {
    ctx: &'a mut RequestContext,
    prev_flag: bool,
    prev_non_isolated: bool,
}

impl<'a> MacroModeGuard<'a> {
    fn new(ctx: &'a mut RequestContext) -> Self {
        let prev_flag = ctx.macro_flag;
        let prev_non_isolated = ctx.macro_non_isolated;
        ctx.macro_flag = true;
        ctx.macro_non_isolated = true;
        Self {
            ctx,
            prev_flag,
            prev_non_isolated,
        }
    }
}

impl Deref for MacroModeGuard<'_> {
    type Target = RequestContext;

    fn deref(&self) -> &Self::Target {
        self.ctx
    }
}

impl DerefMut for MacroModeGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx
    }
}

impl Drop for MacroModeGuard<'_> {
    fn drop(&mut self) {
        self.ctx.macro_flag = self.prev_flag;
        self.ctx.macro_non_isolated = self.prev_non_isolated;
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Macro {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub isolated: bool,
    #[serde(default)]
    pub variables: Vec<MacroVariable>,
    pub steps: Vec<String>,
}

impl Macro {
    pub fn load(name: &str, no_workspace_macros: bool) -> Result<Macro> {
        let workspace_path = paths::workspace_macros_dir().join(format!("{name}.yaml"));
        let path = if !no_workspace_macros && workspace_path.exists() {
            workspace_path
        } else {
            paths::macro_file(name)
        };
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

    /// Leading `name=value` tokens assign declared variables by name; the
    /// first token that is not such an assignment starts the positional
    /// args, which fill the remaining unassigned variables in declaration
    /// order. `trailing_text` (the free text after `--`) is always the last
    /// positional and is never scanned for assignments.
    pub fn resolve_variables(
        &self,
        args: &[String],
        trailing_text: &str,
    ) -> Result<IndexMap<String, String>> {
        let (assignments, positional_start) = self.leading_assignments(args)?;
        let mut positionals: Vec<&str> = args[positional_start..]
            .iter()
            .map(|s| s.as_str())
            .collect();
        if !trailing_text.is_empty() {
            positionals.push(trailing_text);
        }

        let mut output = IndexMap::new();
        let mut pos_index = 0;
        for (i, variable) in self.variables.iter().enumerate() {
            let is_rest = variable.rest && i == self.variables.len() - 1;
            let value = if let Some(value) = assignments.get(variable.name.as_str()) {
                Some(value.clone())
            } else if is_rest {
                if pos_index < positionals.len() {
                    Some(positionals[pos_index..].join(" "))
                } else {
                    variable.default.clone()
                }
            } else {
                let positional = positionals.get(pos_index).map(|v| v.to_string());
                if positional.is_some() {
                    pos_index += 1;
                }
                positional.or_else(|| variable.default.clone())
            };
            let value =
                value.ok_or_else(|| anyhow!("Missing value for variable '{}'", variable.name))?;
            output.insert(variable.name.clone(), value);
        }
        Ok(output)
    }

    fn leading_assignments(&self, args: &[String]) -> Result<(IndexMap<String, String>, usize)> {
        let mut assignments = IndexMap::new();
        let mut positional_start = args.len();
        for (i, arg) in args.iter().enumerate() {
            let Some((key, value)) = parse_assignment(arg) else {
                positional_start = i;
                break;
            };
            if !self.variables.iter().any(|v| v.name == key) {
                let declared: Vec<&str> = self.variables.iter().map(|v| v.name.as_str()).collect();
                bail!(
                    "Unknown variable '{key}' (declared variables: {})",
                    declared.join(", ")
                );
            }
            if assignments
                .insert(key.to_string(), value.to_string())
                .is_some()
            {
                bail!("Variable '{key}' was assigned more than once");
            }
        }
        Ok((assignments, positional_start))
    }

    /// Completion candidates for the assignment prefix: one `name=` entry per
    /// variable not yet assigned in `completed_args`. Empty once a
    /// non-assignment token has ended the prefix.
    pub fn variable_completions(&self, completed_args: &[&str]) -> Vec<(String, Option<String>)> {
        let mut assigned: Vec<&str> = Vec::new();
        for arg in completed_args {
            match parse_assignment(arg) {
                Some((key, _)) if self.variables.iter().any(|v| v.name == key) => {
                    assigned.push(key);
                }
                _ => return vec![],
            }
        }
        self.variables
            .iter()
            .filter(|v| !assigned.contains(&v.name.as_str()))
            .map(|v| {
                let requirement = match &v.default {
                    Some(default) => format!("(default: {default})"),
                    None => "(required)".to_string(),
                };
                let hint = match &v.description {
                    Some(description) => format!("{description} {requirement}"),
                    None => requirement,
                };
                (format!("{}=", v.name), Some(hint))
            })
            .collect()
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MacroVariable {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub rest: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// A token is an assignment only when the key before `=` is
/// identifier-shaped: a letter or underscore followed by letters, digits,
/// underscores, or hyphens. Anything else (paths, URLs, prose) is positional.
pub(crate) fn parse_assignment(arg: &str) -> Option<(&str, &str)> {
    let (key, value) = arg.split_once('=')?;
    let mut chars = key.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some((key, value))
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, Session, WorkingMode};
    use crate::utils::{create_abort_signal, get_env_name};
    use serial_test::serial;
    use std::fs::{create_dir_all, remove_dir_all, write};
    use std::future::Future;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, str};

    struct TestConfigDirGuard {
        key: String,
        previous: Option<std::ffi::OsString>,
        path: PathBuf,
    }

    impl TestConfigDirGuard {
        fn new() -> Self {
            let key = get_env_name("config_dir");
            let previous = env::var_os(&key);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-macros-tests-{unique}"));
            create_dir_all(&path).unwrap();
            unsafe {
                env::set_var(&key, &path);
            }
            Self {
                key,
                previous,
                path,
            }
        }
    }

    impl Drop for TestConfigDirGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                unsafe {
                    env::set_var(&self.key, previous);
                }
            } else {
                unsafe {
                    env::remove_var(&self.key);
                }
            }
            let _ = remove_dir_all(&self.path);
        }
    }

    fn test_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn write_macro_file(name: &str, content: &str) {
        let path = paths::macros_dir().join(format!("{name}.yaml"));
        ensure_parent_exists(&path).unwrap();
        write(&path, content).unwrap();
    }

    /// Sets up a temp workspace macros dir and a temp global macros dir, each
    /// containing a `shared` macro whose `description` names its source, and
    /// points the workspace/global dir env overrides at them for `f`.
    fn with_macro_load_envs<F: FnOnce()>(f: F) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-macro-load-tests-{unique}"));
        let workspace_root = root.join("workspace");
        let workspace_macros = workspace_root.join("macros");
        let global = root.join("global");
        create_dir_all(&workspace_macros).unwrap();
        create_dir_all(&global).unwrap();
        write(
            workspace_macros.join("shared.yaml"),
            "description: workspace\nsteps:\n  - \".help\"\n",
        )
        .unwrap();
        write(
            global.join("shared.yaml"),
            "description: global\nsteps:\n  - \".help\"\n",
        )
        .unwrap();

        let ws_env = get_env_name("workspace_config_dir");
        let global_env = get_env_name("macros_dir");
        let prev_ws = env::var_os(&ws_env);
        let prev_global = env::var_os(&global_env);
        unsafe {
            env::set_var(&ws_env, &workspace_root);
            env::set_var(&global_env, &global);
        }
        f();
        unsafe {
            match prev_ws {
                Some(v) => env::set_var(&ws_env, v),
                None => env::remove_var(&ws_env),
            }
            match prev_global {
                Some(v) => env::set_var(&global_env, v),
                None => env::remove_var(&global_env),
            }
        }
        let _ = remove_dir_all(&root);
    }

    #[test]
    #[serial]
    fn load_prefers_workspace_over_global_by_default() {
        with_macro_load_envs(|| {
            let loaded = Macro::load("shared", false).unwrap();
            assert_eq!(loaded.description.as_deref(), Some("workspace"));
        });
    }

    #[test]
    #[serial]
    fn load_skips_workspace_when_no_workspace_macros() {
        with_macro_load_envs(|| {
            let loaded = Macro::load("shared", true).unwrap();
            assert_eq!(loaded.description.as_deref(), Some("global"));
        });
    }

    /// Drives a macro-execution future to completion on a thread with extra
    /// stack headroom: nested `run_repl_command` poll frames are deep in
    /// debug builds and overflow the 2 MiB default test-thread stack.
    fn run_async<F>(f: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn_scoped(scope, || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(f)
                })
                .unwrap()
                .join()
                .unwrap()
        })
    }

    fn var(name: &str, rest: bool, default: Option<&str>) -> MacroVariable {
        MacroVariable {
            name: name.to_string(),
            description: None,
            rest,
            default: default.map(String::from),
        }
    }

    fn macro_with_vars(vars: Vec<MacroVariable>) -> Macro {
        Macro {
            description: None,
            isolated: true,
            variables: vars,
            steps: vec![],
        }
    }

    #[test]
    fn resolve_no_variables() {
        let m = macro_with_vars(vec![]);

        let result = m.resolve_variables(&[], "").unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn resolve_required_variable_provided() {
        let m = macro_with_vars(vec![var("name", false, None)]);

        let result = m.resolve_variables(&["Alice".into()], "").unwrap();

        assert_eq!(result["name"], "Alice");
    }

    #[test]
    fn resolve_required_variable_missing_errors() {
        let m = macro_with_vars(vec![var("name", false, None)]);

        let result = m.resolve_variables(&[], "");

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[test]
    fn resolve_default_variable_uses_default() {
        let m = macro_with_vars(vec![var("color", false, Some("blue"))]);

        let result = m.resolve_variables(&[], "").unwrap();

        assert_eq!(result["color"], "blue");
    }

    #[test]
    fn resolve_default_variable_overridden() {
        let m = macro_with_vars(vec![var("color", false, Some("blue"))]);

        let result = m.resolve_variables(&["red".into()], "").unwrap();

        assert_eq!(result["color"], "red");
    }

    #[test]
    fn resolve_rest_variable_captures_all_remaining() {
        let m = macro_with_vars(vec![var("first", false, None), var("rest", true, None)]);

        let result = m
            .resolve_variables(&["a".into(), "b".into(), "c".into()], "")
            .unwrap();

        assert_eq!(result["first"], "a");
        assert_eq!(result["rest"], "b c");
    }

    #[test]
    fn resolve_rest_variable_with_default() {
        let m = macro_with_vars(vec![var("args", true, Some("default text"))]);

        let result = m.resolve_variables(&[], "").unwrap();

        assert_eq!(result["args"], "default text");
    }

    #[test]
    fn resolve_multiple_variables() {
        let m = macro_with_vars(vec![
            var("a", false, None),
            var("b", false, None),
            var("c", false, Some("default_c")),
        ]);

        let result = m.resolve_variables(&["x".into(), "y".into()], "").unwrap();

        assert_eq!(result["a"], "x");
        assert_eq!(result["b"], "y");
        assert_eq!(result["c"], "default_c");
    }

    #[test]
    fn resolve_assignment_skips_earlier_defaults() {
        let m = macro_with_vars(vec![
            var("a", false, Some("da")),
            var("b", false, Some("db")),
            var("c", false, None),
        ]);

        let result = m.resolve_variables(&["c=x".into()], "").unwrap();

        assert_eq!(result["a"], "da");
        assert_eq!(result["b"], "db");
        assert_eq!(result["c"], "x");
    }

    #[test]
    fn resolve_positionals_fill_unassigned_variables_after_assignments() {
        let m = macro_with_vars(vec![
            var("a", false, None),
            var("b", false, None),
            var("c", false, None),
        ]);

        let result = m
            .resolve_variables(&["b=middle".into(), "first".into(), "last".into()], "")
            .unwrap();

        assert_eq!(result["a"], "first");
        assert_eq!(result["b"], "middle");
        assert_eq!(result["c"], "last");
    }

    #[test]
    fn resolve_rest_variable_by_assignment_and_by_positionals() {
        let m = macro_with_vars(vec![
            var("scope", false, Some("all")),
            var("text", true, None),
        ]);

        let result = m
            .resolve_variables(&["text=hello world".into()], "")
            .unwrap();
        assert_eq!(result["scope"], "all");
        assert_eq!(result["text"], "hello world");

        let result = m
            .resolve_variables(
                &["scope=one".into(), "fix".into(), "the".into(), "bug".into()],
                "",
            )
            .unwrap();
        assert_eq!(result["scope"], "one");
        assert_eq!(result["text"], "fix the bug");
    }

    #[test]
    fn resolve_assignment_prefix_ends_at_first_positional() {
        let m = macro_with_vars(vec![var("a", false, None), var("b", true, Some(""))]);

        let result = m
            .resolve_variables(&["value".into(), "a=literal".into()], "")
            .unwrap();

        assert_eq!(result["a"], "value");
        assert_eq!(result["b"], "a=literal");
    }

    #[test]
    fn resolve_trailing_text_is_never_scanned_for_assignments() {
        let m = macro_with_vars(vec![var("text", true, None)]);

        let result = m
            .resolve_variables(&[], "text=looks like an assignment")
            .unwrap();

        assert_eq!(result["text"], "text=looks like an assignment");
    }

    #[test]
    fn resolve_rejects_unknown_and_duplicate_assignments() {
        let m = macro_with_vars(vec![var("scope", false, None)]);

        let err = m.resolve_variables(&["scpe=all".into()], "").unwrap_err();
        assert!(err.to_string().contains("Unknown variable 'scpe'"));
        assert!(err.to_string().contains("scope"));

        let err = m
            .resolve_variables(&["scope=a".into(), "scope=b".into()], "")
            .unwrap_err();
        assert!(err.to_string().contains("assigned more than once"));
    }

    #[test]
    fn resolve_non_identifier_equals_tokens_are_positional() {
        let m = macro_with_vars(vec![var("a", false, None)]);

        let result = m.resolve_variables(&["path/x=1".into()], "").unwrap();
        assert_eq!(result["a"], "path/x=1");

        let result = m.resolve_variables(&["1x=2".into()], "").unwrap();
        assert_eq!(result["a"], "1x=2");
    }

    #[test]
    fn parse_assignment_requires_an_identifier_shaped_key() {
        assert_eq!(parse_assignment("scope=all"), Some(("scope", "all")));
        assert_eq!(parse_assignment("_x-1=v"), Some(("_x-1", "v")));
        assert_eq!(parse_assignment("k="), Some(("k", "")));
        assert_eq!(parse_assignment("noequals"), None);
        assert_eq!(parse_assignment("=v"), None);
        assert_eq!(parse_assignment("1a=v"), None);
        assert_eq!(parse_assignment("a/b=v"), None);
    }

    #[test]
    fn variable_completions_list_unassigned_variables_with_hints() {
        let mut m = macro_with_vars(vec![
            var("scope", false, Some("all")),
            var("text", true, None),
        ]);
        m.variables[0].description = Some("What to review".to_string());

        let values = m.variable_completions(&[]);
        assert_eq!(
            values,
            vec![
                (
                    "scope=".to_string(),
                    Some("What to review (default: all)".to_string())
                ),
                ("text=".to_string(), Some("(required)".to_string())),
            ]
        );

        let values = m.variable_completions(&["scope=one"]);
        assert_eq!(
            values,
            vec![("text=".to_string(), Some("(required)".to_string()))]
        );

        assert!(m.variable_completions(&["positional"]).is_empty());
        assert!(
            m.variable_completions(&["scope=one", "free", "text=x"])
                .is_empty()
        );
    }

    #[test]
    fn macro_variable_description_parses_from_yaml_and_defaults_to_none() {
        let parsed: Macro = serde_yaml::from_str(
            "variables:\n  - name: scope\n    description: What to review\n    default: all\n  - name: text\n    rest: true\nsteps:\n  - .file {{scope}} -- {{text}}\n",
        )
        .unwrap();
        assert_eq!(
            parsed.variables[0].description.as_deref(),
            Some("What to review")
        );
        assert!(parsed.variables[1].description.is_none());
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

    #[test]
    fn deserialize_macro_without_new_fields_uses_defaults() {
        let yaml = r#"
steps:
  - ".help"
"#;

        let m: Macro = serde_yaml::from_str(yaml).unwrap();

        assert!(m.description.is_none());
        assert!(m.isolated);
    }

    #[test]
    fn deserialize_macro_with_description_and_isolated() {
        let yaml = r#"
description: "Review WIP against a base branch"
isolated: false
steps:
  - "Review the diff against {{base}}"
variables:
  - name: base
    default: main
"#;

        let m: Macro = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(
            m.description.as_deref(),
            Some("Review WIP against a base branch")
        );
        assert!(!m.isolated);
        assert_eq!(m.variables.len(), 1);
    }

    #[test]
    fn round_trip_preserves_new_fields() {
        let original = Macro {
            description: Some("does a thing".to_string()),
            isolated: false,
            variables: vec![var("target", false, Some("all"))],
            steps: vec!["build {{target}}".to_string()],
        };

        let yaml = serde_yaml::to_string(&original).unwrap();
        let back: Macro = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(back.description.as_deref(), Some("does a thing"));
        assert!(!back.isolated);
        assert_eq!(back.variables.len(), 1);
        assert_eq!(back.variables[0].name, "target");
        assert_eq!(back.variables[0].default.as_deref(), Some("all"));
        assert_eq!(back.steps, original.steps);
    }

    #[test]
    fn round_trip_defaults_survive() {
        let original = macro_with_vars(vec![]);
        let yaml = serde_yaml::to_string(&original).unwrap();

        assert!(!yaml.contains("description"));
        let back: Macro = serde_yaml::from_str(&yaml).unwrap();
        assert!(back.description.is_none());
        assert!(back.isolated);
    }

    #[test]
    fn embedded_macro_assets_deserialize_with_defaults() {
        for file in MacroAssets::iter() {
            let embedded = MacroAssets::get(&file).unwrap();
            let content = str::from_utf8(&embedded.data).unwrap();

            let m: Macro = serde_yaml::from_str(content)
                .unwrap_or_else(|e| panic!("asset '{}' failed to deserialize: {e}", file.as_ref()));

            assert!(m.isolated, "asset '{}'", file.as_ref());
            assert!(!m.steps.is_empty(), "asset '{}'", file.as_ref());
        }
    }

    #[test]
    #[serial]
    fn non_isolated_steps_run_on_live_ctx_and_mutations_persist() {
        let _guard = TestConfigDirGuard::new();
        write_macro_file(
            "live-macro",
            "isolated: false\nsteps:\n  - \".set temperature 0.42\"\n",
        );
        let mut ctx = test_ctx();
        ctx.session = Some(Session::default());

        run_async(macro_execute(
            &mut ctx,
            "live-macro",
            None,
            create_abort_signal(),
        ))
        .unwrap();

        assert!(ctx.session.is_some(), "live session must survive the macro");
        assert_eq!(
            ctx.session.as_ref().unwrap().temperature(),
            Some(0.42),
            "the step must mutate the live context's session, not a fork"
        );
        assert!(!ctx.macro_flag, "flag must be restored after success");
        assert!(
            !ctx.macro_non_isolated,
            "mode must be restored after success"
        );
    }

    #[test]
    #[serial]
    fn non_isolated_step_failure_aborts_and_restores_flag_and_mode() {
        let _guard = TestConfigDirGuard::new();
        write_macro_file(
            "fail-macro",
            "isolated: false\nsteps:\n  - \".set temperature 0.9\"\n  - \".update\"\n  - \".set temperature 0.1\"\n",
        );
        let mut ctx = test_ctx();
        ctx.session = Some(Session::default());

        let result = run_async(macro_execute(
            &mut ctx,
            "fail-macro",
            None,
            create_abort_signal(),
        ));

        assert!(result.is_err(), "a failing step must abort the macro");
        assert_eq!(
            ctx.session.as_ref().unwrap().temperature(),
            Some(0.9),
            "completed steps' mutations persist; steps after the failure never run"
        );
        assert!(!ctx.macro_flag, "flag must be restored on the error path");
        assert!(
            !ctx.macro_non_isolated,
            "mode must be restored on the error path"
        );
    }

    #[test]
    fn nested_macro_rejected_when_non_isolated_mode_active() {
        let mut ctx = test_ctx();
        ctx.macro_flag = true;
        ctx.macro_non_isolated = true;

        let result = run_async(macro_execute(
            &mut ctx,
            "anything",
            None,
            create_abort_signal(),
        ));

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nested macros not allowed in non-isolated mode"),
            "{err}"
        );
    }

    #[test]
    #[serial]
    fn non_isolated_macro_step_invoking_macro_is_rejected() {
        let _guard = TestConfigDirGuard::new();
        write_macro_file(
            "outer-macro",
            "isolated: false\nsteps:\n  - \".inner-macro\"\n",
        );
        write_macro_file(
            "inner-macro",
            "isolated: false\nsteps:\n  - \".set temperature 0.5\"\n",
        );
        let mut ctx = test_ctx();
        ctx.session = Some(Session::default());

        let result = run_async(macro_execute(
            &mut ctx,
            "outer-macro",
            None,
            create_abort_signal(),
        ));

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nested macros not allowed in non-isolated mode"),
            "{err}"
        );
        assert_eq!(
            ctx.session.as_ref().unwrap().temperature(),
            None,
            "the nested macro's steps must not run"
        );
        assert!(!ctx.macro_flag);
        assert!(!ctx.macro_non_isolated);
    }

    #[test]
    #[serial]
    fn isolated_macro_step_runs_non_isolated_macro_inline_on_fork() {
        let _guard = TestConfigDirGuard::new();
        write_macro_file("iso-outer-macro", "steps:\n  - \".inner-macro\"\n");
        write_macro_file(
            "inner-macro",
            "isolated: false\nsteps:\n  - \".set temperature 0.33\"\n",
        );
        let mut ctx = test_ctx();
        ctx.session = Some(Session::default());

        run_async(macro_execute(
            &mut ctx,
            "iso-outer-macro",
            None,
            create_abort_signal(),
        ))
        .unwrap();

        assert_eq!(
            ctx.session.as_ref().unwrap().temperature(),
            None,
            "the inline run happens on the fork, never on the live context"
        );
        assert!(!ctx.macro_flag);
        assert!(!ctx.macro_non_isolated);
    }

    #[test]
    #[serial]
    fn isolated_macro_still_forks_and_leaves_live_ctx_untouched() {
        let _guard = TestConfigDirGuard::new();
        write_macro_file("iso-macro", "steps:\n  - \".set temperature 0.77\"\n");
        let mut ctx = test_ctx();
        ctx.session = Some(Session::default());
        let app_before = Arc::clone(&ctx.app.config);

        run_async(macro_execute(
            &mut ctx,
            "iso-macro",
            None,
            create_abort_signal(),
        ))
        .unwrap();

        assert!(ctx.session.is_some());
        assert_eq!(
            ctx.session.as_ref().unwrap().temperature(),
            None,
            "an isolated macro's mutations must stay on the fork"
        );
        assert!(
            Arc::ptr_eq(&ctx.app.config, &app_before),
            "isolated execution must not swap the live app config"
        );
        assert!(!ctx.macro_flag);
    }

    #[test]
    #[serial]
    fn guard_restores_prior_flag_values_after_inline_run() {
        let _guard = TestConfigDirGuard::new();
        write_macro_file(
            "inner-macro",
            "isolated: false\nsteps:\n  - \".set temperature 0.11\"\n",
        );
        let mut ctx = test_ctx();
        ctx.macro_flag = true;

        run_async(macro_execute(
            &mut ctx,
            "inner-macro",
            None,
            create_abort_signal(),
        ))
        .unwrap();

        assert!(
            ctx.macro_flag,
            "a pre-existing flag must be restored, not cleared"
        );
        assert!(!ctx.macro_non_isolated);
    }
}
