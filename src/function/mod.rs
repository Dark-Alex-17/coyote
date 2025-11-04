use crate::{
    config::{Agent, Config, GlobalConfig},
    utils::*,
};

use crate::config::ensure_parent_exists;
use crate::mcp::{MCP_INVOKE_META_FUNCTION_NAME_PREFIX, MCP_LIST_META_FUNCTION_NAME_PREFIX};
use crate::parsers::{bash, python};
use anyhow::{anyhow, bail, Context, Result};
use indexmap::IndexMap;
use indoc::formatdoc;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
};
use strum_macros::AsRefStr;

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
enum Language {
    Bash,
    Python,
    Unsupported,
}

impl From<&String> for Language {
    fn from(s: &String) -> Self {
        match s.to_lowercase().as_str() {
            "sh" => Language::Bash,
            "py" => Language::Python,
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
            Language::Unsupported => "sh",
        }
    }

    fn to_extension(self) -> &'static str {
        match self {
            Language::Bash => "sh",
            Language::Python => "py",
            _ => "sh",
        }
    }
}

pub async fn eval_tool_calls(
    config: &GlobalConfig,
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
    let mut is_all_null = true;
    for call in calls {
        let mut result = call.eval(config).await?;
        if result.is_null() {
            result = json!("DONE");
        } else {
            is_all_null = false;
        }
        output.push(ToolResult::new(call, result));
    }
    if is_all_null {
        output = vec![];
    }
    Ok(output)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self { call, output }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Functions {
    declarations: Vec<FunctionDeclaration>,
}

impl Functions {
    fn install_global_tools() -> Result<()> {
        info!(
            "Installing global built-in functions in {}",
            Config::functions_dir().display()
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
            let file_path = Config::functions_dir().join(file.as_ref());
            let file_extension = file_path
                .extension()
                .and_then(OsStr::to_str)
                .map(|s| s.to_lowercase());
            #[cfg_attr(not(unix), expect(unused))]
            let is_script = matches!(file_extension.as_deref(), Some("sh") | Some("py"));

            if file_path.exists() {
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

    pub fn init() -> Result<Self> {
        Self::install_global_tools()?;
        Self::clear_global_functions_bin_dir()?;
        info!(
            "Initializing global functions from {}",
            Config::global_tools_file().display()
        );

        let declarations = Self {
            declarations: Self::build_global_tool_declarations_from_path(
                &Config::global_tools_file(),
            )?,
        };

        info!(
            "Building global function binaries in {}",
            Config::functions_bin_dir().display()
        );
        Self::build_global_function_binaries_from_path(Config::global_tools_file())?;

        Ok(declarations)
    }

    pub fn init_agent(name: &str, global_tools: &[String]) -> Result<Self> {
        Self::install_global_tools()?;
        Self::clear_agent_bin_dir(name)?;

        let global_tools_declarations = if !global_tools.is_empty() {
            let enabled_tools = global_tools.join("\n");
            info!("Loading global tools for agent: {name}: {enabled_tools}");
            let tools_declarations = Self::build_global_tool_declarations(&enabled_tools)?;

            info!(
                "Building global function binaries required by agent: {name} in {}",
                Config::functions_bin_dir().display()
            );
            Self::build_global_function_binaries(&enabled_tools, Some(name))?;
            tools_declarations
        } else {
            debug!("No global tools found for agent: {}", name);
            Vec::new()
        };
        let agent_script_declarations = match Config::agent_functions_file(name) {
            Ok(path) if path.exists() => {
                info!(
                    "Loading functions script for agent: {name} from {}",
                    path.display()
                );
                let script_declarations = Self::generate_declarations(&path)?;
                debug!("agent_declarations: {:#?}", script_declarations);

                info!(
                    "Building function binary for agent: {name} in {}",
                    Config::agent_bin_dir(name).display()
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

    pub fn clear_mcp_meta_functions(&mut self) {
        self.declarations.retain(|d| {
            !d.name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX)
                && !d.name.starts_with(MCP_LIST_META_FUNCTION_NAME_PREFIX)
        });
    }

    pub fn append_mcp_meta_functions(&mut self, mcp_servers: Vec<String>) {
        let mut invoke_function_properties = IndexMap::new();
        invoke_function_properties.insert(
            "server".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                ..Default::default()
            },
        );
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

        for server in mcp_servers {
            let invoke_function_name = format!("{}_{server}", MCP_INVOKE_META_FUNCTION_NAME_PREFIX);
            let invoke_function_declaration = FunctionDeclaration {
                name: invoke_function_name.clone(),
                description: formatdoc!(
                    r#"
										Invoke the specified tool on the {server} MCP server. Always call {invoke_function_name} first to find the
										correct names of tools before calling '{invoke_function_name}'.
										"#
                ),
                parameters: JsonSchema {
                    type_value: Some("object".to_string()),
                    properties: Some(invoke_function_properties.clone()),
                    required: Some(vec!["server".to_string(), "tool".to_string()]),
                    ..Default::default()
                },
                agent: false,
            };
            let list_functions_declaration = FunctionDeclaration {
                name: format!("{}_{}", MCP_LIST_META_FUNCTION_NAME_PREFIX, server),
                description: format!("List all the available tools for the {server} MCP server"),
                parameters: JsonSchema::default(),
                agent: false,
            };
            self.declarations.push(invoke_function_declaration);
            self.declarations.push(list_functions_declaration);
        }
    }

    fn build_global_tool_declarations(enabled_tools: &str) -> Result<Vec<FunctionDeclaration>> {
        let global_tools_directory = Config::global_tools_dir();
        let mut function_declarations = Vec::new();

        for line in enabled_tools.lines() {
            if line.starts_with('#') {
                continue;
            }

            let declaration = Self::generate_declarations(&global_tools_directory.join(line))?;
            function_declarations.extend(declaration);
        }

        Ok(function_declarations)
    }

    fn build_global_tool_declarations_from_path(
        tools_txt_path: &PathBuf,
    ) -> Result<Vec<FunctionDeclaration>> {
        let enabled_tools = fs::read_to_string(tools_txt_path)
            .with_context(|| format!("failed to load functions at {}", tools_txt_path.display()))?;

        Self::build_global_tool_declarations(&enabled_tools)
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

    fn build_global_function_binaries(enabled_tools: &str, agent_name: Option<&str>) -> Result<()> {
        for line in enabled_tools.lines() {
            if line.starts_with('#') {
                continue;
            }

            let language = Language::from(
                &Path::new(line)
                    .extension()
                    .and_then(OsStr::to_str)
                    .map(|s| s.to_lowercase())
                    .ok_or_else(|| {
                        anyhow::format_err!("Unable to extract file extension from path: {line:?}")
                    })?,
            );
            let binary_name = Path::new(line)
                .file_stem()
                .and_then(OsStr::to_str)
                .ok_or_else(|| {
                    anyhow::format_err!("Unable to extract file name from path: {line:?}")
                })?;

            if language == Language::Unsupported {
                bail!("Unsupported tool file extension: {}", language.as_ref());
            }

            Self::build_binaries(binary_name, language, BinaryType::Tool(agent_name))?;
        }

        Ok(())
    }

    fn build_global_function_binaries_from_path(tools_txt_path: PathBuf) -> Result<()> {
        let enabled_tools = fs::read_to_string(&tools_txt_path)
            .with_context(|| format!("failed to load functions at {}", tools_txt_path.display()))?;

        Self::build_global_function_binaries(&enabled_tools, None)
    }

    fn clear_agent_bin_dir(name: &str) -> Result<()> {
        let agent_bin_directory = Config::agent_bin_dir(name);
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
        let bin_dir = Config::functions_bin_dir();
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
        let language = Language::from(
            &Config::agent_functions_file(name)?
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

        Self::build_binaries(name, language, BinaryType::Agent)
    }

    #[cfg(windows)]
    fn build_binaries(
        binary_name: &str,
        language: Language,
        binary_type: BinaryType,
    ) -> Result<()> {
        use native::runtime;
        let (binary_file, binary_script_file) = match binary_type {
            BinaryType::Tool(None) => (
                Config::functions_bin_dir().join(format!("{binary_name}.cmd")),
                Config::functions_bin_dir()
                    .join(format!("run-{binary_name}.{}", language.to_extension())),
            ),
            BinaryType::Tool(Some(agent_name)) => (
                Config::agent_bin_dir(agent_name).join(format!("{binary_name}.cmd")),
                Config::agent_bin_dir(agent_name)
                    .join(format!("run-{binary_name}.{}", language.to_extension())),
            ),
            BinaryType::Agent => (
                Config::agent_bin_dir(binary_name).join(format!("{binary_name}.cmd")),
                Config::agent_bin_dir(binary_name)
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
        let content = match binary_type {
            BinaryType::Tool(None) => {
                let root_dir = Config::functions_dir();
                let tool_path = format!(
                    "{}/{binary_name}",
                    &Config::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &root_dir.to_string_lossy())
                    .replace("{tool_path}", &tool_path)
            }
            BinaryType::Tool(Some(agent_name)) => {
                let root_dir = Config::agent_data_dir(agent_name);
                let tool_path = format!(
                    "{}/{binary_name}",
                    &Config::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &root_dir.to_string_lossy())
                    .replace("{tool_path}", &tool_path)
            }
            BinaryType::Agent => content_template
                .replace("{agent_name}", binary_name)
                .replace("{config_dir}", &Config::config_dir().to_string_lossy()),
        }
        .replace(
            "{prompt_utils_file}",
            &Config::bash_prompt_utils_file().to_string_lossy(),
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

        let run = match language {
            Language::Bash => {
                let shell = runtime::bash_path().ok_or_else(|| anyhow!("Shell not found"))?;
                format!("{shell} --noprofile --norc")
            }
            Language::Python if Path::new(".venv").exists() => {
                let executable_path = env::current_dir()?
                    .join(".venv")
                    .join("Scripts")
                    .join("activate.bat");
                let canonicalized_path = fs::canonicalize(&executable_path)?;
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
                let canonicalized_path = fs::canonicalize(&executable_path)?;
                canonicalized_path.to_string_lossy().into_owned()
            }
            _ => bail!("Unsupported language: {}", language.as_ref()),
        };
        let bin_dir = binary_file
            .parent()
            .expect("Failed to get parent directory of binary file")
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let wrapper_binary = binary_script_file
            .canonicalize()?
            .to_string_lossy()
            .into_owned();
        let content = formatdoc!(
            r#"
						@echo off
						setlocal

						set "bin_dir={bin_dir}"

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
    ) -> Result<()> {
        use std::os::unix::prelude::PermissionsExt;

        let binary_file = match binary_type {
            BinaryType::Tool(None) => Config::functions_bin_dir().join(binary_name),
            BinaryType::Tool(Some(agent_name)) => {
                Config::agent_bin_dir(agent_name).join(binary_name)
            }
            BinaryType::Agent => Config::agent_bin_dir(binary_name).join(binary_name),
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
        let content = match binary_type {
            BinaryType::Tool(None) => {
                let root_dir = Config::functions_dir();
                let tool_path = format!(
                    "{}/{binary_name}",
                    &Config::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &root_dir.to_string_lossy())
                    .replace("{tool_path}", &tool_path)
            }
            BinaryType::Tool(Some(agent_name)) => {
                let root_dir = Config::agent_data_dir(agent_name);
                let tool_path = format!(
                    "{}/{binary_name}",
                    &Config::global_tools_dir().to_string_lossy()
                );
                content_template
                    .replace("{function_name}", binary_name)
                    .replace("{root_dir}", &root_dir.to_string_lossy())
                    .replace("{tool_path}", &tool_path)
            }
            BinaryType::Agent => content_template
                .replace("{agent_name}", binary_name)
                .replace("{config_dir}", &Config::config_dir().to_string_lossy()),
        }
        .replace(
            "{prompt_utils_file}",
            &Config::bash_prompt_utils_file().to_string_lossy(),
        );
        if binary_file.exists() {
            fs::remove_file(&binary_file)?;
        }
        let mut file = File::create(&binary_file)?;
        file.write_all(content.as_bytes())?;

        fs::set_permissions(&binary_file, fs::Permissions::from_mode(0o755))?;

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
        }
    }

    pub async fn eval(&self, config: &GlobalConfig) -> Result<Value> {
        let (call_name, cmd_name, mut cmd_args, envs) = match &config.read().agent {
            Some(agent) => self.extract_call_config_from_agent(config, agent)?,
            None => self.extract_call_config_from_config(config)?,
        };
        let agent_name = config
            .read()
            .agent
            .as_ref()
            .map(|agent| agent.name().to_owned());

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

        if *IS_STDOUT_TERMINAL {
            println!("{}", dimmed_text(&prompt));
        }

        let output = match cmd_name.as_str() {
            _ if cmd_name.starts_with(MCP_LIST_META_FUNCTION_NAME_PREFIX) => {
                let registry_arc = {
                    let cfg = config.read();
                    cfg.mcp_registry
                        .clone()
                        .with_context(|| "MCP is not configured")?
                };

                registry_arc.catalog().await?
            }
            _ if cmd_name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX) => {
                let server = json_data
                    .get("server")
                    .ok_or_else(|| anyhow!("Missing 'server' in arguments"))?
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid 'server' in arguments"))?;
                let tool = json_data
                    .get("tool")
                    .ok_or_else(|| anyhow!("Missing 'tool' in arguments"))?
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid 'tool' in arguments"))?;
                let arguments = json_data
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let registry_arc = {
                    let cfg = config.read();
                    cfg.mcp_registry
                        .clone()
                        .with_context(|| "MCP is not configured")?
                };
                let result = registry_arc.invoke(server, tool, arguments).await?;
                serde_json::to_value(result)?
            }
            _ => match run_llm_function(cmd_name, cmd_args, envs, agent_name)? {
                Some(contents) => serde_json::from_str(&contents)
                    .ok()
                    .unwrap_or_else(|| json!({"output": contents})),
                None => Value::Null,
            },
        };

        Ok(output)
    }

    fn extract_call_config_from_agent(
        &self,
        config: &GlobalConfig,
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
                        Default::default(),
                    ))
                }
            }
            None => self.extract_call_config_from_config(config),
        }
    }

    fn extract_call_config_from_config(&self, config: &GlobalConfig) -> Result<CallConfig> {
        let function_name = self.name.clone();
        match config.read().functions.contains(&function_name) {
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
    if let Some(agent_name) = agent_name {
        let dir = Config::agent_bin_dir(&agent_name);
        if dir.exists() {
            bin_dirs.push(dir);
        }
    } else {
        bin_dirs.push(Config::functions_bin_dir());
    }
    let current_path = env::var("PATH").context("No PATH environment variable")?;
    let prepend_path = bin_dirs
        .iter()
        .map(|v| format!("{}{PATH_SEP}", v.display()))
        .collect::<Vec<_>>()
        .join("");
    envs.insert("PATH".into(), format!("{prepend_path}{current_path}"));

    let temp_file = temp_file("-eval-", "");
    envs.insert("LLM_OUTPUT".into(), temp_file.display().to_string());

    #[cfg(windows)]
    let cmd_name = polyfill_cmd_name(&cmd_name, &bin_dirs);

    let exit_code = run_command(&cmd_name, &cmd_args, Some(envs))
        .map_err(|err| anyhow!("Unable to run {cmd_name}, {err}"))?;
    if exit_code != 0 {
        bail!("Tool call exited with {exit_code}");
    }
    let mut output = None;
    if temp_file.exists() {
        let contents =
            fs::read_to_string(temp_file).context("Failed to retrieve tool call output")?;
        if !contents.is_empty() {
            debug!("Tool {cmd_name} output: {}", contents);
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
