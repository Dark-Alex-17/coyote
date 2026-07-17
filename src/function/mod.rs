pub(crate) mod memory;
pub(crate) mod skill;
pub(crate) mod supervisor;
pub(crate) mod todo;
pub(crate) mod user_interaction;

use crate::{
    config::{Agent, RequestContext},
    graph,
    utils::*,
};

use crate::config::ensure_parent_exists;
use crate::config::paths;
use crate::mcp::{
    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
    MCP_SEARCH_META_FUNCTION_NAME_PREFIX, McpServersConfig,
};
use crate::parsers::{bash, python, typescript};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use indoc::formatdoc;
use memory::MEMORY_FUNCTION_PREFIX;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use skill::SKILL_FUNCTION_PREFIX;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use strum_macros::AsRefStr;
use supervisor::SUPERVISOR_FUNCTION_PREFIX;
use todo::TODO_FUNCTION_PREFIX;
use user_interaction::USER_FUNCTION_PREFIX;

#[derive(Embed)]
#[folder = "assets/functions/"]
struct FunctionAssets;

#[cfg(windows)]
const PATH_SEP: &str = ";";
#[cfg(not(windows))]
const PATH_SEP: &str = ":";

#[derive(AsRefStr)]
enum BinaryType<'a> {
    Tool(Option<&'a str>),
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, AsRefStr)]
pub enum Language {
    Bash,
    Python,
    TypeScript,
    Unsupported,
}

impl From<&String> for Language {
    fn from(s: &String) -> Self {
        Language::from_extension(s)
    }
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "sh" => Language::Bash,
            "py" => Language::Python,
            "ts" => Language::TypeScript,
            _ => Language::Unsupported,
        }
    }
}

#[cfg_attr(not(windows), expect(dead_code))]
impl Language {
    fn to_cmd(self) -> &'static str {
        match self {
            Language::Bash => "bash",
            Language::Python => "python",
            Language::TypeScript => "npx tsx",
            Language::Unsupported => "sh",
        }
    }

    fn to_extension(self) -> &'static str {
        match self {
            Language::Bash => "sh",
            Language::Python => "py",
            Language::TypeScript => "ts",
            _ => "sh",
        }
    }
}

impl Language {
    pub fn direct_invoker(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Language::Bash => Some(("bash", &[])),
            Language::Python => Some(("python3", &[])),
            Language::TypeScript => Some(("npx", &["tsx"])),
            Language::Unsupported => None,
        }
    }
}

fn extract_shebang_runtime(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = io::BufReader::new(file);
    let first_line = io::BufRead::lines(reader).next()?.ok()?;
    let shebang = first_line.strip_prefix("#!")?;
    let cmd = shebang.trim();
    if cmd.is_empty() {
        return None;
    }
    if let Some(after_env) = cmd.strip_prefix("/usr/bin/env ") {
        let runtime = after_env.trim();
        if runtime.is_empty() {
            return None;
        }
        Some(runtime.to_string())
    } else {
        Some(cmd.to_string())
    }
}

pub async fn eval_tool_calls(
    ctx: &mut RequestContext,
    mut calls: Vec<ToolCall>,
) -> Result<Vec<ToolResult>> {
    let mut output = vec![];
    if calls.is_empty() {
        return Ok(output);
    }
    calls = ToolCall::dedup(calls);
    if calls.is_empty() {
        bail!("The request was aborted because an infinite loop of function calls was detected.")
    }
    for call in calls {
        if let Some(msg) = ctx.tool_scope.tool_tracker.check_loop(&call.clone()) {
            let dup_msg = format!("{{\"tool_call_loop_alert\":{}}}", msg.trim());
            println!(
                "{}",
                warning_text(format!("{}: ⚠️ Tool-call loop detected! ⚠️", call.name).as_str())
            );
            let val = json!(dup_msg);
            output.push(ToolResult::new(call, val));
            continue;
        }
        let result = call.eval(ctx).await?;
        output.push(ToolResult::new(call, normalize_tool_result(result)));
    }

    if !output.is_empty() {
        let (has_escalations, summary) = if ctx.current_depth == 0
            && let Some(queue) = ctx.root_escalation_queue()
            && queue.has_pending()
        {
            (true, queue.pending_summary())
        } else {
            (false, vec![])
        };

        if has_escalations {
            let notification = json!({
                "pending_escalations": summary,
                "instruction": "Child agents are BLOCKED waiting for your reply. Call agent__reply_escalation for each pending escalation to unblock them."
            });
            let synthetic_call = ToolCall::new(
                "__escalation_notification".to_string(),
                json!({}),
                Some("escalation_check".to_string()),
            );
            output.push(ToolResult::new(synthetic_call, notification));
        }
    }

    Ok(output)
}

