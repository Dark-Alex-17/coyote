pub(crate) mod memory;
pub(crate) mod rag_query;
pub(crate) mod skill;
pub(crate) mod supervisor;
pub(crate) mod todo;
pub(crate) mod user_interaction;

use crate::{
    client::ThinkingBlock,
    config::{
        Agent, RequestContext, flatten_prompt_messages, resolve_prompt_args, sanitize_display_text,
    },
    graph,
    utils::*,
};

use crate::config::ensure_parent_exists;
use crate::config::paths;
use crate::mcp::{
    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
    MCP_META_FUNCTION_PREFIXES, MCP_PROMPT_META_FUNCTION_NAME_PREFIX,
    MCP_READ_META_FUNCTION_NAME_PREFIX, MCP_SEARCH_META_FUNCTION_NAME_PREFIX, McpServerFeatures,
    McpServersConfig, is_mcp_meta_function, render,
};
use crate::parsers::{bash, python, typescript};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future;
use indexmap::IndexMap;
use indoc::formatdoc;
use memory::MEMORY_FUNCTION_PREFIX;
use rag_query::RAG_FUNCTION_PREFIX;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use skill::SKILL_FUNCTION_PREFIX;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::VecDeque, thread};
use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
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

pub(crate) fn write_file_atomic(
    path: &Path,
    content: &str,
    #[cfg_attr(not(unix), expect(unused))] mode: Option<u32>,
) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        #[cfg(unix)]
        if let Some(mode) = mode {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }

        return Ok(());
    }

    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow!("Unable to extract file name from path: {}", path.display()))?;
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let write_synced = || -> io::Result<()> {
        use std::io::Write;
        let mut file = File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()
    };
    if let Err(err) = write_synced() {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }

    #[cfg(unix)]
    if let Some(mode) = mode {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }

    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }

    Ok(())
}

fn tool_source_stems() -> Result<HashSet<String>> {
    let mut stems = HashSet::new();
    let tools_dir = paths::global_tools_dir();
    if !tools_dir.exists() {
        return Ok(stems);
    }

    for entry in fs::read_dir(&tools_dir)? {
        let path = entry?.path();
        if path.is_file()
            && let Some(stem) = path.file_stem().and_then(OsStr::to_str)
        {
            stems.insert(stem.to_string());
        }
    }

    Ok(stems)
}

fn bin_entry_stem(file_name: &str) -> &str {
    let name = file_name.strip_prefix("run-").unwrap_or(file_name);
    Path::new(name)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or(name)
}