/// Tools that succeed silently (e.g. `mkdir -p` via execute_command) evaluate to
/// `Null`. Substitute a concrete `"DONE"` marker so every call produces a
/// `ToolResult`: agentic loops (graph llm nodes, spawned agents, the REPL) treat
/// an empty `tool_results` as "the LLM concluded", so dropping silent results
/// would prematurely terminate a turn that called only silent tools.
fn normalize_tool_result(result: Value) -> Value {
    if result.is_null() {
        json!("DONE")
    } else {
        result
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self {
            call,
            output,
            text: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Functions {
    declarations: Vec<FunctionDeclaration>,
}

impl Functions {
    pub fn install_builtin_global_tools(force: bool) -> Result<()> {
        info!(
            "Installing global built-in functions in {}",
            paths::functions_dir().display()
        );

        for file in FunctionAssets::iter() {
            debug!("Processing function file: {}", file.as_ref());
            if file.as_ref().starts_with("scripts/") {
                debug!("Skipping script file: {}", file.as_ref());
                continue;
            }

            let embedded_file = FunctionAssets::get(&file).ok_or_else(|| {
                anyhow!("Failed to load embedded function file: {}", file.as_ref())
            })?;
            let content = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
            let file_path = paths::functions_dir().join(file.as_ref());
            #[cfg_attr(not(unix), expect(unused))]
            let is_script = file_path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|ext| Language::from_extension(ext) != Language::Unsupported);

            let force_this = force && file.as_ref() != "mcp.json";
            if file_path.exists() && !force_this {
                debug!(
                    "Function file already exists, skipping: {}",
                    file_path.display()
                );
                continue;
            }

            ensure_parent_exists(&file_path)?;
            info!("Creating function file: {}", file_path.display());
            let mut function_file = File::create(&file_path)?;
            function_file.write_all(content.as_bytes())?;

            #[cfg(unix)]
            if is_script {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755))?;
            }
        }

        Ok(())
    }

    pub fn install_mcp_config() -> Result<()> {
        let file_path = paths::mcp_config_file();
        let embedded = FunctionAssets::get("mcp.json")
            .ok_or_else(|| anyhow!("Failed to load embedded mcp.json"))?;
        let bundled_content = unsafe { std::str::from_utf8_unchecked(&embedded.data) };
        let bundled: McpServersConfig =
            serde_json::from_str(bundled_content).context("failed to parse embedded mcp.json")?;

        ensure_parent_exists(&file_path)?;

        let mut merged = if file_path.exists() {
            let existing =
                fs::read_to_string(&file_path).context("failed to read existing mcp.json")?;
            serde_json::from_str::<McpServersConfig>(&existing)
                .context("failed to parse existing mcp.json")?
        } else {
            McpServersConfig {
                mcp_servers: IndexMap::new(),
            }
        };

        let mut added = Vec::new();
        for (name, server) in bundled.mcp_servers {
            if !merged.mcp_servers.contains_key(&name) {
                merged.mcp_servers.insert(name.clone(), server);
                added.push(name);
            }
        }

        info!("Merging bundled MCP config into: {}", file_path.display());

        let serialized =
            serde_json::to_string_pretty(&merged).context("failed to serialize merged mcp.json")?;
        let tmp = file_path.with_extension("json.tmp");
        fs::write(&tmp, &serialized).context("failed to write temporary mcp.json")?;
        fs::rename(&tmp, &file_path).context("failed to finalize mcp.json")?;

        if !added.is_empty() {
            println!("  + new MCP servers: {}", added.join(", "));
        }

        Ok(())
    }

    pub fn init(visible_tools: &[String]) -> Result<Self> {
        Self::clear_global_functions_bin_dir()?;

        let declarations = Self {
            declarations: Self::build_global_tool_declarations(visible_tools)?,
        };

        info!(
            "Building global function binaries in {}",
            paths::functions_bin_dir().display()
        );
        Self::build_global_function_binaries(visible_tools, None)?;

        Ok(declarations)
    }

    pub fn init_agent(name: &str, global_tools: &[String]) -> Result<Self> {
        Self::clear_agent_bin_dir(name)?;

        let global_tools_declarations = if !global_tools.is_empty() {
            info!("Loading global tools for agent: {name}: {global_tools:?}");
            let tools_declarations = Self::build_global_tool_declarations(global_tools)?;

            info!(
                "Building global function binaries required by agent: {name} in {}",
                paths::functions_bin_dir().display()
            );
            Self::build_global_function_binaries(global_tools, Some(name))?;
            tools_declarations
        } else {
            debug!("No global tools found for agent: {}", name);
            Vec::new()
        };
        let agent_script_declarations = match paths::agent_functions_file(name) {
            Ok(path) if path.exists() => {
                info!(
                    "Loading functions script for agent: {name} from {}",
                    path.display()
                );
                let script_declarations = Self::generate_declarations(&path)?;
                debug!("agent_declarations: {:#?}", script_declarations);

                info!(
                    "Building function binary for agent: {name} in {}",
                    paths::agent_bin_dir(name).display()
                );
                Self::build_agent_tool_binaries(name)?;
                script_declarations
            }
            _ => {
                debug!("No functions script found for agent: {}", name);
                Vec::new()
            }
        };
        let declarations = [global_tools_declarations, agent_script_declarations].concat();

        Ok(Self { declarations })
    }

    pub fn find(&self, name: &str) -> Option<&FunctionDeclaration> {
        self.declarations.iter().find(|v| v.name == name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.declarations.iter().any(|v| v.name == name)
    }

    pub fn declarations(&self) -> &[FunctionDeclaration] {
        &self.declarations
    }

    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }

    pub fn append_todo_functions(&mut self) {
        self.declarations.extend(todo::todo_function_declarations());
    }

    pub fn remove_todo_functions(&mut self) {
        self.declarations
            .retain(|f| !f.name.starts_with(TODO_FUNCTION_PREFIX));
    }

    pub fn append_memory_functions(&mut self) {
        self.declarations
            .extend(memory::memory_function_declarations());
    }

    pub fn remove_memory_functions(&mut self) {
        self.declarations
            .retain(|f| !f.name.starts_with(MEMORY_FUNCTION_PREFIX));
    }

    pub fn append_skill_functions(&mut self) {
        self.declarations
            .extend(skill::skill_function_declarations());
    }

    pub fn append_supervisor_functions(&mut self) {
        self.declarations
            .extend(supervisor::supervisor_function_declarations());
        self.declarations
            .extend(supervisor::escalation_function_declarations());
    }

    pub fn append_teammate_functions(&mut self) {
        self.declarations
            .extend(supervisor::teammate_function_declarations());
    }

    pub fn append_user_interaction_functions(&mut self) {
        self.declarations
            .extend(user_interaction::user_interaction_function_declarations());
    }

    pub fn append_mcp_meta_functions(&mut self, mcp_servers: Vec<String>) {
        let mut invoke_function_properties = IndexMap::new();
        invoke_function_properties.insert(
            "tool".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                ..Default::default()
            },
        );
        invoke_function_properties.insert(
            "arguments".to_string(),
            JsonSchema {
                type_value: Some("object".to_string()),
                ..Default::default()
            },
        );

        let mut search_function_properties = IndexMap::new();
        search_function_properties.insert(
            "query".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                description: Some("Generalized explanation of what you want to do".into()),
                ..Default::default()
            },
        );
        search_function_properties.insert(
            "top_k".to_string(),
            JsonSchema {
                type_value: Some("integer".to_string()),
                description: Some("How many results to return, between 1 and 20".into()),
                default: Some(Value::from(8usize)),
                ..Default::default()
            },
        );

        let mut describe_function_properties = IndexMap::new();
        describe_function_properties.insert(
            "tool".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                description: Some("The name of the tool; e.g., search_issues".into()),
                ..Default::default()
            },
        );

        for server in mcp_servers {
            let search_function_name = format!("{}_{server}", MCP_SEARCH_META_FUNCTION_NAME_PREFIX);
            let describe_function_name =
                format!("{}_{server}", MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX);
            let invoke_function_name = format!("{}_{server}", MCP_INVOKE_META_FUNCTION_NAME_PREFIX);
            let invoke_function_declaration = FunctionDeclaration {
                name: invoke_function_name.clone(),
                description: formatdoc!(
                    r#"
										Invoke the specified tool on the {server} MCP server. Always call {describe_function_name} first to
										find the correct invocation schema for the given tool.
										"#
                ),
                parameters: JsonSchema {
                    type_value: Some("object".to_string()),
                    properties: Some(invoke_function_properties.clone()),
                    required: Some(vec!["tool".to_string()]),
                    ..Default::default()
                },
                agent: false,
            };
            let search_functions_declaration = FunctionDeclaration {
                name: search_function_name.clone(),
                description: formatdoc!(
                    r#"
                    Find candidate tools by keywords for the {server} MCP server. Returns small suggestions; fetch
                    schemas with {describe_function_name}.
                    "#
                ),
                parameters: JsonSchema {
                    type_value: Some("object".to_string()),
                    properties: Some(search_function_properties.clone()),
                    required: Some(vec!["query".to_string()]),
                    ..Default::default()
                },
                agent: false,
            };
            let describe_functions_declaration = FunctionDeclaration {
                name: describe_function_name.clone(),
                description: "Get the full JSON schema for exactly one MCP tool.".to_string(),
                parameters: JsonSchema {
                    type_value: Some("object".to_string()),
                    properties: Some(describe_function_properties.clone()),
                    required: Some(vec!["tool".to_string()]),
                    ..Default::default()
                },
                agent: false,
            };
            self.declarations.push(invoke_function_declaration);
            self.declarations.push(search_functions_declaration);
            self.declarations.push(describe_functions_declaration);
        }
    }

    fn build_global_tool_declarations(
        enabled_tools: &[String],
    ) -> Result<Vec<FunctionDeclaration>> {
        let global_tools_directory = paths::global_tools_dir();
        let mut function_declarations = Vec::new();

        for tool in enabled_tools {
            let declaration = Self::generate_declarations(&global_tools_directory.join(tool))?;
            function_declarations.extend(declaration);
        }

        Ok(function_declarations)
    }

    fn generate_declarations(tools_file_path: &Path) -> Result<Vec<FunctionDeclaration>> {
        info!(
            "Loading tool definitions from {}",
            tools_file_path.display()
        );
        let file_name = tools_file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                anyhow::format_err!("Unable to extract file name from path: {tools_file_path:?}")
            })?;

        match File::open(tools_file_path) {
            Ok(tool_file) => {
                let language = Language::from(
                    &tools_file_path
                        .extension()
                        .and_then(OsStr::to_str)
                        .map(|s| s.to_lowercase())
                        .ok_or_else(|| {
                            anyhow!("Unable to extract language from tool file: {file_name}")
                        })?,
                );

                match language {
                    Language::Bash => {
                        bash::generate_bash_declarations(tool_file, tools_file_path, file_name)
                    }
                    Language::Python => python::generate_python_declarations(
                        tool_file,
                        file_name,
                        tools_file_path.parent(),
                    ),
                    Language::TypeScript => typescript::generate_typescript_declarations(
                        tool_file,
                        file_name,
                        tools_file_path.parent(),
                    ),
                    Language::Unsupported => {
                        bail!("Unsupported tool file extension: {}", language.as_ref())
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                bail!(
                    "Tool definition file not found: {}",
                    tools_file_path.display()
                );
            }
            Err(err) => bail!("Unable to open tool definition file. {}", err),
        }
    }

    fn build_global_function_binaries(
        enabled_tools: &[String],
        agent_name: Option<&str>,
    ) -> Result<()> {
        for tool in enabled_tools {
            let language = Language::from(
                &Path::new(&tool)
                    .extension()
                    .and_then(OsStr::to_str)
                    .map(|s| s.to_lowercase())
                    .ok_or_else(|| {
                        anyhow::format_err!("Unable to extract file extension from path: {tool:?}")
                    })?,
            );
            let binary_name = Path::new(&tool)
                .file_stem()
                .and_then(OsStr::to_str)
                .ok_or_else(|| {
                    anyhow::format_err!("Unable to extract file name from path: {tool:?}")
                })?;

            if language == Language::Unsupported {
                bail!("Unsupported tool file extension: {}", language.as_ref());
            }

            let tool_path = paths::global_tools_dir().join(tool);
            let custom_runtime = extract_shebang_runtime(&tool_path);
            Self::build_binaries(
                binary_name,
                language,
                BinaryType::Tool(agent_name),
                custom_runtime.as_deref(),
            )?;
        }

        Ok(())
    }

    fn clear_agent_bin_dir(name: &str) -> Result<()> {
        let agent_bin_directory = paths::agent_bin_dir(name);
        if !agent_bin_directory.exists() {
            debug!(
                "Creating agent bin directory: {}",
                agent_bin_directory.display()
            );
            fs::create_dir_all(&agent_bin_directory)?;
        } else {
            debug!(
                "Clearing existing agent bin directory: {}",
                agent_bin_directory.display()
            );
            clear_dir(&agent_bin_directory)?;
        }

        Ok(())
    }

    fn clear_global_functions_bin_dir() -> Result<()> {
        let bin_dir = paths::functions_bin_dir();
        if !bin_dir.exists() {
            fs::create_dir_all(&bin_dir)?;
        }

        info!(
            "Clearing existing function binaries in {}",
            bin_dir.display()
        );
        clear_dir(&bin_dir)?;

        Ok(())
    }

    fn build_agent_tool_binaries(name: &str) -> Result<()> {
        let tools_file = paths::agent_functions_file(name)?;
        let language = Language::from(
            &tools_file
                .extension()
                .and_then(OsStr::to_str)
                .map(|s| s.to_lowercase())
                .ok_or_else(|| {
                    anyhow::format_err!("Unable to extract file extension from path: {name:?}")
                })?,
        );

        if language == Language::Unsupported {
            bail!("Unsupported tool file extension: {}", language.as_ref());
        }

        let custom_runtime = extract_shebang_runtime(&tools_file);
        Self::build_binaries(name, language, BinaryType::Agent, custom_runtime.as_deref())
    }

    #[cfg(windows)]
    fn build_binaries(
        binary_name: &str,
        language: Language,
        binary_type: BinaryType,
        custom_runtime: Option<&str>,
    ) -> Result<()> {
        use native::runtime;
        let (binary_file, binary_script_file) = match binary_type {
            BinaryType::Tool(None) => (
                paths::functions_bin_dir().join(format!("{binary_name}.cmd")),
                paths::functions_bin_dir()
                    .join(format!("run-{binary_name}.{}", language.to_extension())),
            ),
            BinaryType::Tool(Some(agent_name)) => (
                paths::agent_bin_dir(agent_name).join(format!("{binary_name}.cmd")),
                paths::agent_bin_dir(agent_name)
                    .join(format!("run-{binary_name}.{}", language.to_extension())),
            ),
            BinaryType::Agent => (
                paths::agent_bin_dir(binary_name).join(format!("{binary_name}.cmd")),
                paths::agent_bin_dir(binary_name)
                    .join(format!("run-{binary_name}.{}", language.to_extension())),
            ),
        };
        info!(
            "Building binary runner for function: {} ({})",
            binary_name,
            binary_script_file.display(),
        );
        let embedded_file = FunctionAssets::get(&format!(
            "scripts/run-{}.{}",
            binary_type.as_ref().to_lowercase(),
            language.to_extension()
        ))
        .ok_or_else(|| {
            anyhow!(
                "Failed to load embedded script for run-{}.{}",
                binary_type.as_ref().to_lowercase(),
                language.to_extension()
            )
        })?;
        let content_template = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
        let to_script_path = |p: &str| -> String { p.replace('\\', "/") };
        let content = match binary_type {
            BinaryType::Tool(None) => {
                let root_dir = paths::functions_dir();
                let tool_path = format!(
                    "{}/{binary_name}",
                    paths::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &to_script_path(&root_dir.to_string_lossy()))
                    .replace("{tool_path}", &to_script_path(&tool_path))
            }
            BinaryType::Tool(Some(agent_name)) => {
                let root_dir = paths::agent_data_dir(agent_name);
                let tool_path = format!(
                    "{}/{binary_name}",
                    paths::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &to_script_path(&root_dir.to_string_lossy()))
                    .replace("{tool_path}", &to_script_path(&tool_path))
            }
            BinaryType::Agent => content_template
                .replace("{agent_name}", binary_name)
                .replace(
                    "{config_dir}",
                    &to_script_path(&paths::config_dir().to_string_lossy()),
                ),
        }
        .replace(
            "{prompt_utils_file}",
            &to_script_path(&paths::bash_prompt_utils_file().to_string_lossy()),
        );
        if binary_script_file.exists() {
            fs::remove_file(&binary_script_file)?;
        }
        let mut script_file = File::create(&binary_script_file)?;
        script_file.write_all(content.as_bytes())?;

        info!(
            "Building binary for function: {} ({})",
            binary_name,
            binary_file.display()
        );

        let run = if let Some(rt) = custom_runtime {
            rt.to_string()
        } else {
            match language {
                Language::Bash => {
                    let shell = runtime::bash_path().ok_or_else(|| anyhow!("Shell not found"))?;
                    format!("{shell} --noprofile --norc")
                }
                Language::Python if Path::new(".venv").exists() => {
                    let executable_path = env::current_dir()?
                        .join(".venv")
                        .join("Scripts")
                        .join("activate.bat");
                    let canonicalized_path = dunce::canonicalize(&executable_path)?;
                    format!(
                        "call \"{}\" && {}",
                        canonicalized_path.to_string_lossy(),
                        language.to_cmd()
                    )
                }
                Language::Python => {
                    let executable_path = which::which("python")
                        .or_else(|_| which::which("python3"))
                        .map_err(|_| anyhow!("Python executable not found in PATH"))?;
                    let canonicalized_path = dunce::canonicalize(&executable_path)?;
                    canonicalized_path.to_string_lossy().into_owned()
                }
                Language::TypeScript => {
                    let npx_path = which::which("npx").map_err(|_| {
                        anyhow!("npx executable not found in PATH (required for TypeScript tools)")
                    })?;
                    let canonicalized_path = dunce::canonicalize(&npx_path)?;
                    format!("{} tsx", canonicalized_path.to_string_lossy())
                }
                _ => bail!("Unsupported language: {}", language.as_ref()),
            }
        };
        let bin_dir = binary_file
            .parent()
            .expect("Failed to get parent directory of binary file");
        let canonical_bin_dir = dunce::canonicalize(bin_dir)?.to_string_lossy().into_owned();
        let wrapper_binary = dunce::canonicalize(&binary_script_file)?
            .to_string_lossy()
            .into_owned();
        let content = formatdoc!(
            r#"
						@echo off
						setlocal

						set "bin_dir={canonical_bin_dir}"

						{run} "{wrapper_binary}" %*"#,
        );

        let mut file = File::create(&binary_file)?;
        file.write_all(content.as_bytes())?;

        Ok(())
    }

    #[cfg(not(windows))]
    fn build_binaries(
        binary_name: &str,
        language: Language,
        binary_type: BinaryType,
        custom_runtime: Option<&str>,
    ) -> Result<()> {
        use std::os::unix::prelude::PermissionsExt;

        let binary_file = match binary_type {
            BinaryType::Tool(None) => paths::functions_bin_dir().join(binary_name),
            BinaryType::Tool(Some(agent_name)) => {
                paths::agent_bin_dir(agent_name).join(binary_name)
            }
            BinaryType::Agent => paths::agent_bin_dir(binary_name).join(binary_name),
        };
        info!(
            "Building binary for function: {} ({})",
            binary_name,
            binary_file.display()
        );
        let embedded_file = FunctionAssets::get(&format!(
            "scripts/run-{}.{}",
            binary_type.as_ref().to_lowercase(),
            language.to_extension()
        ))
        .ok_or_else(|| {
            anyhow!(
                "Failed to load embedded script for run-{}.{}",
                binary_type.as_ref().to_lowercase(),
                language.to_extension()
            )
        })?;
        let content_template = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
        let mut content = match binary_type {
            BinaryType::Tool(None) => {
                let root_dir = paths::functions_dir();
                let tool_path = format!(
                    "{}/{binary_name}",
                    paths::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &root_dir.to_string_lossy())
                    .replace("{tool_path}", &tool_path)
            }
            BinaryType::Tool(Some(agent_name)) => {
                let root_dir = paths::agent_data_dir(agent_name);
                let tool_path = format!(
                    "{}/{binary_name}",
                    paths::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &root_dir.to_string_lossy())
                    .replace("{tool_path}", &tool_path)
            }
            BinaryType::Agent => content_template
                .replace("{agent_name}", binary_name)
                .replace("{config_dir}", &paths::config_dir().to_string_lossy()),
        }
        .replace(
            "{prompt_utils_file}",
            &paths::bash_prompt_utils_file().to_string_lossy(),
        );

        if let Some(rt) = custom_runtime
            && let Some(newline_pos) = content.find('\n')
        {
            content = format!("#!/usr/bin/env {rt}{}", &content[newline_pos..]);
        }

        if language == Language::TypeScript {
            let bin_dir = binary_file
                .parent()
                .expect("Failed to get parent directory of binary file");
            let script_file = bin_dir.join(format!("run-{binary_name}.ts"));
            if script_file.exists() {
                fs::remove_file(&script_file)?;
            }
            let mut sf = File::create(&script_file)?;
            sf.write_all(content.as_bytes())?;
            fs::set_permissions(&script_file, fs::Permissions::from_mode(0o755))?;

            let ts_runtime = custom_runtime.unwrap_or("tsx");
            let wrapper = format!(
                "#!/bin/sh\nexec {ts_runtime} \"{}\" \"$@\"\n",
                script_file.display()
            );
            if binary_file.exists() {
                fs::remove_file(&binary_file)?;
            }
            let mut wf = File::create(&binary_file)?;
            wf.write_all(wrapper.as_bytes())?;
            fs::set_permissions(&binary_file, fs::Permissions::from_mode(0o755))?;
        } else {
            if binary_file.exists() {
                fs::remove_file(&binary_file)?;
            }
            let mut file = File::create(&binary_file)?;
            file.write_all(content.as_bytes())?;
            fs::set_permissions(&binary_file, fs::Permissions::from_mode(0o755))?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: JsonSchema,
    #[serde(skip_serializing, default)]
    pub agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JsonSchema {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<IndexMap<String, JsonSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<JsonSchema>>,
    #[serde(rename = "anyOf", skip_serializing_if = "Option::is_none")]
    pub any_of: Option<Vec<JsonSchema>>,
    #[serde(rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_value: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
}

impl JsonSchema {
    pub fn is_empty_properties(&self) -> bool {
        match &self.properties {
            Some(v) => v.is_empty(),
            None => true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub id: Option<String>,
    /// Gemini 3's thought signature for stateful reasoning in function calling.
    /// Must be preserved and sent back when submitting function responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

type CallConfig = (String, String, Vec<String>, HashMap<String, String>);

impl ToolCall {
    pub fn dedup(calls: Vec<Self>) -> Vec<Self> {
        let mut new_calls = vec![];
        let mut seen_ids = HashSet::new();

        for call in calls.into_iter().rev() {
            if let Some(id) = &call.id {
                if !seen_ids.contains(id) {
                    seen_ids.insert(id.clone());
                    new_calls.push(call);
                }
            } else {
                new_calls.push(call);
            }
        }

        new_calls.reverse();
        new_calls
    }

    pub fn new(name: String, arguments: Value, id: Option<String>) -> Self {
        Self {
            name,
            arguments,
            id,
            thought_signature: None,
        }
    }

    pub fn with_thought_signature(mut self, thought_signature: Option<String>) -> Self {
        self.thought_signature = thought_signature;
        self
    }

    pub async fn eval(&self, ctx: &mut RequestContext) -> Result<Value> {
        let agent = ctx.agent.clone();
        let functions = ctx.tool_scope.functions.clone();
        let current_depth = ctx.current_depth;
        let agent_name = agent.as_ref().map(|agent| agent.name().to_owned());
        let (call_name, cmd_name, mut cmd_args, envs) = match agent.as_ref() {
            Some(agent) => self.extract_call_config_from_agent(&functions, agent)?,
            None => self.extract_call_config_from_ctx(&functions)?,
        };

        let json_data = if self.arguments.is_object() {
            self.arguments.clone()
        } else if let Some(arguments) = self.arguments.as_str() {
            let arguments: Value = serde_json::from_str(arguments).map_err(|_| {
                anyhow!("The call '{call_name}' has invalid arguments: {arguments}")
            })?;
            arguments
        } else {
            bail!(
                "The call '{call_name}' has invalid arguments: {}",
                self.arguments
            );
        };

        cmd_args.push(json_data.to_string());

        let prompt = format!("Call {cmd_name} {}", cmd_args.join(" "));

        if *IS_STDOUT_TERMINAL && current_depth == 0 {
            println!("{}", dimmed_text(&prompt));
        }

        let output = match cmd_name.as_str() {
            _ if cmd_name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX) => {
                Self::search_mcp_tools(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP search failed: {e}");
                        eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX) => {
                Self::describe_mcp_tool(ctx, &cmd_name, json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP describe failed: {e}");
                        eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX) => {
                Self::invoke_mcp_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP tool invocation failed: {e}");
                        eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(TODO_FUNCTION_PREFIX) => {
                todo::handle_todo_tool(ctx, &cmd_name, &json_data).unwrap_or_else(|e| {
                    let error_msg = format!("Todo tool failed: {e}");
                    eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                    json!({"tool_call_error": error_msg})
                })
            }
            _ if cmd_name.starts_with(MEMORY_FUNCTION_PREFIX) => {
                memory::handle_memory_tool(ctx, &cmd_name, &json_data).unwrap_or_else(|e| {
                    let error_msg = format!("Memory tool failed: {e}");
                    eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                    json!({"tool_call_error": error_msg})
                })
            }
            _ if cmd_name.starts_with(SKILL_FUNCTION_PREFIX) => {
                skill::handle_skill_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("Skill tool failed: {e}");
                        eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(SUPERVISOR_FUNCTION_PREFIX) => {
                supervisor::handle_supervisor_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("Supervisor tool failed: {e}");
                        eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(USER_FUNCTION_PREFIX) => {
                user_interaction::handle_user_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("User interaction failed: {e}");
                        eprintln!("{}", warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ => match run_llm_function(cmd_name, cmd_args, envs, agent_name) {
                Ok(Some(contents)) => serde_json::from_str(&contents)
                    .ok()
                    .unwrap_or_else(|| json!({"output": contents})),
                Ok(None) => Value::Null,
                Err(e) => serde_json::from_str(&e.to_string())
                    .ok()
                    .unwrap_or_else(|| json!({"output": e.to_string()})),
            },
        };

        Ok(output)
    }

    async fn describe_mcp_tool(
        ctx: &RequestContext,
        cmd_name: &str,
        json_data: Value,
    ) -> Result<Value> {
        let server_id = cmd_name.replace(&format!("{MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX}_"), "");
        let tool = json_data
            .get("tool")
            .ok_or_else(|| anyhow!("Missing 'tool' in arguments"))?
            .as_str()
            .ok_or_else(|| anyhow!("Invalid 'tool' in arguments"))?;
        let result = ctx
            .tool_scope
            .mcp_runtime
            .describe(&server_id, tool)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    async fn search_mcp_tools(
        ctx: &RequestContext,
        cmd_name: &str,
        json_data: &Value,
    ) -> Result<Value> {
        let server = cmd_name.replace(&format!("{MCP_SEARCH_META_FUNCTION_NAME_PREFIX}_"), "");
        let query = json_data
            .get("query")
            .ok_or_else(|| anyhow!("Missing 'query' in arguments"))?
            .as_str()
            .ok_or_else(|| anyhow!("Invalid 'query' in arguments"))?;
        let top_k = json_data
            .get("top_k")
            .cloned()
            .unwrap_or_else(|| Value::from(8u64))
            .as_u64()
            .ok_or_else(|| anyhow!("Invalid 'top_k' in arguments"))? as usize;

        let catalog_items = ctx
            .tool_scope
            .mcp_runtime
            .search(&server, query, top_k)
            .await?
            .into_iter()
            .map(|it| serde_json::to_value(&it).unwrap_or_default())
            .collect();
        Ok(Value::Array(catalog_items))
    }

    async fn invoke_mcp_tool(
        ctx: &RequestContext,
        cmd_name: &str,
        json_data: &Value,
    ) -> Result<Value> {
        let server = cmd_name.replace(&format!("{MCP_INVOKE_META_FUNCTION_NAME_PREFIX}_"), "");
        let tool = json_data
            .get("tool")
            .ok_or_else(|| anyhow!("Missing 'tool' in arguments"))?
            .as_str()
            .ok_or_else(|| anyhow!("Invalid 'tool' in arguments"))?;
        let arguments = json_data
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let result = ctx
            .tool_scope
            .mcp_runtime
            .invoke(&server, tool, arguments)
            .await?;
        Ok(serde_json::to_value(result)?)
    }

    fn extract_call_config_from_agent(
        &self,
        functions: &Functions,
        agent: &Agent,
    ) -> Result<CallConfig> {
        let function_name = self.name.clone();
        match agent.functions().find(&function_name) {
            Some(function) => {
                let agent_name = agent.name().to_string();
                if function.agent {
                    Ok((
                        format!("{agent_name}-{function_name}"),
                        agent_name,
                        vec![function_name],
                        agent.variable_envs(),
                    ))
                } else {
                    Ok((
                        function_name.clone(),
                        function_name,
                        vec![],
                        agent.variable_envs(),
                    ))
                }
            }
            None => self.extract_call_config_from_ctx(functions),
        }
    }

    fn extract_call_config_from_ctx(&self, functions: &Functions) -> Result<CallConfig> {
        let function_name = self.name.clone();
        match functions.contains(&function_name) {
            true => Ok((
                function_name.clone(),
                function_name,
                vec![],
                Default::default(),
            )),
            false => bail!("Unexpected call: {function_name} {}", self.arguments),
        }
    }
}

pub fn run_llm_function(
    cmd_name: String,
    cmd_args: Vec<String>,
    mut envs: HashMap<String, String>,
    agent_name: Option<String>,
) -> Result<Option<String>> {
    let mut bin_dirs: Vec<PathBuf> = vec![];
    let mut command_name = cmd_name.clone();
    if let Some(agent_name) = agent_name {
        command_name = cmd_args[0].clone();
        let dir = paths::agent_bin_dir(&agent_name);
        if dir.exists() {
            bin_dirs.push(dir);
        }
        if graph::agent_has_graph(&agent_name) {
            envs.insert("AUTO_CONFIRM".into(), "true".into());
        }
    } else {
        bin_dirs.push(paths::functions_bin_dir());
    }
    let current_path = env::var("PATH").context("No PATH environment variable")?;
    let prepend_path = bin_dirs
        .iter()
        .map(|v| format!("{}{PATH_SEP}", v.display()))
        .collect::<Vec<_>>()
        .join("");
    envs.insert("PATH".into(), format!("{prepend_path}{current_path}"));

    let tmp_file = temp_file("-eval-", "");
    envs.insert("LLM_OUTPUT".into(), tmp_file.display().to_string());

    #[cfg(windows)]
    let cmd_name = polyfill_cmd_name(&cmd_name, &bin_dirs);

    #[cfg(windows)]
    let cmd_args = {
        let mut args = cmd_args;
        if let Some(json_data) = args.pop() {
            let tool_data_file = temp_file("-tool-data-", ".json");
            fs::write(&tool_data_file, &json_data)?;
            envs.insert(
                "LLM_TOOL_DATA_FILE".into(),
                tool_data_file.display().to_string(),
            );
        }
        args
    };

    envs.insert("CLICOLOR_FORCE".into(), "1".into());
    envs.insert("FORCE_COLOR".into(), "1".into());

    let mut child = Command::new(&cmd_name)
        .args(&cmd_args)
        .envs(envs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow!("Unable to run {command_name}, {err}"))?;

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    let stdout_thread = std::thread::spawn(move || {
        let mut buffer = [0; 1024];
        let mut reader = stdout;
        let mut out = io::stdout();
        let mut buf = Vec::new();
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let chunk = &buffer[0..n];
            buf.extend_from_slice(chunk);
            let mut last_pos = 0;
            for (i, &byte) in chunk.iter().enumerate() {
                if byte == b'\n' {
                    let _ = out.write_all(&chunk[last_pos..i]);
                    let _ = out.write_all(b"\r\n");
                    last_pos = i + 1;
                }
            }
            if last_pos < n {
                let _ = out.write_all(&chunk[last_pos..n]);
            }
            let _ = out.flush();
        }
        buf
    });

    let stderr_thread = std::thread::spawn(move || {
        let mut buffer = [0; 1024];
        let mut reader = stderr;
        let mut err = io::stderr();
        let mut buf = Vec::new();
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let chunk = &buffer[0..n];
            buf.extend_from_slice(chunk);
            let mut last_pos = 0;
            for (i, &byte) in chunk.iter().enumerate() {
                if byte == b'\n' {
                    let _ = err.write_all(&chunk[last_pos..i]);
                    let _ = err.write_all(b"\r\n");
                    last_pos = i + 1;
                }
            }
            if last_pos < n {
                let _ = err.write_all(&chunk[last_pos..n]);
            }
            let _ = err.flush();
        }
        buf
    });

    let status = child
        .wait()
        .map_err(|err| anyhow!("Unable to run {command_name}, {err}"))?;
    let stdout_bytes = stdout_thread.join().unwrap_or_default();
    let stderr_bytes = stderr_thread.join().unwrap_or_default();

    let exit_code = status.code().unwrap_or_default();
    if exit_code != 0 {
        let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
        let stdout = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
        let tool_error_message = format!("Tool call '{command_name}' exited with code {exit_code}");
        eprintln!("{}", warning_text(&format!("⚠️ {tool_error_message} ⚠️")));
        let mut error_json = json!({"tool_call_error": tool_error_message});
        if !stderr.is_empty() {
            error_json["stderr"] = json!(stderr);
        }
        if !stdout.is_empty() {
            error_json["stdout"] = json!(stdout);
        }
        debug!("Tool call error: {error_json:?}");
        return Ok(Some(error_json.to_string()));
    }
    let mut output = None;
    if tmp_file.exists() {
        let contents =
            fs::read_to_string(tmp_file).context("Failed to retrieve tool call output")?;
        if !contents.is_empty() {
            debug!("Tool {command_name} output: {}", contents);
            output = Some(contents);
        }
    };
    Ok(output)
}

#[cfg(windows)]
fn polyfill_cmd_name<T: AsRef<Path>>(cmd_name: &str, bin_dir: &[T]) -> String {
    let cmd_name = cmd_name.to_string();
    if let Ok(exts) = env::var("PATHEXT") {
        for name in exts.split(';').map(|ext| format!("{cmd_name}{ext}")) {
            for dir in bin_dir {
                let path = dir.as_ref().join(&name);
                if path.exists() {
                    return name.to_string();
                }
            }
        }
    }
    cmd_name
}

#[derive(Debug, Clone)]
pub struct ToolCallTracker {
    last_calls: VecDeque<ToolCall>,
    max_repeats: usize,
    chain_len: usize,
}

impl ToolCallTracker {
    pub fn new(max_repeats: usize, chain_len: usize) -> Self {
        Self {
            last_calls: VecDeque::new(),
            max_repeats,
            chain_len,
        }
    }

    pub fn default() -> Self {
        Self::new(2, 3)
    }

    pub fn check_loop(&self, new_call: &ToolCall) -> Option<String> {
        if self.last_calls.len() < self.max_repeats {
            return None;
        }

        if let Some(last) = self.last_calls.back()
            && self.calls_match(last, new_call)
        {
            let mut repeat_count = 1;
            for i in (1..self.last_calls.len()).rev() {
                if self.calls_match(&self.last_calls[i - 1], &self.last_calls[i]) {
                    repeat_count += 1;
                    if repeat_count >= self.max_repeats {
                        return Some(self.create_loop_message());
                    }
                } else {
                    break;
                }
            }
        }

        let start = self.last_calls.len().saturating_sub(self.chain_len);
        let chain: Vec<_> = self.last_calls.iter().skip(start).collect();
        if chain.len() == self.chain_len {
            let mut is_repeating = true;
            for i in 0..chain.len() - 1 {
                if !self.calls_match(chain[i], chain[i + 1]) {
                    is_repeating = false;
                    break;
                }
            }
            if is_repeating && self.calls_match(chain[chain.len() - 1], new_call) {
                return Some(self.create_loop_message());
            }
        }

        None
    }

    fn calls_match(&self, a: &ToolCall, b: &ToolCall) -> bool {
        a.name == b.name && a.arguments == b.arguments
    }

    fn create_loop_message(&self) -> String {
        let message = r#"{"error":{"message":"⚠️ Tool-call loop detected! ⚠️","code":400,"param":"Use the output of the last call to this function and parameter-set then move on to the next step of workflow, change tools/parameters called, or request assistance in the conversation sream"}}"#;

        if self.last_calls.len() >= self.chain_len {
            let start = self.last_calls.len().saturating_sub(self.chain_len);
            let chain: Vec<_> = self.last_calls.iter().skip(start).collect();
            let mut loopset = "[".to_string();
            for c in chain {
                loopset +=
                    format!("{{\"name\":{},\"parameters\":{}}},", c.name, c.arguments).as_str();
            }
            let _ = loopset.pop();
            loopset.push(']');
            format!(
                "{},\"call_history\":{}}}}}",
                &message[..(&message.len() - 2)],
                loopset
            )
        } else {
            message.to_string()
        }
    }

    pub fn record_call(&mut self, call: ToolCall) {
        if self.last_calls.len() >= self.chain_len * self.max_repeats {
            self.last_calls.pop_front();
        }
        self.last_calls.push_back(call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, id: Option<&str>) -> ToolCall {
        ToolCall::new(name.to_string(), json!({}), id.map(|s| s.to_string()))
    }

    fn call_with_args(name: &str, args: Value) -> ToolCall {
        ToolCall::new(name.to_string(), args, Some("id1".to_string()))
    }

    #[test]
    fn normalize_tool_result_substitutes_done_for_null() {
        assert_eq!(normalize_tool_result(Value::Null), json!("DONE"));
    }

    #[test]
    fn normalize_tool_result_preserves_non_null_values() {
        assert_eq!(
            normalize_tool_result(json!({"output": "hi"})),
            json!({"output": "hi"})
        );
        assert_eq!(normalize_tool_result(json!("")), json!(""));
        assert_eq!(normalize_tool_result(json!(false)), json!(false));
    }

    #[test]
    fn toolcall_new_sets_fields() {
        let tc = ToolCall::new("my_tool".into(), json!({"x": 1}), Some("call-1".into()));
        assert_eq!(tc.name, "my_tool");
        assert_eq!(tc.arguments, json!({"x": 1}));
        assert_eq!(tc.id, Some("call-1".to_string()));
        assert!(tc.thought_signature.is_none());
    }

    #[test]
    fn toolcall_default_has_empty_fields() {
        let tc = ToolCall::default();
        assert_eq!(tc.name, "");
        assert_eq!(tc.arguments, Value::Null);
        assert!(tc.id.is_none());
        assert!(tc.thought_signature.is_none());
    }

    #[test]
    fn direct_invoker_maps_each_language() {
        assert_eq!(
            Language::Bash.direct_invoker(),
            Some(("bash", &[] as &[&str]))
        );
        assert_eq!(
            Language::Python.direct_invoker(),
            Some(("python3", &[] as &[&str]))
        );
        assert_eq!(
            Language::TypeScript.direct_invoker(),
            Some(("npx", &["tsx"] as &[&str]))
        );
        assert_eq!(Language::Unsupported.direct_invoker(), None);
    }

    #[test]
    fn toolcall_with_thought_signature() {
        let tc = ToolCall::new("t".into(), json!({}), None)
            .with_thought_signature(Some("sig123".into()));
        assert_eq!(tc.thought_signature, Some("sig123".to_string()));
    }

    #[test]
    fn toolcall_with_thought_signature_none() {
        let tc = ToolCall::new("t".into(), json!({}), None).with_thought_signature(None);
        assert!(tc.thought_signature.is_none());
    }

    #[test]
    fn dedup_keeps_unique_ids() {
        let calls = vec![call("tool_a", Some("id-1")), call("tool_b", Some("id-2"))];
        let result = ToolCall::dedup(calls);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedup_keeps_calls_without_ids() {
        let calls = vec![call("tool_a", None), call("tool_b", None)];
        let result = ToolCall::dedup(calls);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn dedup_removes_duplicate_ids_keeps_last() {
        let calls = vec![call("tool_a", Some("id-1")), call("tool_b", Some("id-1"))];
        let result = ToolCall::dedup(calls);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "tool_b");
    }

    #[test]
    fn dedup_empty_input_returns_empty() {
        let result = ToolCall::dedup(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn dedup_mixed_with_and_without_ids() {
        let calls = vec![
            call("a", Some("id-1")),
            call("b", None),
            call("c", Some("id-1")),
            call("d", None),
        ];
        let result = ToolCall::dedup(calls);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "b");
        assert_eq!(result[1].name, "c");
        assert_eq!(result[2].name, "d");
    }

    #[test]
    fn tracker_default_values() {
        let tracker = ToolCallTracker::default();
        assert_eq!(tracker.max_repeats, 2);
        assert_eq!(tracker.chain_len, 3);
        assert!(tracker.last_calls.is_empty());
    }

    #[test]
    fn tracker_no_loop_on_fresh_tracker() {
        let tracker = ToolCallTracker::default();
        assert!(tracker.check_loop(&call("tool", None)).is_none());
    }

    #[test]
    fn tracker_no_loop_below_threshold() {
        let mut tracker = ToolCallTracker::new(3, 5);
        let c = call_with_args("tool", json!({"a": 1}));
        tracker.record_call(c.clone());
        tracker.record_call(c.clone());
        assert!(tracker.check_loop(&c).is_none());
    }

    #[test]
    fn tracker_detects_loop_at_max_repeats() {
        let mut tracker = ToolCallTracker::new(2, 3);
        let c = call_with_args("tool", json!({"a": 1}));
        tracker.record_call(c.clone());
        tracker.record_call(c.clone());
        let result = tracker.check_loop(&c);
        assert!(result.is_some());
        assert!(result.unwrap().contains("loop"));
    }

    #[test]
    fn tracker_different_args_no_loop() {
        let mut tracker = ToolCallTracker::new(2, 3);
        tracker.record_call(call_with_args("tool", json!({"a": 1})));
        tracker.record_call(call_with_args("tool", json!({"a": 2})));
        let new_call = call_with_args("tool", json!({"a": 3}));
        assert!(tracker.check_loop(&new_call).is_none());
    }

    #[test]
    fn tracker_different_names_no_loop() {
        let mut tracker = ToolCallTracker::new(2, 3);
        tracker.record_call(call_with_args("tool_a", json!({})));
        tracker.record_call(call_with_args("tool_b", json!({})));
        let new_call = call_with_args("tool_a", json!({}));
        assert!(tracker.check_loop(&new_call).is_none());
    }

    #[test]
    fn tracker_chain_detection() {
        let mut tracker = ToolCallTracker::new(2, 3);
        let c = call_with_args("tool", json!({"x": "same"}));
        tracker.record_call(c.clone());
        tracker.record_call(c.clone());
        tracker.record_call(c.clone());
        let result = tracker.check_loop(&c);
        assert!(result.is_some());
    }

    #[test]
    fn tracker_record_call_respects_capacity() {
        let mut tracker = ToolCallTracker::new(2, 2);
        for i in 0..10 {
            tracker.record_call(call_with_args(&format!("tool_{i}"), json!({})));
        }
        assert!(tracker.last_calls.len() <= 2 * 2);
    }

    #[test]
    fn tracker_loop_message_contains_call_history() {
        let mut tracker = ToolCallTracker::new(2, 3);
        let c = call_with_args("repeat_tool", json!({"k": "v"}));
        tracker.record_call(c.clone());
        tracker.record_call(c.clone());
        tracker.record_call(c.clone());
        let msg = tracker.check_loop(&c).unwrap();
        assert!(msg.contains("call_history"));
        assert!(msg.contains("repeat_tool"));
    }

    #[test]
    fn prefix_constants_are_correct() {
        assert_eq!(TODO_FUNCTION_PREFIX, "todo__");
        assert_eq!(SUPERVISOR_FUNCTION_PREFIX, "agent__");
        assert_eq!(USER_FUNCTION_PREFIX, "user__");
        assert_eq!(MCP_INVOKE_META_FUNCTION_NAME_PREFIX, "mcp_invoke");
        assert_eq!(MCP_SEARCH_META_FUNCTION_NAME_PREFIX, "mcp_search");
        assert_eq!(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, "mcp_describe");
    }

    #[test]
    fn functions_default_is_empty() {
        let f = Functions::default();
        assert!(f.is_empty());
        assert!(f.declarations().is_empty());
    }

    #[test]
    fn bundled_bash_tools_generate_declarations() {
        let tools_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/functions/tools");
        let mut checked = Vec::new();
        for entry in std::fs::read_dir(&tools_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(OsStr::to_str) != Some("sh") {
                continue;
            }
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let declarations = Functions::generate_declarations(&path)
                .unwrap_or_else(|e| panic!("bundled tool '{name}' failed to parse: {e}"));
            assert!(
                !declarations.is_empty(),
                "bundled tool '{name}' produced no function declaration"
            );
            checked.push(name);
        }
        for expected in ["fs_grep", "ast_grep", "execute_command"] {
            assert!(
                checked.iter().any(|n| n == expected),
                "expected bundled tool '{expected}' to be checked; found {checked:?}"
            );
        }
    }

    #[test]
    fn functions_append_todo_adds_declarations() {
        let mut f = Functions::default();
        f.append_todo_functions();
        assert!(!f.is_empty());
        assert!(f.contains("todo__init"));
        assert!(f.contains("todo__add"));
        assert!(f.contains("todo__done"));
        assert!(f.contains("todo__list"));
        assert!(f.contains("todo__clear"));
    }

    #[test]
    fn functions_append_supervisor_adds_declarations() {
        let mut f = Functions::default();
        f.append_supervisor_functions();
        assert!(f.contains("agent__spawn"));
        assert!(f.contains("agent__check"));
        assert!(f.contains("agent__collect"));
        assert!(f.contains("agent__list"));
        assert!(f.contains("agent__cancel"));
        assert!(f.contains("agent__reply_escalation"));
    }

    #[test]
    fn functions_append_teammate_adds_declarations() {
        let mut f = Functions::default();
        f.append_teammate_functions();
        assert!(f.contains("agent__send_message"));
        assert!(f.contains("agent__check_inbox"));
    }

    #[test]
    fn functions_append_user_interaction_adds_declarations() {
        let mut f = Functions::default();
        f.append_user_interaction_functions();
        assert!(f.contains("user__ask"));
        assert!(f.contains("user__confirm"));
        assert!(f.contains("user__input"));
        assert!(f.contains("user__checkbox"));
    }

    #[test]
    fn functions_append_mcp_meta_creates_three_per_server() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec!["github".to_string()]);
        assert_eq!(f.declarations().len(), 3);
        assert!(f.contains("mcp_invoke_github"));
        assert!(f.contains("mcp_search_github"));
        assert!(f.contains("mcp_describe_github"));
    }

    #[test]
    fn functions_append_mcp_meta_multiple_servers() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec!["github".into(), "slack".into()]);
        assert_eq!(f.declarations().len(), 6);
        assert!(f.contains("mcp_invoke_github"));
        assert!(f.contains("mcp_invoke_slack"));
    }

    #[test]
    fn functions_append_mcp_meta_empty_servers() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec![]);
        assert!(f.is_empty());
    }

    #[test]
    fn functions_find_returns_declaration() {
        let mut f = Functions::default();
        f.append_todo_functions();
        let decl = f.find("todo__init");
        assert!(decl.is_some());
        assert_eq!(decl.unwrap().name, "todo__init");
    }

    #[test]
    fn functions_find_returns_none_for_missing() {
        let f = Functions::default();
        assert!(f.find("nonexistent").is_none());
    }

    #[test]
    fn functions_contains_true_for_existing() {
        let mut f = Functions::default();
        f.append_todo_functions();
        assert!(f.contains("todo__init"));
    }

    #[test]
    fn functions_contains_false_for_missing() {
        let f = Functions::default();
        assert!(!f.contains("todo__init"));
    }

    #[test]
    fn functions_mcp_invoke_declaration_has_tool_and_arguments_params() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec!["srv".to_string()]);
        let decl = f.find("mcp_invoke_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        assert!(props.contains_key("tool"));
        assert!(props.contains_key("arguments"));
        let required = decl.parameters.required.as_ref().unwrap();
        assert!(required.contains(&"tool".to_string()));
    }

    #[test]
    fn functions_mcp_search_declaration_has_query_and_top_k_params() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec!["srv".to_string()]);
        let decl = f.find("mcp_search_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("top_k"));
    }

    #[test]
    fn functions_mcp_describe_declaration_has_tool_param() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec!["srv".to_string()]);
        let decl = f.find("mcp_describe_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        assert!(props.contains_key("tool"));
    }

    #[test]
    fn functions_supervisor_includes_task_queue_tools() {
        let mut f = Functions::default();
        f.append_supervisor_functions();
        assert!(f.contains("agent__task_create"));
        assert!(f.contains("agent__task_list"));
        assert!(f.contains("agent__task_complete"));
        assert!(f.contains("agent__task_fail"));
    }

    #[test]
    fn tool_result_stores_call_and_output() {
        let tc = call("my_tool", Some("id-1"));
        let result = ToolResult::new(tc.clone(), json!({"result": "ok"}));
        assert_eq!(result.call.name, "my_tool");
        assert_eq!(result.output, json!({"result": "ok"}));
    }
}