fn prune_stale_bin_entries(
    bin_dir: &Path,
    valid_stems: &HashSet<String>,
    extra_valid_stem: Option<&str>,
) -> Result<()> {
    if !bin_dir.exists() {
        fs::create_dir_all(bin_dir)?;
        return Ok(());
    }

    for entry in fs::read_dir(bin_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            debug!(
                "Removing unexpected directory in bin dir: {}",
                path.display()
            );
            fs::remove_dir_all(&path)?;
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let stem = bin_entry_stem(file_name);

        if valid_stems.contains(stem) || extra_valid_stem == Some(stem) {
            continue;
        }

        debug!("Removing stale bin entry: {}", path.display());
        fs::remove_file(&path)?;
    }

    Ok(())
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
    let mut to_execute: Vec<(usize, ToolCall)> = Vec::with_capacity(calls.len());
    let mut indexed_results: Vec<(usize, ToolResult)> = vec![];
    for (idx, call) in calls.into_iter().enumerate() {
        if let Some(msg) = ctx.tool_scope.tool_tracker.check_loop(&call.clone()) {
            let dup_msg = format!("{{\"tool_call_loop_alert\":{}}}", msg.trim());
            println!(
                "{}",
                muted_warning_text(
                    format!("{}: ⚠️ Tool-call loop detected! ⚠️", call.name).as_str()
                )
            );
            indexed_results.push((idx, ToolResult::new(call, json!(dup_msg))));
        } else {
            to_execute.push((idx, call));
        }
    }

    let (mcp_calls, sequential_calls): (Vec<_>, Vec<_>) = to_execute
        .into_iter()
        .partition(|(_, call)| is_mcp_meta_function(&call.name));

    if !mcp_calls.is_empty() {
        let ctx_ref: &RequestContext = ctx;
        let futs: Vec<_> = mcp_calls
            .into_iter()
            .map(|(idx, call)| async move {
                let result = call.eval_mcp(ctx_ref).await;
                (idx, call, result)
            })
            .collect();
        for (idx, call, result) in future::join_all(futs).await {
            let value = match result {
                Ok(v) => normalize_tool_result(v),
                Err(e) => json!({"tool_call_error": format!("{e}")}),
            };
            indexed_results.push((idx, ToolResult::new(call, value)));
        }
    }

    for (idx, call) in sequential_calls {
        let value = match call.eval(ctx).await {
            Ok(v) => normalize_tool_result(v),
            Err(e) => json!({
                "tool_call_error": format!(
                    "{e}. This tool is not available or the call failed; use only tools listed in your catalog."
                )
            }),
        };
        indexed_results.push((idx, ToolResult::new(call, value)));
    }

    indexed_results.sort_unstable_by_key(|(idx, _)| *idx);
    output = indexed_results.into_iter().map(|(_, r)| r).collect();

    {
        let max_chars = ctx
            .agent
            .as_ref()
            .and_then(|a| a.max_tool_result_chars())
            .or_else(|| ctx.app.config.max_tool_result_chars);
        if let Some(max_chars) = max_chars.filter(|&n| n > 0) {
            output = output
                .into_iter()
                .map(|r| r.truncate_if_needed(max_chars))
                .collect();
        }
    }

    if ctx.current_depth == 0
        && let Some(queue) = ctx.root_escalation_queue()
        && queue.has_pending()
        && let Some(last) = output.last_mut()
    {
        inject_escalation_notification(last, queue.pending_summary());
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

fn inject_escalation_notification(last: &mut ToolResult, summary: Vec<Value>) {
    let instruction = "Child agents are BLOCKED waiting for your reply. \
        Call agent__reply_escalation for each pending escalation to unblock them.";
    match &mut last.output {
        Value::Object(map) => {
            map.insert("pending_escalations".into(), json!(summary));
            map.insert("escalation_instruction".into(), json!(instruction));
        }
        other => {
            *other = json!({
                "output": other.take(),
                "pending_escalations": summary,
                "escalation_instruction": instruction,
            });
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    pub call: ToolCall,
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking: Vec<ThinkingBlock>,
}

impl ToolResult {
    pub fn new(call: ToolCall, output: Value) -> Self {
        Self {
            call,
            output,
            text: None,
            thinking: vec![],
        }
    }

    pub fn truncate_if_needed(mut self, max_chars: usize) -> Self {
        let s = self.output.to_string();
        if s.len() > max_chars {
            let prefix = s.get(..max_chars).unwrap_or(s.as_str());
            self.output = json!(format!(
                "[truncated: tool output exceeded {max_chars} chars]\n{prefix}"
            ));
        }
        self
    }
}

fn gated_meta_function_prefixes(features: &McpServerFeatures) -> Vec<&'static str> {
    MCP_META_FUNCTION_PREFIXES
        .into_iter()
        .filter(|&prefix| match prefix {
            MCP_INVOKE_META_FUNCTION_NAME_PREFIX => features.tools,
            MCP_READ_META_FUNCTION_NAME_PREFIX => features.resources,
            MCP_PROMPT_META_FUNCTION_NAME_PREFIX => features.prompts,
            _ => true,
        })
        .collect()
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
            write_file_atomic(&file_path, content, is_script.then_some(0o755))?;
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
        write_file_atomic(&file_path, &serialized, None)
            .context("failed to write merged mcp.json")?;

        if !added.is_empty() {
            println!("  + new MCP servers: {}", added.join(", "));
        }

        Ok(())
    }

    pub fn init(visible_tools: &[String]) -> Result<Self> {
        Self::remove_stale_global_function_binaries()?;

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
        Self::remove_stale_agent_bin_entries(name)?;

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

    pub fn append_rag_query_functions(&mut self) {
        self.declarations
            .extend(rag_query::rag_query_function_declarations());
    }

    pub fn remove_rag_query_functions(&mut self) {
        self.declarations
            .retain(|f| !f.name.starts_with(RAG_FUNCTION_PREFIX));
    }

    pub fn append_mcp_meta_functions(&mut self, mcp_servers: Vec<McpServerFeatures>) {
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
        describe_function_properties.insert(
            "kind".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                description: Some(
                    "Catalog item kind: tool (default), resource, resource_template, or prompt"
                        .into(),
                ),
                default: Some(Value::from("tool")),
                ..Default::default()
            },
        );

        let mut read_function_properties = IndexMap::new();
        read_function_properties.insert(
            "uri".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                description: Some(
                    "Resource URI, or a resource template with {var} placeholders".into(),
                ),
                ..Default::default()
            },
        );
        read_function_properties.insert(
            "arguments".to_string(),
            JsonSchema {
                type_value: Some("object".to_string()),
                description: Some("Template variable values (RFC 6570 Level 1 only)".into()),
                ..Default::default()
            },
        );
        read_function_properties.insert(
            "pattern".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                description: Some(
                    "Optional regex; returns only matching lines (with context) from text content"
                        .into(),
                ),
                ..Default::default()
            },
        );
        read_function_properties.insert(
            "offset".to_string(),
            JsonSchema {
                type_value: Some("integer".to_string()),
                description: Some(
                    "Byte offset for paging text. When pattern is set, offsets (and \
                     next_offset/total_bytes in the result) refer to the filtered stream, not \
                     the raw resource"
                        .into(),
                ),
                default: Some(Value::from(0usize)),
                ..Default::default()
            },
        );
        read_function_properties.insert(
            "max_bytes".to_string(),
            JsonSchema {
                type_value: Some("integer".to_string()),
                description: Some(format!(
                    "Max text bytes to return (clamped to {})",
                    render::TEXT_MAX_BYTES_CLAMP
                )),
                default: Some(Value::from(render::DEFAULT_TEXT_MAX_BYTES)),
                ..Default::default()
            },
        );

        let mut prompt_function_properties = IndexMap::new();
        prompt_function_properties.insert(
            "prompt".to_string(),
            JsonSchema {
                type_value: Some("string".to_string()),
                ..Default::default()
            },
        );
        prompt_function_properties.insert(
            "arguments".to_string(),
            JsonSchema {
                type_value: Some("object".to_string()),
                description: Some("String values only; prompt arguments have no schemas".into()),
                ..Default::default()
            },
        );

        for features in mcp_servers {
            let server = &features.name;
            let search_function_name = format!("{}_{server}", MCP_SEARCH_META_FUNCTION_NAME_PREFIX);
            let describe_function_name =
                format!("{}_{server}", MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX);
            let invoke_function_name = format!("{}_{server}", MCP_INVOKE_META_FUNCTION_NAME_PREFIX);
            let read_function_name = format!("{}_{server}", MCP_READ_META_FUNCTION_NAME_PREFIX);
            let prompt_function_name = format!("{}_{server}", MCP_PROMPT_META_FUNCTION_NAME_PREFIX);
            for prefix in gated_meta_function_prefixes(&features) {
                match prefix {
                    MCP_INVOKE_META_FUNCTION_NAME_PREFIX => {
                        self.declarations.push(FunctionDeclaration {
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
                        });
                    }
                    MCP_SEARCH_META_FUNCTION_NAME_PREFIX => {
                        self.declarations.push(FunctionDeclaration {
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
                        });
                    }
                    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX => {
                        self.declarations.push(FunctionDeclaration {
                            name: describe_function_name.clone(),
                            description: "Get the full schema or metadata for exactly one MCP \
                                          catalog item: a tool, resource, resource template, or \
                                          prompt."
                                .to_string(),
                            parameters: JsonSchema {
                                type_value: Some("object".to_string()),
                                properties: Some(describe_function_properties.clone()),
                                required: Some(vec!["tool".to_string()]),
                                ..Default::default()
                            },
                            agent: false,
                        });
                    }
                    MCP_READ_META_FUNCTION_NAME_PREFIX => {
                        self.declarations.push(FunctionDeclaration {
                            name: read_function_name.clone(),
                            description: formatdoc!(
                                r#"
                                Read a resource, or expand a resource template, from the {server} MCP server. Call
                                {describe_function_name} with kind "resource" or "resource_template" to find URIs and
                                template variables. Text content is paged via offset/max_bytes and can be filtered
                                with pattern; binary content is spilled to disk and its metadata returned.
                                "#
                            ),
                            parameters: JsonSchema {
                                type_value: Some("object".to_string()),
                                properties: Some(read_function_properties.clone()),
                                required: Some(vec!["uri".to_string()]),
                                ..Default::default()
                            },
                            agent: false,
                        });
                    }
                    MCP_PROMPT_META_FUNCTION_NAME_PREFIX => {
                        self.declarations.push(FunctionDeclaration {
                            name: prompt_function_name.clone(),
                            description: formatdoc!(
                                r#"
                                Fetch a prompt from the {server} MCP server, rendered with the given arguments. Call
                                {describe_function_name} with kind "prompt" to discover prompt names and their
                                arguments. The result is the prompt text, labeled per message; fold it into your
                                reasoning.
                                "#
                            ),
                            parameters: JsonSchema {
                                type_value: Some("object".to_string()),
                                properties: Some(prompt_function_properties.clone()),
                                required: Some(vec!["prompt".to_string()]),
                                ..Default::default()
                            },
                            agent: false,
                        });
                    }
                    _ => debug_assert!(false, "unhandled MCP meta-function prefix: {prefix}"),
                }
            }
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

    fn remove_stale_agent_bin_entries(name: &str) -> Result<()> {
        let agent_bin_directory = paths::agent_bin_dir(name);

        debug!(
            "Pruning stale entries in agent bin directory: {}",
            agent_bin_directory.display()
        );

        prune_stale_bin_entries(&agent_bin_directory, &tool_source_stems()?, Some(name))
    }

    fn remove_stale_global_function_binaries() -> Result<()> {
        let bin_dir = paths::functions_bin_dir();

        info!("Pruning stale function binaries in {}", bin_dir.display());

        prune_stale_bin_entries(&bin_dir, &tool_source_stems()?, None)
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
        write_file_atomic(&binary_script_file, &content, None)?;

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

        write_file_atomic(&binary_file, &content, None)?;

        Ok(())
    }

    #[cfg(not(windows))]
    fn build_binaries(
        binary_name: &str,
        language: Language,
        binary_type: BinaryType,
        custom_runtime: Option<&str>,
    ) -> Result<()> {
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
            write_file_atomic(&script_file, &content, Some(0o755))?;

            let ts_runtime = custom_runtime.unwrap_or("tsx");
            let wrapper = format!(
                "#!/bin/sh\nexec {ts_runtime} \"{}\" \"$@\"\n",
                script_file.display()
            );
            write_file_atomic(&binary_file, &wrapper, Some(0o755))?;
        } else {
            write_file_atomic(&binary_file, &content, Some(0o755))?;
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

    fn parse_arguments(&self) -> Result<Value> {
        if self.arguments.is_object() {
            Ok(self.arguments.clone())
        } else if let Some(arguments) = self.arguments.as_str() {
            serde_json::from_str(arguments).map_err(|_| {
                anyhow!(
                    "The call '{}' has invalid arguments: {arguments}",
                    self.name
                )
            })
        } else {
            bail!(
                "The call '{}' has invalid arguments: {}",
                self.name,
                self.arguments
            )
        }
    }

    async fn eval_mcp(&self, ctx: &RequestContext) -> Result<Value> {
        let json_data = self.parse_arguments()?;
        let cmd_name = self.name.as_str();
        if *IS_STDOUT_TERMINAL && ctx.current_depth == 0 && !HEADLESS.load(Ordering::SeqCst) {
            println!(
                "{}",
                format_call_log(cmd_name, &[json_data.to_string()], &json_data)
            );
        }
        let result = if cmd_name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX) {
            Self::search_mcp_tools(ctx, cmd_name, &json_data)
                .await
                .unwrap_or_else(|e| {
                    let error_msg = format!("MCP search failed: {e}");
                    eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                    json!({"tool_call_error": error_msg})
                })
        } else if cmd_name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX) {
            Self::describe_mcp_tool(ctx, cmd_name, json_data.clone())
                .await
                .unwrap_or_else(|e| {
                    let error_msg = format!("MCP describe failed: {e}");
                    eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                    json!({"tool_call_error": error_msg})
                })
        } else if cmd_name.starts_with(MCP_READ_META_FUNCTION_NAME_PREFIX) {
            Self::read_mcp_resource(ctx, cmd_name, &json_data)
                .await
                .unwrap_or_else(|e| {
                    let error_msg = format!("MCP read failed: {e}");
                    eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                    json!({"tool_call_error": error_msg})
                })
        } else if cmd_name.starts_with(MCP_PROMPT_META_FUNCTION_NAME_PREFIX) {
            Self::get_mcp_prompt(ctx, cmd_name, &json_data)
                .await
                .unwrap_or_else(|e| {
                    let error_msg = format!("MCP prompt failed: {e}");
                    eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                    json!({"tool_call_error": error_msg})
                })
        } else {
            Self::invoke_mcp_tool(ctx, cmd_name, &json_data)
                .await
                .unwrap_or_else(|e| {
                    let error_msg = format!("MCP tool invocation failed: {e}");
                    eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                    json!({"tool_call_error": error_msg})
                })
        };
        Ok(result)
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

        if *IS_STDOUT_TERMINAL && current_depth == 0 && !HEADLESS.load(Ordering::SeqCst) {
            println!("{}", format_call_log(&cmd_name, &cmd_args, &json_data));
        }

        let output = match cmd_name.as_str() {
            _ if cmd_name.starts_with(MCP_SEARCH_META_FUNCTION_NAME_PREFIX) => {
                Self::search_mcp_tools(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP search failed: {e}");
                        eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX) => {
                Self::describe_mcp_tool(ctx, &cmd_name, json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP describe failed: {e}");
                        eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(MCP_READ_META_FUNCTION_NAME_PREFIX) => {
                Self::read_mcp_resource(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP read failed: {e}");
                        eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(MCP_PROMPT_META_FUNCTION_NAME_PREFIX) => {
                Self::get_mcp_prompt(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP prompt failed: {e}");
                        eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX) => {
                Self::invoke_mcp_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("MCP tool invocation failed: {e}");
                        eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(TODO_FUNCTION_PREFIX) => {
                todo::handle_todo_tool(ctx, &cmd_name, &json_data).unwrap_or_else(|e| {
                    let error_msg = format!("Todo tool failed: {e}");
                    eprintln!("{}", muted_warning_text(&format!("⚠️ {error_msg} ⚠️")));
                    json!({"tool_call_error": error_msg})
                })
            }
            _ if cmd_name.starts_with(MEMORY_FUNCTION_PREFIX) => {
                memory::handle_memory_tool(ctx, &cmd_name, &json_data).unwrap_or_else(|e| {
                    let error_msg = format!("Memory tool failed: {e}");
                    eprintln!("{}", muted_warning_text(&format!("⚠️ {error_msg} ⚠️")));
                    json!({"tool_call_error": error_msg})
                })
            }
            _ if cmd_name.starts_with(SKILL_FUNCTION_PREFIX) => {
                skill::handle_skill_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("Skill tool failed: {e}");
                        eprintln!("{}", muted_warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(SUPERVISOR_FUNCTION_PREFIX) => {
                supervisor::handle_supervisor_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("Supervisor tool failed: {e}");
                        eprintln!("{}", muted_warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(USER_FUNCTION_PREFIX) => {
                user_interaction::handle_user_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("User interaction failed: {e}");
                        eprintln!("{}", muted_warning_text(&format!("⚠️ {error_msg} ⚠️")));
                        json!({"tool_call_error": error_msg})
                    })
            }
            _ if cmd_name.starts_with(RAG_FUNCTION_PREFIX) => {
                rag_query::handle_rag_tool(ctx, &cmd_name, &json_data)
                    .await
                    .unwrap_or_else(|e| {
                        let error_msg = format!("RAG query failed: {e}");
                        eprintln!("{}", muted_warning_text(&format!("⚠️ {error_msg} ⚠️")));
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
        let kind = match json_data.get("kind") {
            Some(value) => value
                .as_str()
                .ok_or_else(|| anyhow!("Invalid 'kind' in arguments"))?,
            None => "tool",
        };
        let result = ctx
            .tool_scope
            .mcp_runtime
            .describe(&server_id, kind, tool)
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
        render_tool_result(serde_json::to_value(result)?, &server)
    }

    async fn read_mcp_resource(
        ctx: &RequestContext,
        cmd_name: &str,
        json_data: &Value,
    ) -> Result<Value> {
        let server = cmd_name
            .strip_prefix(&format!("{MCP_READ_META_FUNCTION_NAME_PREFIX}_"))
            .ok_or_else(|| anyhow!("Malformed MCP read function name: {cmd_name}"))?;
        let uri = json_data
            .get("uri")
            .ok_or_else(|| anyhow!("Missing 'uri' in arguments"))?
            .as_str()
            .ok_or_else(|| anyhow!("Invalid 'uri' in arguments"))?;
        let pattern = match json_data.get("pattern") {
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| anyhow!("Invalid 'pattern' in arguments"))?,
            ),
            None => None,
        };
        let offset = match json_data.get("offset") {
            Some(value) => value
                .as_u64()
                .ok_or_else(|| anyhow!("Invalid 'offset' in arguments"))?
                as usize,
            None => 0,
        };
        let max_bytes = match json_data.get("max_bytes") {
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or_else(|| anyhow!("Invalid 'max_bytes' in arguments"))?
                    as usize,
            ),
            None => None,
        };
        let uri = match json_data.get("arguments").and_then(Value::as_object) {
            Some(args) if !args.is_empty() => expand_uri_template(uri, args)?,
            _ => uri.to_string(),
        };

        let result = ctx.tool_scope.mcp_runtime.read(server, &uri).await?;
        let audience = ctx
            .tool_scope
            .mcp_runtime
            .resource_audience(server, &uri)
            .await;
        let items: Vec<Value> = result
            .contents
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()?;

        let mut rendered_items = Vec::with_capacity(items.len());
        let mut total_size = 0usize;
        for (index, item) in items.iter().enumerate() {
            let mut rendered = render_resource_content(item, pattern, offset, max_bytes, server)?;
            if let (Some(audience), Some(map)) = (&audience, rendered.as_object_mut()) {
                map.insert("audience".to_string(), json!(audience));
            }
            let size = rendered.to_string().len();
            // Bound the overall response; the first item is always included.
            if index > 0 && total_size + size > render::TEXT_MAX_BYTES_CLAMP {
                let omitted = items.len() - index;
                rendered_items.push(json!({
                    "truncated": true,
                    "omitted_items": omitted,
                    "note": format!(
                        "{omitted} content item(s) omitted: the combined response would exceed \
                         {} bytes",
                        render::TEXT_MAX_BYTES_CLAMP
                    ),
                }));
                break;
            }
            total_size += size;
            rendered_items.push(rendered);
        }

        if rendered_items.len() == 1 {
            Ok(rendered_items.remove(0))
        } else {
            Ok(Value::Array(rendered_items))
        }
    }

    async fn get_mcp_prompt(
        ctx: &RequestContext,
        cmd_name: &str,
        json_data: &Value,
    ) -> Result<Value> {
        let server = cmd_name
            .strip_prefix(&format!("{MCP_PROMPT_META_FUNCTION_NAME_PREFIX}_"))
            .ok_or_else(|| anyhow!("Malformed MCP prompt function name: {cmd_name}"))?;
        let prompt = json_data
            .get("prompt")
            .ok_or_else(|| anyhow!("Missing 'prompt' in arguments"))?
            .as_str()
            .ok_or_else(|| anyhow!("Invalid 'prompt' in arguments"))?;
        let mut provided = HashMap::new();
        if let Some(value) = json_data.get("arguments") {
            let entries = value
                .as_object()
                .ok_or_else(|| anyhow!("Invalid 'arguments' in arguments"))?;
            for (key, value) in entries {
                let value = value.as_str().ok_or_else(|| {
                    anyhow!(
                        "Invalid value for prompt argument '{key}': prompt arguments are strings"
                    )
                })?;
                provided.insert(key.clone(), value.to_string());
            }
        }

        let declared = ctx
            .tool_scope
            .mcp_runtime
            .list_prompts(server)
            .await?
            .into_iter()
            .find(|candidate| candidate.name == prompt)
            .ok_or_else(|| {
                anyhow!(
                    "Prompt '{prompt}' not found on MCP server '{server}'; call the describe \
                     meta-tool with kind \"prompt\" to list available prompts"
                )
            })?
            .arguments
            .unwrap_or_default();
        let (arguments, missing) = resolve_prompt_args(&declared, provided);
        if !missing.is_empty() {
            bail!(
                "Missing required prompt argument(s): {}. Provide them as string values in \
                 'arguments'.",
                missing.join(", ")
            );
        }

        let result = ctx
            .tool_scope
            .mcp_runtime
            .prompt(server, prompt, arguments)
            .await?;
        Ok(Value::String(flatten_prompt_messages(&result.messages)))
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

fn expand_uri_template(template: &str, args: &serde_json::Map<String, Value>) -> Result<String> {
    let mut expanded = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(len) = after.find('}') else {
            bail!("Unclosed '{{' in URI template: {template}");
        };
        expanded.push_str(&expand_uri_template_variable(&after[..len], args)?);
        rest = &after[len + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

fn expand_uri_template_variable(
    expr: &str,
    args: &serde_json::Map<String, Value>,
) -> Result<String> {
    const LEVEL_1_ONLY: &str = "only RFC 6570 Level 1 simple substitution {var} is supported";
    if let Some(operator) = expr.chars().next().filter(|c| "+#./;?&".contains(*c)) {
        let name = match operator {
            '+' => "reserved-expansion",
            '#' => "fragment-expansion",
            '.' => "label-expansion",
            '/' => "path-segment-expansion",
            ';' => "path-style-parameter-expansion",
            '?' => "form-style-query-expansion",
            _ => "form-style-query-continuation",
        };
        bail!(
            "The '{operator}' {name} operator in '{{{expr}}}' requires RFC 6570 Level 2 or \
             higher; {LEVEL_1_ONLY}"
        );
    }
    if expr.contains(',') {
        bail!(
            "The ',' multi-variable expression '{{{expr}}}' requires RFC 6570 Level 3; {LEVEL_1_ONLY}"
        );
    }
    if expr.contains(':') {
        bail!("The ':' prefix modifier in '{{{expr}}}' requires RFC 6570 Level 4; {LEVEL_1_ONLY}");
    }
    if expr.ends_with('*') {
        bail!("The '*' explode modifier in '{{{expr}}}' requires RFC 6570 Level 4; {LEVEL_1_ONLY}");
    }
    if expr.is_empty()
        || !expr
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        bail!("Invalid variable name '{{{expr}}}' in URI template; expected [A-Za-z0-9_.]+");
    }
    let value = args
        .get(expr)
        .ok_or_else(|| anyhow!("URI template variable '{expr}' is missing from 'arguments'"))?;
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        other => bail!(
            "URI template variable '{expr}' must be a string, number, or boolean; got {other}"
        ),
    };
    Ok(urlencoding::encode(&text).into_owned())
}

#[derive(Debug)]
enum ResourceContentBody {
    Text(String),
    Blob(String),
}

// rmcp's untagged ResourceContents enum cannot represent malformed items
// (both or neither of text/blob), so classification happens on the raw Value.
fn parse_resource_content(item: &Value) -> Result<ResourceContentBody> {
    let text = item.get("text");
    let blob = item.get("blob");
    match (text, blob) {
        (Some(text), None) => Ok(ResourceContentBody::Text(
            text.as_str()
                .ok_or_else(|| anyhow!("Resource content 'text' is not a string"))?
                .to_string(),
        )),
        (None, Some(blob)) => Ok(ResourceContentBody::Blob(
            blob.as_str()
                .ok_or_else(|| anyhow!("Resource content 'blob' is not a string"))?
                .to_string(),
        )),
        (Some(_), Some(_)) => {
            bail!("Resource content item has both 'text' and 'blob'; expected exactly one")
        }
        (None, None) => {
            bail!("Resource content item has neither 'text' nor 'blob'; expected exactly one")
        }
    }
}

fn render_resource_content(
    item: &Value,
    pattern: Option<&str>,
    offset: usize,
    max_bytes: Option<usize>,
    server: &str,
) -> Result<Value> {
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .map(render::clamp_metadata);
    let mime_type = item
        .get("mimeType")
        .and_then(Value::as_str)
        .map(render::clamp_metadata);
    let text = match parse_resource_content(item)? {
        ResourceContentBody::Text(text) => text,
        ResourceContentBody::Blob(blob) => {
            match render::render_blob(&blob, mime_type.as_deref(), server)? {
                render::RenderedBlob::Text(text) => text,
                render::RenderedBlob::Spilled(meta) => {
                    let mut value = serde_json::to_value(meta)?;
                    if let Some(map) = value.as_object_mut() {
                        map.insert("uri".to_string(), json!(uri));
                    }
                    return Ok(value);
                }
            }
        }
    };
    let rendered = render::render_text(&text, pattern, offset, max_bytes)?;
    let mut value = json!({
        "uri": uri,
        "mime_type": mime_type,
        "text": rendered.text,
        "truncated": rendered.truncated,
        "total_bytes": rendered.total_bytes,
        "next_offset": rendered.next_offset,
    });

    if let Some(next_offset) = rendered.next_offset {
        value["note"] = json!(format!(
            "Content truncated; re-call with offset={next_offset} to continue (max_bytes is \
             clamped to {})",
            render::TEXT_MAX_BYTES_CLAMP
        ));
    }

    Ok(value)
}

// Terminal-only rendering of an MCP dispatch error: escape sequences are
// stripped so a hostile server cannot drive the terminal, while the JSON
// payload keeps the raw message.
fn mcp_error_display(error_msg: &str) -> String {
    sanitize_display_text(&format!("⚠️ {error_msg} ⚠️"))
}

/// Bounds a raw `CallToolResult` JSON value: oversized text is sliced,
/// base64 blob content is routed through the blob renderer instead of
/// reaching model context, and oversized structured content is replaced with
/// a truncation marker. In-bounds results pass through unchanged.
fn render_tool_result(mut result: Value, server: &str) -> Result<Value> {
    let Some(map) = result.as_object_mut() else {
        return Ok(result);
    };
    if let Some(items) = map.get_mut("content").and_then(Value::as_array_mut) {
        for item in items {
            render_tool_content_item(item, server)?;
        }
    }
    let oversized_structured = map
        .get("structuredContent")
        .is_some_and(|structured| structured.to_string().len() > render::TEXT_MAX_BYTES_CLAMP);
    if oversized_structured {
        map.insert(
            "structuredContent".to_string(),
            json!({
                "truncated": true,
                "note": format!(
                    "structuredContent omitted: its serialized form exceeds \
                     TEXT_MAX_BYTES_CLAMP ({} bytes); re-call the tool with narrower \
                     arguments",
                    render::TEXT_MAX_BYTES_CLAMP
                ),
            }),
        );
    }

    Ok(result)
}

fn render_tool_content_item(item: &mut Value, server: &str) -> Result<()> {
    match item.get("type").and_then(Value::as_str) {
        Some("text") => clamp_tool_text(item),
        Some("image") | Some("audio") => {
            let mime_type = item
                .get("mimeType")
                .and_then(Value::as_str)
                .map(render::clamp_metadata);
            if let Some(data) = item.get("data").and_then(Value::as_str) {
                let replacement = render_tool_blob(data, mime_type, None, server)?;
                *item = replacement;
            }
        }
        Some("resource") => {
            let Some(resource) = item.get("resource") else {
                return Ok(());
            };
            let uri = resource
                .get("uri")
                .and_then(Value::as_str)
                .map(render::clamp_metadata);
            let mime_type = resource
                .get("mimeType")
                .and_then(Value::as_str)
                .map(render::clamp_metadata);
            if let Some(blob) = resource.get("blob").and_then(Value::as_str) {
                let replacement = render_tool_blob(blob, mime_type, uri, server)?;
                *item = replacement;
            } else if let Some(resource) = item.get_mut("resource") {
                clamp_tool_text(resource);
                clamp_metadata_field(resource, "uri");
                clamp_metadata_field(resource, "mimeType");
            }
        }
        _ => {}
    }

    Ok(())
}

fn render_tool_blob(
    b64: &str,
    mime_type: Option<String>,
    uri: Option<String>,
    server: &str,
) -> Result<Value> {
    let mut value = match render::render_blob(b64, mime_type.as_deref(), server) {
        Ok(render::RenderedBlob::Text(text)) => {
            let mut item = json!({ "type": "text", "text": text });
            clamp_tool_text(&mut item);
            item
        }
        Ok(render::RenderedBlob::Spilled(meta)) => serde_json::to_value(meta)?,
        // One undecodable item must not sink the rest of the result.
        Err(error) => json!({ "error": format!("Failed to render blob content: {error}") }),
    };

    if let Some(map) = value.as_object_mut() {
        if let Some(mime_type) = mime_type
            && !map.contains_key("mime_type")
        {
            map.insert("mime_type".to_string(), json!(mime_type));
        }
        if let Some(uri) = uri {
            map.insert("uri".to_string(), json!(uri));
        }
    }

    Ok(value)
}

fn clamp_tool_text(container: &mut Value) {
    let Some(text) = container.get("text").and_then(Value::as_str) else {
        return;
    };

    if text.len() <= render::TEXT_MAX_BYTES_CLAMP {
        return;
    }

    let total_bytes = text.len();
    let clamped = render::truncate_utf8(text, render::TEXT_MAX_BYTES_CLAMP).to_string();
    let Some(map) = container.as_object_mut() else {
        return;
    };
    map.insert("text".to_string(), json!(clamped));
    map.insert("truncated".to_string(), json!(true));
    map.insert("total_bytes".to_string(), json!(total_bytes));
    map.insert(
        "note".to_string(),
        json!(format!(
            "Text truncated; re-call the tool with narrower arguments (text is clamped to \
             TEXT_MAX_BYTES_CLAMP = {} bytes)",
            render::TEXT_MAX_BYTES_CLAMP
        )),
    );
}

fn clamp_metadata_field(object: &mut Value, key: &str) {
    let Some(text) = object.get(key).and_then(Value::as_str) else {
        return;
    };

    if text.len() <= render::METADATA_MAX_BYTES {
        return;
    }

    let clamped = render::clamp_metadata(text);
    object[key] = json!(clamped);
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow!("Unable to run {command_name}, {err}"))?;

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    let stdout_thread = thread::spawn(move || {
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

    let stderr_thread = thread::spawn(move || {
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

    let timeout_secs = env::var("COYOTE_TOOL_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1800);
    let deadline = (timeout_secs > 0).then(|| Instant::now() + Duration::from_secs(timeout_secs));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => bail!("Unable to run {command_name}, {err}"),
        }
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            let _ = child.kill();
            let _ = child.wait();
            drop(stdout_thread);
            drop(stderr_thread);
            let tool_error_message = format!(
                "Tool call '{command_name}' timed out after {timeout_secs}s and was killed (set COYOTE_TOOL_TIMEOUT to adjust; 0 = unlimited)"
            );
            eprintln!(
                "{}",
                muted_warning_text(&format!("⚠️ {tool_error_message} ⚠️"))
            );
            let error_json = json!({"tool_call_error": tool_error_message});

            debug!("Tool call error: {error_json:?}");

            return Ok(Some(error_json.to_string()));
        }
        thread::sleep(Duration::from_millis(100));
    };
    let stdout_bytes = stdout_thread.join().unwrap_or_default();
    let stderr_bytes = stderr_thread.join().unwrap_or_default();

    let exit_code = status.code().unwrap_or_default();
    if exit_code != 0 {
        let stderr = String::from_utf8_lossy(&stderr_bytes).trim().to_string();
        let stdout = String::from_utf8_lossy(&stdout_bytes).trim().to_string();
        let tool_error_message = format!("Tool call '{command_name}' exited with code {exit_code}");
        eprintln!(
            "{}",
            muted_warning_text(&format!("⚠️ {tool_error_message} ⚠️"))
        );
        let mut error_json = json!({"tool_call_error": tool_error_message});
        if !stderr.is_empty() {
            error_json["stderr"] = json!(stderr);
        }
        if !stdout.is_empty() {
            error_json["stdout"] = json!(stdout);
        }
        if let Ok(contents) = fs::read_to_string(&tmp_file)
            && !contents.trim().is_empty()
        {
            error_json["output"] = json!(contents);
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

fn format_call_log(cmd_name: &str, cmd_args: &[String], json_data: &serde_json::Value) -> String {
    if *NO_COLOR {
        return format!("Call {cmd_name} {}", cmd_args.join(" "));
    }
    let prefix_args = &cmd_args[..cmd_args.len().saturating_sub(1)];
    let prefix = if prefix_args.is_empty() {
        String::new()
    } else {
        format!("{} ", dimmed_text(&prefix_args.join(" ")))
    };
    format!(
        "{}{} {}{}",
        dimmed_text("Call "),
        cyan_bold_text(cmd_name),
        prefix,
        format_json_colored_keys(json_data),
    )
}

fn format_json_colored_keys(value: &serde_json::Value) -> String {
    let serde_json::Value::Object(map) = value else {
        return dimmed_text(&value.to_string());
    };
    if map.is_empty() {
        return dimmed_text("{}");
    }
    let pairs: Vec<String> = map
        .iter()
        .map(|(k, v)| {
            let key = magenta_text(&format!("\"{k}\""));
            format!("{}{}", key, dimmed_text(&format!(": {v}")))
        })
        .collect();
    format!(
        "{}{}{}",
        dimmed_text("{"),
        pairs.join(&dimmed_text(", ")),
        dimmed_text("}")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::{
        FIXTURE_ANNOTATED_TEXT, FIXTURE_ANNOTATED_URI, FIXTURE_BLOB_BYTES, FIXTURE_BLOB_URI,
        FIXTURE_LOG_TEXT, FIXTURE_LOG_URI, FixtureServer, fixture_runtime,
    };
    use crate::config::{Agent, AgentConfig, AppConfig, AppState, WorkingMode};
    use crate::supervisor::escalation::{EscalationQueue, EscalationRequest};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rmcp::model::{CallToolResult, ContentBlock};
    use serde_json::json;
    use serial_test::serial;
    use std::process;
    use std::sync::Arc;

    fn call(name: &str, id: Option<&str>) -> ToolCall {
        ToolCall::new(name.to_string(), json!({}), id.map(|s| s.to_string()))
    }

    fn call_with_args(name: &str, args: Value) -> ToolCall {
        ToolCall::new(name.to_string(), args, Some("id1".to_string()))
    }

    fn mcp_features(name: &str, tools: bool, resources: bool, prompts: bool) -> McpServerFeatures {
        McpServerFeatures {
            name: name.to_string(),
            tools,
            resources,
            prompts,
        }
    }

    fn tools_only(name: &str) -> McpServerFeatures {
        mcp_features(name, true, false, false)
    }

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn submit_escalation(queue: &EscalationQueue, id: &str) {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        queue.submit(EscalationRequest {
            id: id.to_string(),
            from_agent_id: "a1".into(),
            from_agent_name: "explore".into(),
            question: "What do?".into(),
            options: None,
            reply_tx: tx,
        });
    }

    #[test]
    fn normalize_tool_result_substitutes_done_for_null() {
        assert_eq!(normalize_tool_result(Value::Null), json!("DONE"));
    }

    #[test]
    fn inject_escalation_notification_extends_object_output() {
        let mut result = ToolResult::new(call("t", Some("id-1")), json!({"status": "ok"}));
        inject_escalation_notification(&mut result, vec![json!({"escalation_id": "esc_1"})]);
        assert_eq!(result.output["status"], "ok");
        assert_eq!(
            result.output["pending_escalations"],
            json!([{"escalation_id": "esc_1"}])
        );
        assert!(
            result.output["escalation_instruction"]
                .as_str()
                .unwrap()
                .contains("agent__reply_escalation")
        );
        assert!(result.text.is_none());
    }

    #[test]
    fn inject_escalation_notification_wraps_non_object_output() {
        let mut result = ToolResult::new(call("t", Some("id-1")), json!("DONE"));
        inject_escalation_notification(&mut result, vec![json!({"escalation_id": "esc_2"})]);
        assert_eq!(result.output["output"], json!("DONE"));
        assert_eq!(
            result.output["pending_escalations"],
            json!([{"escalation_id": "esc_2"}])
        );
        assert!(result.output["escalation_instruction"].is_string());
    }

    #[test]
    fn eval_tool_calls_soft_fails_unknown_tool() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let calls = vec![call("__escalation_notification", Some("id-1"))];
        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();
        assert_eq!(results.len(), 1);
        let err = results[0].output["tool_call_error"].as_str().unwrap();
        assert!(err.contains("Unexpected call"));
        assert!(err.contains("use only tools listed in your catalog"));
    }

    #[test]
    fn eval_tool_calls_injects_escalations_into_last_result() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let queue = ctx.ensure_root_escalation_queue();
        submit_escalation(&queue, "esc_1");

        let calls = vec![call("unknown_tool", Some("id-1"))];
        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        assert_eq!(results.len(), 1);
        assert!(
            results
                .iter()
                .all(|r| r.call.name != "__escalation_notification")
        );
        let out = &results[0].output;
        assert!(out["tool_call_error"].is_string());
        assert_eq!(out["pending_escalations"][0]["escalation_id"], "esc_1");
        assert!(
            out["escalation_instruction"]
                .as_str()
                .unwrap()
                .contains("agent__reply_escalation")
        );
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
        assert_eq!(MCP_READ_META_FUNCTION_NAME_PREFIX, "mcp_read");
        assert_eq!(MCP_PROMPT_META_FUNCTION_NAME_PREFIX, "mcp_prompt");
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
        assert!(f.contains("agent__list_running"));
        assert!(f.contains("agent__list_available"));
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
        assert!(f.contains("user__select"));
        assert!(f.contains("user__confirm"));
        assert!(f.contains("user__input"));
        assert!(f.contains("user__checkbox"));
    }

    #[test]
    fn functions_append_mcp_meta_creates_three_per_server() {
        let mut f = Functions::default();

        f.append_mcp_meta_functions(vec![tools_only("github")]);

        assert_eq!(f.declarations().len(), 3);
        assert!(f.contains("mcp_invoke_github"));
        assert!(f.contains("mcp_search_github"));
        assert!(f.contains("mcp_describe_github"));
    }

    #[test]
    fn functions_append_mcp_meta_multiple_servers() {
        let mut f = Functions::default();

        f.append_mcp_meta_functions(vec![tools_only("github"), tools_only("slack")]);

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
    fn functions_append_mcp_meta_resources_only_omits_invoke() {
        let mut f = Functions::default();

        f.append_mcp_meta_functions(vec![mcp_features("res", false, true, false)]);

        assert_eq!(f.declarations().len(), 3);
        assert!(!f.contains("mcp_invoke_res"));
        assert!(!f.contains("mcp_prompt_res"));
        assert!(f.contains("mcp_search_res"));
        assert!(f.contains("mcp_describe_res"));
        assert!(f.contains("mcp_read_res"));
    }

    #[test]
    fn functions_append_mcp_meta_all_capabilities_emits_five() {
        let mut f = Functions::default();

        f.append_mcp_meta_functions(vec![mcp_features("srv", true, true, true)]);

        assert_eq!(f.declarations().len(), 5);
        assert!(f.contains("mcp_invoke_srv"));
        assert!(f.contains("mcp_search_srv"));
        assert!(f.contains("mcp_describe_srv"));
        assert!(f.contains("mcp_read_srv"));
        assert!(f.contains("mcp_prompt_srv"));
    }

    #[test]
    fn features_from_missing_capabilities_fail_open_for_tools() {
        let features = McpServerFeatures::from_capabilities("srv", None);
        assert!(features.tools);
        assert!(!features.resources);
        assert!(!features.prompts);

        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec![features]);
        assert!(f.contains("mcp_invoke_srv"));
    }

    #[test]
    fn gated_prefixes_tools_only() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", true, false, false)),
            vec![
                MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
            ]
        );
    }

    #[test]
    fn gated_prefixes_tools_and_resources_include_read() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", true, true, false)),
            vec![
                MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
                MCP_READ_META_FUNCTION_NAME_PREFIX,
            ]
        );
    }

    #[test]
    fn gated_prefixes_tools_and_prompts_include_prompt() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", true, false, true)),
            vec![
                MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
                MCP_PROMPT_META_FUNCTION_NAME_PREFIX,
            ]
        );
    }

    #[test]
    fn gated_prefixes_all_capabilities_include_all() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", true, true, true)),
            MCP_META_FUNCTION_PREFIXES.to_vec()
        );
    }

    #[test]
    fn gated_prefixes_resources_only_omit_invoke() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", false, true, false)),
            vec![
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
                MCP_READ_META_FUNCTION_NAME_PREFIX,
            ]
        );
    }

    #[test]
    fn gated_prefixes_prompts_only_omit_invoke_and_read() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", false, false, true)),
            vec![
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
                MCP_PROMPT_META_FUNCTION_NAME_PREFIX,
            ]
        );
    }

    #[test]
    fn gated_prefixes_resources_and_prompts_omit_invoke() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", false, true, true)),
            vec![
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
                MCP_READ_META_FUNCTION_NAME_PREFIX,
                MCP_PROMPT_META_FUNCTION_NAME_PREFIX,
            ]
        );
    }

    #[test]
    fn gated_prefixes_no_capabilities_keep_search_and_describe() {
        assert_eq!(
            gated_meta_function_prefixes(&mcp_features("srv", false, false, false)),
            vec![
                MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
                MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
            ]
        );
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
        f.append_mcp_meta_functions(vec![tools_only("srv")]);
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
        f.append_mcp_meta_functions(vec![tools_only("srv")]);
        let decl = f.find("mcp_search_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("top_k"));
    }

    #[test]
    fn functions_mcp_describe_declaration_has_tool_param() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec![tools_only("srv")]);
        let decl = f.find("mcp_describe_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        assert!(props.contains_key("tool"));
    }

    #[test]
    fn functions_mcp_describe_declaration_has_optional_kind_param() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec![tools_only("srv")]);
        let decl = f.find("mcp_describe_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        let kind = props.get("kind").unwrap();
        assert_eq!(kind.default, Some(Value::from("tool")));
        let required = decl.parameters.required.as_ref().unwrap();
        assert_eq!(required, &vec!["tool".to_string()]);
    }

    #[test]
    fn eval_mcp_describe_without_kind_defaults_to_tool() {
        let output = run_async(async {
            let (runtime, _server) = fixture_runtime(FixtureServer::default()).await;
            let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
            ctx.tool_scope.mcp_runtime = runtime;
            let call = call_with_args("mcp_describe_fixture", json!({"tool": "dup"}));
            call.eval_mcp(&ctx).await
        })
        .unwrap();

        assert_eq!(
            output,
            json!({
                "type": "object",
                "properties": {
                    "tool": { "type": "string" },
                    "arguments": {
                        "type": "object",
                        "properties": { "q": { "type": "string" } }
                    }
                }
            })
        );
    }

    fn template_args(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect()
    }

    fn resources_fixture() -> FixtureServer {
        FixtureServer {
            resources_capability: true,
            ..Default::default()
        }
    }

    async fn eval_mcp_read(args: Value) -> Result<Value> {
        let (runtime, _server) = fixture_runtime(resources_fixture()).await;
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.mcp_runtime = runtime;
        call_with_args("mcp_read_fixture", args)
            .eval_mcp(&ctx)
            .await
    }

    fn prompts_fixture() -> FixtureServer {
        FixtureServer {
            prompts_capability: true,
            ..Default::default()
        }
    }

    async fn eval_mcp_prompt(args: Value) -> Result<Value> {
        let (runtime, _server) = fixture_runtime(prompts_fixture()).await;
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.mcp_runtime = runtime;
        call_with_args("mcp_prompt_fixture", args)
            .eval_mcp(&ctx)
            .await
    }

    const FLATTENED_SUMMARIZE_PROMPT: &str =
        "[user]\nSummarize notes.txt\n\n[assistant]\nIn which style?\n\n[user]\nConcise.";

    #[test]
    fn expand_uri_template_substitutes_simple_vars() {
        let args = template_args(&[("path", json!("docs")), ("name", json!("readme"))]);
        assert_eq!(
            expand_uri_template("file:///{path}/{name}", &args).unwrap(),
            "file:///docs/readme"
        );
    }

    #[test]
    fn expand_uri_template_stringifies_numbers_and_bools() {
        let args = template_args(&[("id", json!(42)), ("flag", json!(true))]);
        assert_eq!(
            expand_uri_template("item://{id}/{flag}", &args).unwrap(),
            "item://42/true"
        );
    }

    #[test]
    fn expand_uri_template_percent_encodes_values() {
        let args = template_args(&[("q", json!("a b/c✓"))]);
        assert_eq!(
            expand_uri_template("search://{q}", &args).unwrap(),
            "search://a%20b%2Fc%E2%9C%93"
        );
    }

    #[test]
    fn expand_uri_template_without_placeholders_is_noop() {
        let args = template_args(&[("unused", json!("x"))]);
        assert_eq!(
            expand_uri_template("file:///static", &args).unwrap(),
            "file:///static"
        );
    }

    #[test]
    fn expand_uri_template_missing_variable_names_it() {
        let err = expand_uri_template("file:///{path}", &template_args(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'path'"), "{err}");
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn expand_uri_template_rejects_higher_level_operators() {
        let args = template_args(&[("var", json!("v"))]);
        for operator in ["+", "#", ".", "/", ";", "?", "&"] {
            let err = expand_uri_template(&format!("x://{{{operator}var}}"), &args)
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("'{operator}'")), "{err}");
            assert!(err.contains("Level 1 simple substitution"), "{err}");
        }
    }

    #[test]
    fn expand_uri_template_rejects_modifiers_and_multi_vars() {
        let args = template_args(&[("a", json!("v")), ("b", json!("w")), ("var", json!("v"))]);
        for (template, construct) in [
            ("x://{var*}", "'*' explode modifier"),
            ("x://{var:3}", "':' prefix modifier"),
            ("x://{a,b}", "',' multi-variable"),
        ] {
            let err = expand_uri_template(template, &args)
                .unwrap_err()
                .to_string();
            assert!(err.contains(construct), "{err}");
            assert!(err.contains("Level 1 simple substitution"), "{err}");
        }
    }

    #[test]
    fn expand_uri_template_unclosed_brace_errors() {
        let err = expand_uri_template("file:///{path", &template_args(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unclosed"), "{err}");
    }

    #[test]
    fn expand_uri_template_rejects_invalid_variable_names() {
        let err = expand_uri_template("x://{va r}", &template_args(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid variable name"), "{err}");
    }

    #[test]
    fn expand_uri_template_rejects_non_scalar_values() {
        for value in [json!(null), json!(["a"]), json!({"k": "v"})] {
            let args = template_args(&[("v", value)]);
            let err = expand_uri_template("x://{v}", &args)
                .unwrap_err()
                .to_string();
            assert!(err.contains("string, number, or boolean"), "{err}");
        }
    }

    #[test]
    fn parse_resource_content_classifies_by_field_presence() {
        assert!(matches!(
            parse_resource_content(&json!({"uri": "u", "text": "hi"})).unwrap(),
            ResourceContentBody::Text(text) if text == "hi"
        ));
        assert!(matches!(
            parse_resource_content(&json!({"uri": "u", "blob": "aGk="})).unwrap(),
            ResourceContentBody::Blob(blob) if blob == "aGk="
        ));
    }

    #[test]
    fn parse_resource_content_rejects_both_and_neither() {
        let err = parse_resource_content(&json!({"text": "t", "blob": "b"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("both"), "{err}");

        let err = parse_resource_content(&json!({"uri": "u"}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("neither"), "{err}");
    }

    #[test]
    fn functions_mcp_read_declaration_has_paging_params() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec![mcp_features("srv", false, true, false)]);
        let decl = f.find("mcp_read_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        for param in ["uri", "arguments", "pattern", "offset", "max_bytes"] {
            assert!(props.contains_key(param), "missing {param}");
        }
        assert_eq!(props.len(), 5);
        assert_eq!(props["offset"].default, Some(Value::from(0usize)));
        assert_eq!(
            props["max_bytes"].default,
            Some(Value::from(render::DEFAULT_TEXT_MAX_BYTES))
        );
        assert_eq!(decl.parameters.required, Some(vec!["uri".to_string()]));
    }

    #[test]
    fn mcp_read_routes_through_the_concurrent_mcp_path() {
        assert!(is_mcp_meta_function("mcp_read_x"));
    }

    #[test]
    fn eval_mcp_read_returns_rendered_text() {
        let output = run_async(eval_mcp_read(json!({"uri": FIXTURE_LOG_URI}))).unwrap();

        assert_eq!(output["uri"], FIXTURE_LOG_URI);
        assert_eq!(output["mime_type"], "text/plain");
        assert_eq!(output["text"], FIXTURE_LOG_TEXT);
        assert_eq!(output["truncated"], false);
        assert_eq!(output["next_offset"], Value::Null);
    }

    #[test]
    fn eval_routes_mcp_read_to_resource_handler() {
        let output = run_async(async {
            let (runtime, _server) = fixture_runtime(resources_fixture()).await;
            let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
            ctx.tool_scope
                .functions
                .append_mcp_meta_functions(vec![mcp_features("fixture", true, true, false)]);
            ctx.tool_scope.mcp_runtime = runtime;
            let call = call_with_args("mcp_read_fixture", json!({"uri": FIXTURE_LOG_URI}));
            call.eval(&mut ctx).await
        })
        .unwrap();

        assert_eq!(output["text"], FIXTURE_LOG_TEXT);
    }

    #[test]
    fn functions_mcp_prompt_declaration_has_prompt_and_arguments_params() {
        let mut f = Functions::default();
        f.append_mcp_meta_functions(vec![mcp_features("srv", false, false, true)]);
        let decl = f.find("mcp_prompt_srv").unwrap();
        let props = decl.parameters.properties.as_ref().unwrap();
        assert!(props.contains_key("prompt"));
        assert!(props.contains_key("arguments"));
        assert_eq!(props.len(), 2);
        assert_eq!(decl.parameters.required, Some(vec!["prompt".to_string()]));
    }

    #[test]
    fn eval_mcp_routes_mcp_prompt_to_prompt_handler() {
        let fixture = prompts_fixture();
        let get_prompt_calls = Arc::clone(&fixture.get_prompt_calls);
        let call_tool_calls = Arc::clone(&fixture.call_tool_calls);
        let output = run_async(async {
            let (runtime, _server) = fixture_runtime(fixture).await;
            let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
            ctx.tool_scope.mcp_runtime = runtime;
            let call = call_with_args(
                "mcp_prompt_fixture",
                json!({"prompt": "summarize", "arguments": {"path": "notes.txt"}}),
            );
            call.eval_mcp(&ctx).await
        })
        .unwrap();

        assert_eq!(output, Value::String(FLATTENED_SUMMARIZE_PROMPT.into()));
        assert_eq!(get_prompt_calls.load(Ordering::SeqCst), 1);
        assert_eq!(call_tool_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn eval_routes_mcp_prompt_to_prompt_handler() {
        let fixture = prompts_fixture();
        let get_prompt_calls = Arc::clone(&fixture.get_prompt_calls);
        let call_tool_calls = Arc::clone(&fixture.call_tool_calls);
        let output = run_async(async {
            let (runtime, _server) = fixture_runtime(fixture).await;
            let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
            ctx.tool_scope
                .functions
                .append_mcp_meta_functions(vec![mcp_features("fixture", true, false, true)]);
            ctx.tool_scope.mcp_runtime = runtime;
            let call = call_with_args(
                "mcp_prompt_fixture",
                json!({"prompt": "summarize", "arguments": {"path": "notes.txt"}}),
            );
            call.eval(&mut ctx).await
        })
        .unwrap();

        assert_eq!(output, Value::String(FLATTENED_SUMMARIZE_PROMPT.into()));
        assert_eq!(get_prompt_calls.load(Ordering::SeqCst), 1);
        assert_eq!(call_tool_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn eval_mcp_prompt_missing_required_arg_returns_teaching_error() {
        let output = run_async(eval_mcp_prompt(json!({"prompt": "summarize"}))).unwrap();

        let err = output["tool_call_error"].as_str().unwrap();
        assert!(
            err.contains("Missing required prompt argument(s): path"),
            "{err}"
        );
    }

    #[test]
    fn eval_mcp_prompt_unknown_prompt_returns_teaching_error() {
        let output = run_async(eval_mcp_prompt(json!({"prompt": "ghost"}))).unwrap();

        let err = output["tool_call_error"].as_str().unwrap();
        assert!(
            err.contains("Prompt 'ghost' not found on MCP server 'fixture'"),
            "{err}"
        );
        assert!(err.contains("kind \"prompt\""), "{err}");
    }

    #[test]
    fn eval_mcp_prompt_rejects_non_string_argument_values() {
        let output = run_async(eval_mcp_prompt(
            json!({"prompt": "summarize", "arguments": {"path": 5}}),
        ))
        .unwrap();

        let err = output["tool_call_error"].as_str().unwrap();
        assert!(err.contains("prompt arguments are strings"), "{err}");
    }

    #[test]
    fn eval_mcp_read_pages_text_with_offset() {
        let (page1, page2) = run_async(async {
            let (runtime, _server) = fixture_runtime(resources_fixture()).await;
            let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
            ctx.tool_scope.mcp_runtime = runtime;
            let call = call_with_args(
                "mcp_read_fixture",
                json!({"uri": FIXTURE_LOG_URI, "max_bytes": 20}),
            );
            let page1 = call.eval_mcp(&ctx).await.unwrap();
            let call = call_with_args(
                "mcp_read_fixture",
                json!({"uri": FIXTURE_LOG_URI, "offset": page1["next_offset"], "max_bytes": 20}),
            );
            let page2 = call.eval_mcp(&ctx).await.unwrap();
            (page1, page2)
        });

        assert_eq!(page1["truncated"], true);
        assert!(page1["note"].as_str().unwrap().contains("204800"));
        let text1 = page1["text"].as_str().unwrap();
        let text2 = page2["text"].as_str().unwrap();
        assert!(!text2.is_empty());
        assert!(FIXTURE_LOG_TEXT.starts_with(&format!("{text1}{text2}")));
    }

    #[test]
    fn eval_mcp_read_pattern_filters_lines_with_context() {
        let output = run_async(eval_mcp_read(
            json!({"uri": FIXTURE_LOG_URI, "pattern": "café"}),
        ))
        .unwrap();

        let text = output["text"].as_str().unwrap();
        assert!(text.contains("6:ERROR: café overheated"), "{text}");
        assert!(text.contains("4-fourth line"), "{text}");
        assert!(!text.contains("disk full"), "{text}");
        assert_eq!(output["total_bytes"], text.len());
    }

    #[test]
    fn eval_mcp_read_invalid_pattern_returns_teaching_error() {
        let output = run_async(eval_mcp_read(
            json!({"uri": FIXTURE_LOG_URI, "pattern": "("}),
        ))
        .unwrap();

        let err = output["tool_call_error"].as_str().unwrap();
        assert!(err.contains("Invalid filter pattern"), "{err}");
    }

    #[test]
    #[serial]
    fn eval_mcp_read_blob_spills_with_metadata() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let cache_dir = env::temp_dir().join(format!(
            "coyote-read-blob-{}-{}",
            process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&cache_dir).unwrap();
        let env_name = get_env_name("cache_dir");
        let previous = env::var_os(&env_name);
        unsafe { env::set_var(&env_name, &cache_dir) };

        let output = run_async(eval_mcp_read(json!({"uri": FIXTURE_BLOB_URI}))).unwrap();

        unsafe {
            match previous {
                Some(value) => env::set_var(&env_name, value),
                None => env::remove_var(&env_name),
            }
        }

        assert_eq!(output["spilled"], true);
        assert_eq!(output["uri"], FIXTURE_BLOB_URI);
        assert_eq!(output["mime_type"], "application/pdf");
        assert_eq!(output["sha256"].as_str().unwrap().len(), 64);
        let path = PathBuf::from(output["path"].as_str().unwrap());
        assert!(path.starts_with(&cache_dir));
        assert_eq!(fs::read(&path).unwrap(), FIXTURE_BLOB_BYTES);

        fs::remove_dir_all(&cache_dir).unwrap();
    }

    #[test]
    fn eval_mcp_read_multi_content_returns_array() {
        let output = run_async(eval_mcp_read(json!({"uri": "file:///multi"}))).unwrap();

        let items = output.as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["text"], "first");
        assert_eq!(items[1]["text"], "second");
        assert_eq!(items[2]["text"], "third");
        assert_eq!(items[1]["uri"], "file:///multi/1");
    }

    #[test]
    fn eval_mcp_read_multi_content_enforces_overall_ceiling() {
        let output = run_async(eval_mcp_read(
            json!({"uri": "file:///huge", "max_bytes": render::TEXT_MAX_BYTES_CLAMP}),
        ))
        .unwrap();

        let items = output.as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["text"].as_str().unwrap().len(), 150 * 1024);
        let marker = &items[1];
        assert_eq!(marker["truncated"], true);
        assert_eq!(marker["omitted_items"], 2);
        let note = marker["note"].as_str().unwrap();
        assert!(note.contains("2 content item(s) omitted"), "{note}");
        assert!(note.contains("204800"), "{note}");
    }

    #[test]
    fn eval_mcp_read_expands_template_end_to_end() {
        let output = run_async(eval_mcp_read(json!({
            "uri": "file:///{path}/{name}",
            "arguments": {"path": "docs", "name": "readme"},
        })))
        .unwrap();

        assert_eq!(output["uri"], "file:///docs/readme");
        assert_eq!(output["text"], "readme body");
    }

    #[test]
    fn eval_mcp_read_attaches_catalog_audience() {
        let output = run_async(eval_mcp_read(json!({"uri": FIXTURE_ANNOTATED_URI}))).unwrap();

        assert_eq!(output["audience"], json!(["user"]));
        assert_eq!(output["text"], FIXTURE_ANNOTATED_TEXT);
    }

    #[test]
    fn eval_mcp_read_omits_audience_for_unannotated_uri() {
        let output = run_async(eval_mcp_read(json!({"uri": FIXTURE_LOG_URI}))).unwrap();

        assert!(output.get("audience").is_none());
    }

    #[test]
    fn mcp_error_display_strips_terminal_escapes() {
        let hostile = "fail\u{1b}[31mred\u{1b}]0;pwn\u{7}end";

        let display = mcp_error_display(hostile);

        assert!(!display.contains('\u{1b}'));
        assert_eq!(display, "⚠️ failredend ⚠️");
        // The payload keeps the raw message; only the terminal string differs.
        assert_ne!(display, format!("⚠️ {hostile} ⚠️"));
    }

    #[test]
    fn render_tool_result_passes_in_bounds_result_through_unchanged() {
        let mut result = CallToolResult::success(vec![ContentBlock::text("small text")]);
        result.structured_content = Some(json!({"rows": [1, 2, 3]}));

        let bounded = render_tool_result(serde_json::to_value(&result).unwrap(), "srv").unwrap();

        assert_eq!(bounded, serde_json::to_value(&result).unwrap());
        assert_eq!(bounded["isError"], false);
    }

    #[test]
    fn render_tool_result_clamps_oversized_text() {
        let result = CallToolResult::success(vec![ContentBlock::text(
            "x".repeat(render::TEXT_MAX_BYTES_CLAMP + 10),
        )]);

        let bounded = render_tool_result(serde_json::to_value(&result).unwrap(), "srv").unwrap();

        let item = &bounded["content"][0];
        assert_eq!(
            item["text"].as_str().unwrap().len(),
            render::TEXT_MAX_BYTES_CLAMP
        );
        assert_eq!(item["truncated"], true);
        assert_eq!(item["total_bytes"], render::TEXT_MAX_BYTES_CLAMP + 10);
        assert!(
            item["note"]
                .as_str()
                .unwrap()
                .contains("TEXT_MAX_BYTES_CLAMP")
        );
        assert_eq!(bounded["isError"], false);
    }

    #[test]
    #[serial]
    fn render_tool_result_spills_blob_content_without_base64() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let cache_dir = env::temp_dir().join(format!(
            "coyote-tool-blob-{}-{}",
            process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&cache_dir).unwrap();
        let env_name = get_env_name("cache_dir");
        let previous = env::var_os(&env_name);
        unsafe { env::set_var(&env_name, &cache_dir) };

        let b64 = STANDARD.encode(FIXTURE_BLOB_BYTES);
        let result = CallToolResult::success(vec![ContentBlock::image(b64.clone(), "image/png")]);
        let bounded = render_tool_result(serde_json::to_value(&result).unwrap(), "srv");

        unsafe {
            match previous {
                Some(value) => env::set_var(&env_name, value),
                None => env::remove_var(&env_name),
            }
        }

        let bounded = bounded.unwrap();
        let item = &bounded["content"][0];
        assert_eq!(item["spilled"], true);
        assert_eq!(item["mime_type"], "image/png");
        assert_eq!(item["sha256"].as_str().unwrap().len(), 64);
        assert!(
            !bounded.to_string().contains(&b64),
            "base64 payload must not reach model context"
        );

        fs::remove_dir_all(&cache_dir).unwrap();
    }

    #[test]
    fn render_tool_result_inlines_utf8_blob_as_text() {
        let b64 = STANDARD.encode("hello ✓ world");
        let result = CallToolResult::success(vec![ContentBlock::image(b64.clone(), "image/png")]);

        let bounded = render_tool_result(serde_json::to_value(&result).unwrap(), "srv").unwrap();

        let item = &bounded["content"][0];
        assert_eq!(item["type"], "text");
        assert_eq!(item["text"], "hello ✓ world");
        assert!(item.get("spilled").is_none());
        assert!(!bounded.to_string().contains(&b64));
    }

    #[test]
    fn render_tool_result_degrades_undecodable_blob_item() {
        let value = json!({
            "content": [
                {"type": "image", "data": "!!!not base64!!!", "mimeType": "image/png"},
                {"type": "text", "text": "still here"},
            ],
        });

        let bounded = render_tool_result(value, "srv").unwrap();

        let error = bounded["content"][0]["error"].as_str().unwrap();
        assert!(error.contains("base64"), "{error}");
        assert_eq!(bounded["content"][0]["mime_type"], "image/png");
        assert_eq!(bounded["content"][1]["text"], "still here");
    }

    #[test]
    fn render_tool_result_replaces_oversized_structured_content() {
        let mut result = CallToolResult::success(vec![]);
        result.structured_content =
            Some(json!({"blob": "x".repeat(render::TEXT_MAX_BYTES_CLAMP + 1)}));

        let bounded = render_tool_result(serde_json::to_value(&result).unwrap(), "srv").unwrap();

        let structured = &bounded["structuredContent"];
        assert_eq!(structured["truncated"], true);
        assert!(
            structured["note"]
                .as_str()
                .unwrap()
                .contains("TEXT_MAX_BYTES_CLAMP")
        );
        assert!(bounded.to_string().len() < render::TEXT_MAX_BYTES_CLAMP);
    }

    #[test]
    fn render_tool_result_clamps_embedded_resource_metadata() {
        let uri = "u".repeat(render::METADATA_MAX_BYTES + 1);
        let value = json!({
            "content": [{"type": "resource", "resource": {"uri": uri, "text": "hi"}}],
        });

        let bounded = render_tool_result(value, "srv").unwrap();

        let resource = &bounded["content"][0]["resource"];
        assert!(
            resource["uri"]
                .as_str()
                .unwrap()
                .contains("METADATA_MAX_BYTES")
        );
        assert_eq!(resource["text"], "hi");
    }

    #[test]
    fn render_resource_content_clamps_metadata_strings() {
        let uri = format!("file:///{}", "u".repeat(render::METADATA_MAX_BYTES));
        let mime = format!("text/{}", "m".repeat(render::METADATA_MAX_BYTES));
        let item = json!({"uri": uri, "mimeType": mime, "text": "hi"});

        let value = render_resource_content(&item, None, 0, None, "srv").unwrap();

        assert!(
            value["uri"]
                .as_str()
                .unwrap()
                .contains("METADATA_MAX_BYTES")
        );
        assert!(
            value["mime_type"]
                .as_str()
                .unwrap()
                .contains("METADATA_MAX_BYTES")
        );
        assert_eq!(value["text"], "hi");
    }

    #[test]
    fn eval_mcp_invoke_bounds_tool_results_end_to_end() {
        let mut result = CallToolResult::success(vec![ContentBlock::text("hi")]);
        result.structured_content = Some(json!({"ok": true}));
        let fixture = FixtureServer {
            tool_result: Some(result),
            ..Default::default()
        };
        let call_tool_calls = Arc::clone(&fixture.call_tool_calls);

        let output = run_async(async {
            let (runtime, _server) = fixture_runtime(fixture).await;
            let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
            ctx.tool_scope.mcp_runtime = runtime;
            call_with_args("mcp_invoke_fixture", json!({"tool": "dup"}))
                .eval_mcp(&ctx)
                .await
        })
        .unwrap();

        assert_eq!(output["content"][0]["text"], "hi");
        assert_eq!(output["structuredContent"], json!({"ok": true}));
        assert_eq!(output["isError"], false);
        assert_eq!(call_tool_calls.load(Ordering::SeqCst), 1);
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

    #[test]
    fn thinking_block_matches_anthropic_wire_format() {
        let block = ThinkingBlock::Thinking {
            thinking: "chain of thought".to_string(),
            signature: "sig123".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"type": "thinking", "thinking": "chain of thought", "signature": "sig123"})
        );

        let redacted = ThinkingBlock::RedactedThinking {
            data: "opaque".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&redacted).unwrap(),
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
    }

    #[test]
    fn tool_result_deserializes_without_text_and_thinking() {
        let yaml = "call:\n  name: my_tool\n  arguments: {}\noutput: ok\n";
        let result: ToolResult = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(result.call.name, "my_tool");
        assert!(result.text.is_none());
        assert!(result.thinking.is_empty());
    }

    #[test]
    fn parse_arguments_passes_through_object() {
        let tc = call_with_args("t", json!({"x": 1, "y": "hello"}));
        assert_eq!(tc.parse_arguments().unwrap(), json!({"x": 1, "y": "hello"}));
    }

    #[test]
    fn parse_arguments_deserializes_json_string() {
        let tc = call_with_args("t", json!(r#"{"a": true}"#));
        assert_eq!(tc.parse_arguments().unwrap(), json!({"a": true}));
    }

    #[test]
    fn parse_arguments_returns_err_for_invalid_json_string() {
        let tc = call_with_args("t", json!("not json {"));
        assert!(tc.parse_arguments().is_err());
    }

    #[test]
    fn parse_arguments_returns_err_for_non_object_non_string() {
        let tc = call_with_args("t", json!(42));
        assert!(tc.parse_arguments().is_err());
    }

    #[test]
    fn write_file_atomic_writes_and_skips_unchanged() {
        let dir = temp_file("-atomic-", "");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shim");

        write_file_atomic(&path, "one", Some(0o755)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ino = fs::metadata(&path).unwrap().ino();
            write_file_atomic(&path, "one", Some(0o755)).unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().ino(),
                ino,
                "unchanged content must not be rewritten"
            );
        }

        write_file_atomic(&path, "two", None).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no tmp files left behind"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_file_atomic_concurrent_writers_to_same_target() {
        let dir = temp_file("-atomic-concurrent-", "");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shim");

        let contents: Vec<String> = (0..8)
            .map(|i| format!("#!/bin/sh\necho writer-{i}\n"))
            .collect();
        thread::scope(|scope| {
            for content in &contents {
                scope.spawn(|| {
                    for _ in 0..50 {
                        write_file_atomic(&path, content, Some(0o755)).unwrap();
                    }
                });
            }
        });

        let final_content = fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(&final_content),
            "final content must be one writer's complete content, got: {final_content:?}"
        );
        assert_eq!(
            fs::read_dir(&dir).unwrap().count(),
            1,
            "no tmp files left behind"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn run_llm_function_includes_llm_output_on_nonzero_exit() {
        let result = run_llm_function(
            "bash".into(),
            vec![
                "-c".into(),
                "echo partial-output >> \"$LLM_OUTPUT\"; echo err-text >&2; exit 3".into(),
            ],
            HashMap::new(),
            None,
        )
        .unwrap()
        .expect("nonzero exit must return an error payload");

        let json: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            json["tool_call_error"]
                .as_str()
                .unwrap()
                .contains("exited with code 3")
        );
        assert_eq!(json["stderr"], "err-text");
        assert_eq!(json["output"], "partial-output\n");
    }

    #[test]
    fn bin_entry_stem_strips_run_prefix_and_extension() {
        assert_eq!(bin_entry_stem("fs_grep"), "fs_grep");
        assert_eq!(bin_entry_stem("fs_grep.cmd"), "fs_grep");
        assert_eq!(bin_entry_stem("run-web_search.ts"), "web_search");
        assert_eq!(bin_entry_stem("run-fs_grep.sh"), "fs_grep");
    }

    #[test]
    fn prune_stale_bin_entries_removes_only_stale_files() {
        let dir = temp_file("-prune-", "");
        fs::create_dir_all(dir.join("nested")).unwrap();
        for name in [
            "fs_grep",
            "run-web_search.ts",
            "old_tool",
            "run-old_tool.ts",
            "myagent",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let valid_stems: HashSet<String> = ["fs_grep", "web_search"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        prune_stale_bin_entries(&dir, &valid_stems, Some("myagent")).unwrap();

        assert!(dir.join("fs_grep").exists());
        assert!(dir.join("run-web_search.ts").exists());
        assert!(dir.join("myagent").exists(), "agent binary must survive");
        assert!(!dir.join("old_tool").exists());
        assert!(!dir.join("run-old_tool.ts").exists());
        assert!(!dir.join("nested").exists(), "directories must be removed");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_stale_bin_entries_creates_missing_dir() {
        let dir = temp_file("-prune-missing-", "");
        prune_stale_bin_entries(&dir, &HashSet::new(), None).unwrap();
        assert!(dir.is_dir());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eval_tool_calls_partitions_mcp_and_sequential_then_resorts() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let calls = vec![
            call("unknown_first", Some("id-1")),
            ToolCall::new(
                "mcp_search_foo".into(),
                json!({"query": "q"}),
                Some("id-2".into()),
            ),
            call("unknown_last", Some("id-3")),
        ];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].call.name, "unknown_first");
        assert_eq!(results[1].call.name, "mcp_search_foo");
        assert_eq!(results[2].call.name, "unknown_last");

        for sequential in [&results[0], &results[2]] {
            let err = sequential.output["tool_call_error"].as_str().unwrap();
            assert!(
                err.contains("use only tools listed in your catalog"),
                "{err}"
            );
        }
        let mcp_err = results[1].output["tool_call_error"].as_str().unwrap();
        assert!(mcp_err.starts_with("MCP search failed"), "{mcp_err}");
        assert!(!mcp_err.contains("use only tools listed in your catalog"));
    }

    #[test]
    fn eval_tool_calls_isolates_failures_within_a_batch() {
        let app = AppState {
            config: Arc::new(AppConfig {
                auto_continue: true,
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_todo_functions();
        let calls = vec![
            ToolCall::new(
                "todo__init".into(),
                json!({"goal": "ship it"}),
                Some("id-1".into()),
            ),
            call("unknown_tool", Some("id-2")),
        ];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].call.name, "todo__init");
        assert_eq!(results[0].output["status"], "ok");
        assert!(results[0].output.get("tool_call_error").is_none());
        let err = results[1].output["tool_call_error"].as_str().unwrap();
        assert!(err.contains("Unexpected call"), "{err}");
    }

    #[test]
    fn eval_tool_calls_reports_loop_alert_without_executing() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let looped = call_with_args("looped_tool", json!({"a": 1}));
        ctx.tool_scope.tool_tracker.record_call(looped.clone());
        ctx.tool_scope.tool_tracker.record_call(looped.clone());
        let calls = vec![
            looped,
            ToolCall::new("other_tool".into(), json!({}), Some("id-2".into())),
        ];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        assert_eq!(results.len(), 2);
        let alert = results[0].output.as_str().unwrap();
        assert!(alert.starts_with("{\"tool_call_loop_alert\":"), "{alert}");
        let err = results[1].output["tool_call_error"].as_str().unwrap();
        assert!(err.contains("Unexpected call"), "{err}");
    }

    #[test]
    fn eval_tool_calls_truncates_with_global_max_chars() {
        let app = AppState {
            config: Arc::new(AppConfig {
                max_tool_result_chars: Some(50),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        let calls = vec![ToolCall::new(
            "unknown_tool".into(),
            json!({"padding": "x".repeat(200)}),
            Some("id-1".into()),
        )];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        let out = results[0].output.as_str().unwrap();
        assert!(
            out.starts_with("[truncated: tool output exceeded 50 chars]\n"),
            "{out}"
        );
    }

    #[test]
    fn eval_tool_calls_agent_max_chars_overrides_global() {
        let app = AppState {
            config: Arc::new(AppConfig {
                max_tool_result_chars: Some(5000),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            max_tool_result_chars: Some(30),
            ..Default::default()
        }));
        let calls = vec![ToolCall::new(
            "unknown_tool".into(),
            json!({"padding": "x".repeat(200)}),
            Some("id-1".into()),
        )];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        let out = results[0].output.as_str().unwrap();
        assert!(
            out.starts_with("[truncated: tool output exceeded 30 chars]\n"),
            "{out}"
        );
    }

    #[test]
    fn eval_tool_calls_zero_max_chars_disables_truncation() {
        let app = AppState {
            config: Arc::new(AppConfig {
                max_tool_result_chars: Some(0),
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        let calls = vec![ToolCall::new(
            "unknown_tool".into(),
            json!({"padding": "x".repeat(200)}),
            Some("id-1".into()),
        )];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        assert!(results[0].output["tool_call_error"].is_string());
        assert!(!results[0].output.to_string().contains("[truncated"));
    }

    #[test]
    fn eval_tool_calls_no_max_chars_configured_never_truncates() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let calls = vec![ToolCall::new(
            "unknown_tool".into(),
            json!({"padding": "x".repeat(200)}),
            Some("id-1".into()),
        )];

        let results = run_async(eval_tool_calls(&mut ctx, calls)).unwrap();

        assert!(results[0].output["tool_call_error"].is_string());
        assert!(!results[0].output.to_string().contains("[truncated"));
    }

    /// Pins current behavior: when the char cap lands inside a multi-byte
    /// UTF-8 character of the serialized output, no prefix can be taken, so
    /// the truncation marker is prepended to the FULL original output and the
    /// "truncated" result is longer than the input.
    #[test]
    fn truncate_if_needed_utf8_boundary_returns_full_output_with_marker() {
        let serialized = json!("aé").to_string();
        let result = ToolResult::new(call("t", Some("id-1")), json!("aé"));

        let truncated = result.truncate_if_needed(3);

        let out = truncated.output.as_str().unwrap();
        assert_eq!(
            out,
            format!("[truncated: tool output exceeded 3 chars]\n{serialized}")
        );
        assert!(out.len() > serialized.len());
    }

    #[test]
    fn eval_routes_agent_prefix_to_supervisor_handler() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_supervisor_functions();

        let out =
            run_async(call_with_args("agent__check", json!({"id": "x"})).eval(&mut ctx)).unwrap();

        let err = out["tool_call_error"].as_str().unwrap();
        assert!(err.starts_with("Supervisor tool failed"), "{err}");
        assert!(err.contains("No supervisor active"), "{err}");
    }

    #[test]
    fn eval_routes_todo_prefix_to_todo_handler() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_todo_functions();

        let out = run_async(call_with_args("todo__list", json!({})).eval(&mut ctx)).unwrap();

        let err = out["tool_call_error"].as_str().unwrap();
        assert!(err.starts_with("Todo tool failed"), "{err}");
        assert!(err.contains("Auto-continue is not enabled"), "{err}");
    }

    #[test]
    fn eval_routes_memory_prefix_to_memory_handler() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_memory_functions();

        let out = run_async(call_with_args("memory__read", json!({})).eval(&mut ctx)).unwrap();

        let err = out["tool_call_error"].as_str().unwrap();
        assert!(err.starts_with("Memory tool failed"), "{err}");
        assert!(err.contains("name is required"), "{err}");
    }

    #[test]
    fn eval_routes_skill_prefix_to_skill_handler() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_skill_functions();

        let out = run_async(call_with_args("skill__load", json!({})).eval(&mut ctx)).unwrap();

        assert_eq!(out["error"], "name is required");
    }

    #[test]
    fn eval_routes_user_prefix_to_user_handler() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_user_interaction_functions();

        let out = run_async(call_with_args("user__confirm", json!({})).eval(&mut ctx)).unwrap();

        let err = out["tool_call_error"].as_str().unwrap();
        assert!(err.starts_with("User interaction failed"), "{err}");
        assert!(err.contains("'question' is required"), "{err}");
    }

    #[test]
    fn eval_routes_rag_prefix_to_rag_handler() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.tool_scope.functions.append_rag_query_functions();

        let out =
            run_async(call_with_args("rag__query", json!({"query": "x"})).eval(&mut ctx)).unwrap();

        let err = out["tool_call_error"].as_str().unwrap();
        assert!(err.starts_with("RAG query failed"), "{err}");
        assert!(err.contains("No RAG is attached"), "{err}");
    }

    #[test]
    fn eval_unknown_name_errors_with_unexpected_call() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);

        let err = run_async(call_with_args("nope", json!({})).eval(&mut ctx)).unwrap_err();

        assert!(err.to_string().contains("Unexpected call"), "{err}");
    }

    #[test]
    fn eval_mcp_empty_runtime_returns_distinct_error_per_prefix() {
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let cases = [
            (
                "mcp_invoke_ghost",
                json!({"tool": "t"}),
                "MCP tool invocation failed",
            ),
            (
                "mcp_search_ghost",
                json!({"query": "q"}),
                "MCP search failed",
            ),
            (
                "mcp_describe_ghost",
                json!({"tool": "t"}),
                "MCP describe failed",
            ),
            (
                "mcp_read_ghost",
                json!({"uri": "file:///x"}),
                "MCP read failed",
            ),
            (
                "mcp_prompt_ghost",
                json!({"prompt": "p"}),
                "MCP prompt failed",
            ),
        ];

        for (name, args, expected) in cases {
            let out = run_async(call_with_args(name, args).eval_mcp(&ctx)).unwrap();
            let err = out["tool_call_error"].as_str().unwrap();
            assert!(err.starts_with(expected), "{name}: {err}");
        }
    }
}
