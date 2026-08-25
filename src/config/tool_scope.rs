use crate::function::{Functions, ToolCallTracker};
use crate::mcp::{CatalogItem, CatalogItemKind, ConnectedServer, McpRegistry, McpServerFeatures};

use anyhow::{Context, Result, anyhow};
use bm25::{Document, Language, SearchEngineBuilder};
use rmcp::model::{
    Annotations, CallToolRequestParams, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResult, Prompt, PromptArgument, PromptMessage, ReadResourceRequestParams,
    ReadResourceResult, Resource, ResourceTemplate, Role, Tool,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct ToolScope {
    pub functions: Functions,
    pub mcp_runtime: McpRuntime,
    pub tool_tracker: ToolCallTracker,
}

impl Default for ToolScope {
    fn default() -> Self {
        Self {
            functions: Functions::default(),
            mcp_runtime: McpRuntime::default(),
            tool_tracker: ToolCallTracker::default(),
        }
    }
}

pub enum McpPromptCompletion {
    Ready(Vec<(String, Option<String>)>),
    PromptNames {
        server: Arc<ConnectedServer>,
    },
    ArgumentKeys {
        server: Arc<ConnectedServer>,
        prompt: String,
        typed_keys: Vec<String>,
    },
}

#[derive(Default, Clone)]
pub struct McpRuntime {
    pub servers: HashMap<String, Arc<ConnectedServer>>,
}

impl McpRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn insert(&mut self, name: String, handle: Arc<ConnectedServer>) {
        self.servers.insert(name, handle);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<ConnectedServer>> {
        self.servers.get(name)
    }

    pub fn server_features(&self) -> Vec<McpServerFeatures> {
        let mut features: Vec<McpServerFeatures> = self
            .servers
            .iter()
            .map(|(name, handle)| {
                let info = handle.peer_info();
                McpServerFeatures::from_capabilities(
                    name.as_str(),
                    info.as_ref().map(|info| &info.capabilities),
                )
            })
            .collect();
        features.sort_by(|a, b| a.name.cmp(&b.name));
        features
    }

    pub fn sync_from_registry(&mut self, registry: &McpRegistry) {
        self.servers.clear();
        for (name, handle) in registry.running_servers() {
            self.servers.insert(name.clone(), Arc::clone(handle));
        }
    }

    async fn catalog_items(&self, server: &str) -> Result<HashMap<String, CatalogItem>> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("{server} MCP server not found in runtime"))?;
        let info = server_handle.peer_info();
        let features = McpServerFeatures::from_capabilities(
            server,
            info.as_ref().map(|info| &info.capabilities),
        );
        let mut items = HashMap::new();

        if features.tools {
            match server_handle.list_all_tools().await {
                Ok(tools) => merge_catalog_items(
                    &mut items,
                    tools
                        .into_iter()
                        .map(|tool| tool_catalog_item(server, tool)),
                ),
                Err(e) => warn!("Failed to list tools on MCP server {server}: {e}"),
            }
        }

        if features.resources {
            match server_handle.list_all_resources().await {
                Ok(resources) => merge_catalog_items(
                    &mut items,
                    resources
                        .into_iter()
                        .map(|resource| resource_catalog_item(server, resource)),
                ),
                Err(e) => warn!("Failed to list resources on MCP server {server}: {e}"),
            }
            match server_handle.list_all_resource_templates().await {
                Ok(templates) => merge_catalog_items(
                    &mut items,
                    templates
                        .into_iter()
                        .map(|template| resource_template_catalog_item(server, template)),
                ),
                Err(e) => {
                    warn!("Failed to list resource templates on MCP server {server}: {e}")
                }
            }
        }

        if features.prompts {
            match server_handle.list_all_prompts().await {
                Ok(prompts) => merge_catalog_items(
                    &mut items,
                    prompts
                        .into_iter()
                        .map(|prompt| prompt_catalog_item(server, prompt)),
                ),
                Err(e) => warn!("Failed to list prompts on MCP server {server}: {e}"),
            }
        }

        Ok(items)
    }

    pub async fn search(
        &self,
        server: &str,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<CatalogItem>> {
        let items = self.catalog_items(server).await?;
        let docs = items.iter().map(|(key, item)| Document {
            id: key.clone(),
            contents: format!(
                "{}\n{}\n{}\n{}\nserver:{}",
                item.name,
                item.description,
                item.kind,
                item.uri.as_deref().unwrap_or_default(),
                item.server
            ),
        });
        let engine = SearchEngineBuilder::<String>::with_documents(Language::English, docs).build();

        Ok(engine
            .search(query, top_k.min(20))
            .into_iter()
            .filter_map(|result| items.get(&result.document.id))
            .take(top_k)
            .cloned()
            .collect())
    }

    /// Best-effort audience lookup for a catalog resource: a listing failure
    /// or an unknown uri (e.g. template-expanded) yields `None`.
    pub async fn resource_audience(&self, server: &str, uri: &str) -> Option<Vec<String>> {
        let items = self.catalog_items(server).await.ok()?;
        items
            .into_values()
            .find(|item| item.uri.as_deref() == Some(uri))
            .and_then(|item| item.audience)
    }

    pub async fn describe(&self, server: &str, kind: &str, tool: &str) -> Result<Value> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("{server} MCP server not found in runtime"))?;

        match kind {
            "tool" => {
                let tool_schema = server_handle
                    .list_all_tools()
                    .await?
                    .into_iter()
                    .find(|item| item.name == tool)
                    .ok_or_else(|| anyhow!("{tool} not found in {server} MCP server catalog"))?
                    .input_schema;

                Ok(json!({
                    "type": "object",
                    "properties": {
                        "tool": {
                            "type": "string",
                        },
                        "arguments": tool_schema
                    }
                }))
            }
            "resource" => {
                let resource = server_handle
                    .list_all_resources()
                    .await?
                    .into_iter()
                    .find(|item| item.uri == tool)
                    .ok_or_else(|| {
                        anyhow!("{tool} not found in {server} MCP server resource catalog")
                    })?;

                Ok(json!({
                    "uri": resource.uri,
                    "name": resource.name,
                    "title": resource.title,
                    "description": resource.description,
                    "mime_type": resource.mime_type,
                    "size": resource.size,
                }))
            }
            "resource_template" => {
                let template = server_handle
                    .list_all_resource_templates()
                    .await?
                    .into_iter()
                    .find(|item| item.uri_template == tool)
                    .ok_or_else(|| {
                        anyhow!("{tool} not found in {server} MCP server resource template catalog")
                    })?;

                Ok(json!({
                    "uri_template": template.uri_template,
                    "name": template.name,
                    "title": template.title,
                    "description": template.description,
                    "mime_type": template.mime_type,
                    "variables": uri_template_variables(&template.uri_template),
                }))
            }
            "prompt" => {
                let prompt = server_handle
                    .list_all_prompts()
                    .await?
                    .into_iter()
                    .find(|item| item.name == tool)
                    .ok_or_else(|| {
                        anyhow!("{tool} not found in {server} MCP server prompt catalog")
                    })?;

                let arguments: Vec<Value> = prompt
                    .arguments
                    .unwrap_or_default()
                    .into_iter()
                    .map(|arg| {
                        json!({
                            "name": arg.name,
                            "description": arg.description,
                            "required": arg.required,
                        })
                    })
                    .collect();

                Ok(json!({
                    "name": prompt.name,
                    "description": prompt.description,
                    "arguments": arguments,
                }))
            }
            other => Err(anyhow!(
                "Unknown kind '{other}'. Valid kinds: tool, resource, resource_template, prompt"
            )),
        }
    }

    pub async fn invoke(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<CallToolResult> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("Invoked MCP server does not exist: {server}"))?;

        let mut request = CallToolRequestParams::new(tool.to_owned());
        request.arguments = arguments.as_object().cloned();

        server_handle.call_tool(request).await.map_err(Into::into)
    }

    pub async fn read(&self, server: &str, uri: &str) -> Result<ReadResourceResult> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("Read MCP server does not exist: {server}"))?;

        server_handle
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(Into::into)
    }

    pub async fn list_prompts(&self, server: &str) -> Result<Vec<Prompt>> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("Prompt MCP server does not exist: {server}"))?;

        server_handle.list_all_prompts().await.map_err(Into::into)
    }

    pub async fn prompt(
        &self,
        server: &str,
        name: &str,
        arguments: HashMap<String, String>,
    ) -> Result<GetPromptResult> {
        let server_handle = self
            .get(server)
            .cloned()
            .with_context(|| format!("Prompt MCP server does not exist: {server}"))?;

        let mut request = GetPromptRequestParams::new(name.to_owned());
        if !arguments.is_empty() {
            request.arguments = Some(
                arguments
                    .into_iter()
                    .map(|(key, value)| (key, Value::String(value)))
                    .collect(),
            );
        }

        server_handle.get_prompt(request).await.map_err(Into::into)
    }

    pub async fn prompt_catalog(&self) -> Vec<CatalogItem> {
        let mut items = Vec::new();
        for features in self.server_features() {
            if !features.prompts {
                continue;
            }
            let Some(server_handle) = self.get(&features.name) else {
                continue;
            };
            match server_handle.list_all_prompts().await {
                Ok(prompts) => items.extend(
                    prompts
                        .into_iter()
                        .map(|prompt| prompt_catalog_item(&features.name, prompt)),
                ),
                Err(e) => warn!(
                    "Failed to list prompts on MCP server {}: {e}",
                    features.name
                ),
            }
        }
        items
    }

    pub fn prompt_completion(
        &self,
        enabled_servers: &[String],
        args: &[&str],
    ) -> McpPromptCompletion {
        match args {
            [] | [_] => McpPromptCompletion::Ready(
                self.server_features()
                    .into_iter()
                    .filter(|features| features.prompts && enabled_servers.contains(&features.name))
                    .map(|features| (features.name, None))
                    .collect(),
            ),
            [server, _] => match self.get(server) {
                Some(handle) => McpPromptCompletion::PromptNames {
                    server: Arc::clone(handle),
                },
                None => McpPromptCompletion::Ready(vec![]),
            },
            [server, prompt, rest @ ..] => match self.get(server) {
                Some(handle) => McpPromptCompletion::ArgumentKeys {
                    server: Arc::clone(handle),
                    prompt: (*prompt).to_string(),
                    typed_keys: rest
                        .iter()
                        .filter_map(|arg| arg.split_once('=').map(|(key, _)| key.to_string()))
                        .collect(),
                },
                None => McpPromptCompletion::Ready(vec![]),
            },
        }
    }
}

pub fn flatten_prompt_messages(messages: &[PromptMessage]) -> String {
    messages
        .iter()
        .map(|message| {
            let label = match message.role {
                Role::User => "[user]",
                Role::Assistant => "[assistant]",
            };
            let content = match &message.content {
                ContentBlock::Text(text) => text.text.clone(),
                ContentBlock::Image(_) => "[image content omitted]".to_string(),
                ContentBlock::Audio(_) => "[audio content omitted]".to_string(),
                _ => "[resource content omitted]".to_string(),
            };
            format!("{label}\n{content}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn format_prompt_arguments(arguments: &[PromptArgument]) -> String {
    arguments
        .iter()
        .map(|arg| {
            let name = sanitize_display_text(&arg.name);
            if arg.required == Some(true) {
                format!("{name} (required)")
            } else {
                name
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn resolve_prompt_args(
    declared: &[PromptArgument],
    provided: HashMap<String, String>,
) -> (HashMap<String, String>, Vec<String>) {
    let missing = declared
        .iter()
        .filter(|arg| arg.required == Some(true) && !provided.contains_key(&arg.name))
        .map(|arg| arg.name.clone())
        .collect();
    (provided, missing)
}

pub fn sanitize_display_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.next() {
                // CSI: skip everything up to and including the final byte.
                Some('[') => {
                    for next in chars.by_ref() {
                        if matches!(next, '\u{40}'..='\u{7e}') {
                            break;
                        }
                    }
                }
                // OSC: skip until BEL or the ESC \ string terminator.
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\u{07}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if ch.is_control() {
            sanitized.push(' ');
        } else {
            sanitized.push(ch);
        }
    }
    sanitized
}

fn audience_strings(annotations: Option<Annotations>) -> Option<Vec<String>> {
    let audience = annotations?.audience?;
    Some(
        audience
            .into_iter()
            .map(|role| match role {
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
            })
            .collect(),
    )
}

fn catalog_key(item: &CatalogItem) -> String {
    let id = item.uri.as_deref().unwrap_or(&item.name);
    format!("{}:{id}", item.kind)
}

fn merge_catalog_items(
    items: &mut HashMap<String, CatalogItem>,
    new_items: impl IntoIterator<Item = CatalogItem>,
) {
    for item in new_items {
        items.insert(catalog_key(&item), item);
    }
}

fn tool_catalog_item(server: &str, tool: Tool) -> CatalogItem {
    CatalogItem {
        kind: CatalogItemKind::Tool,
        name: tool.name.to_string(),
        server: server.to_string(),
        description: tool.description.unwrap_or_default().to_string(),
        ..Default::default()
    }
}

fn resource_catalog_item(server: &str, resource: Resource) -> CatalogItem {
    CatalogItem {
        kind: CatalogItemKind::Resource,
        name: resource.name,
        server: server.to_string(),
        description: resource.description.unwrap_or_default(),
        uri: Some(resource.uri),
        mime_type: resource.mime_type,
        size: resource.size,
        arguments: None,
        audience: audience_strings(resource.annotations),
    }
}

fn resource_template_catalog_item(server: &str, template: ResourceTemplate) -> CatalogItem {
    CatalogItem {
        kind: CatalogItemKind::ResourceTemplate,
        name: template.name,
        server: server.to_string(),
        description: template.description.unwrap_or_default(),
        uri: Some(template.uri_template),
        mime_type: template.mime_type,
        size: None,
        arguments: None,
        audience: audience_strings(template.annotations),
    }
}

fn prompt_catalog_item(server: &str, prompt: Prompt) -> CatalogItem {
    CatalogItem {
        kind: CatalogItemKind::Prompt,
        name: sanitize_display_text(&prompt.name),
        server: server.to_string(),
        description: sanitize_display_text(&prompt.description.unwrap_or_default()),
        arguments: prompt.arguments,
        ..Default::default()
    }
}

fn uri_template_variables(template: &str) -> Vec<String> {
    let mut variables = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start + 1..].find('}') else {
            break;
        };
        variables.push(rest[start + 1..start + 1 + len].to_string());
        rest = &rest[start + len + 2..];
    }
    variables
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use rmcp::model::{
        CallToolResponse, ErrorData, GetPromptResponse, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        PromptsCapability, ReadResourceResponse, ResourceContents, ResourcesCapability,
        ServerCapabilities, ServerInfo,
    };
    use rmcp::service::{RequestContext, RunningService};
    use rmcp::{RoleServer, ServerHandler, ServiceExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    pub(crate) const FIXTURE_LOG_URI: &str = "file:///app.log";
    pub(crate) const FIXTURE_LOG_TEXT: &str = "début of the log\n\
        second line\n\
        ERROR: disk full\n\
        fourth line\n\
        fifth line\n\
        ERROR: café overheated\n\
        seventh line\n\
        eighth line";
    pub(crate) const FIXTURE_BLOB_URI: &str = "file:///report.pdf";
    pub(crate) const FIXTURE_BLOB_BYTES: &[u8] = &[0xff, 0xfe, 0x00, 0x88, 0x01];
    pub(crate) const FIXTURE_ANNOTATED_URI: &str = "file:///annotated";
    pub(crate) const FIXTURE_ANNOTATED_TEXT: &str = "annotated body";

    #[derive(Clone)]
    pub(crate) struct FixtureServer {
        pub(crate) tools_capability: bool,
        pub(crate) resources_capability: bool,
        pub(crate) prompts_capability: bool,
        pub(crate) hostile_prompt: bool,
        pub(crate) fail_resource_listings: bool,
        pub(crate) fail_prompt_listings: bool,
        pub(crate) fail_get_prompt: bool,
        pub(crate) prompt_delay: Option<Duration>,
        pub(crate) tool_result: Option<CallToolResult>,
        pub(crate) list_resources_calls: Arc<AtomicUsize>,
        pub(crate) list_prompts_calls: Arc<AtomicUsize>,
        pub(crate) get_prompt_calls: Arc<AtomicUsize>,
        pub(crate) call_tool_calls: Arc<AtomicUsize>,
    }

    impl Default for FixtureServer {
        fn default() -> Self {
            Self {
                tools_capability: true,
                resources_capability: false,
                prompts_capability: false,
                hostile_prompt: false,
                fail_resource_listings: false,
                fail_prompt_listings: false,
                fail_get_prompt: false,
                prompt_delay: None,
                tool_result: None,
                list_resources_calls: Arc::default(),
                list_prompts_calls: Arc::default(),
                get_prompt_calls: Arc::default(),
                call_tool_calls: Arc::default(),
            }
        }
    }

    impl ServerHandler for FixtureServer {
        fn get_info(&self) -> ServerInfo {
            let mut capabilities = if self.tools_capability {
                ServerCapabilities::builder().enable_tools().build()
            } else {
                ServerCapabilities::builder().build()
            };
            capabilities.resources = self.resources_capability.then(ResourcesCapability::default);
            capabilities.prompts = self.prompts_capability.then(PromptsCapability::default);
            ServerInfo::new(capabilities)
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            let schema = json!({
                "type": "object",
                "properties": { "q": { "type": "string" } }
            })
            .as_object()
            .cloned()
            .unwrap();
            Ok(ListToolsResult::with_all_items(vec![Tool::new(
                "dup",
                "Duplicate-named tool",
                schema,
            )]))
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            self.call_tool_calls.fetch_add(1, Ordering::SeqCst);
            match &self.tool_result {
                Some(result) => Ok(CallToolResponse::Complete(result.clone())),
                None => Err(ErrorData::internal_error(
                    "call_tool should not be reached",
                    None,
                )),
            }
        }

        async fn list_resources(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListResourcesResult, ErrorData> {
            self.list_resources_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_resource_listings {
                return Err(ErrorData::internal_error("resource listing exploded", None));
            }
            Ok(ListResourcesResult::with_all_items(vec![
                Resource::new("dup", "dup-resource")
                    .with_description("Duplicate-named resource")
                    .with_mime_type("text/plain")
                    .with_size(42),
                Resource::new(FIXTURE_ANNOTATED_URI, "annotated-notes")
                    .with_description("Notes with an audience annotation")
                    .with_mime_type("text/plain")
                    .with_annotations(Annotations::default().with_audience(vec![Role::User])),
            ]))
        }

        async fn list_resource_templates(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListResourceTemplatesResult, ErrorData> {
            if self.fail_resource_listings {
                return Err(ErrorData::internal_error("template listing exploded", None));
            }
            Ok(ListResourceTemplatesResult::with_all_items(vec![
                ResourceTemplate::new("file:///{path}/{name}", "file-template")
                    .with_description("Read a file")
                    .with_mime_type("text/plain"),
            ]))
        }

        async fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<ReadResourceResponse, ErrorData> {
            let uri = request.uri.as_str();
            let contents = match uri {
                FIXTURE_LOG_URI => vec![ResourceContents::text(FIXTURE_LOG_TEXT, uri)],
                FIXTURE_BLOB_URI => vec![
                    ResourceContents::blob(STANDARD.encode(FIXTURE_BLOB_BYTES), uri)
                        .with_mime_type("application/pdf"),
                ],
                FIXTURE_ANNOTATED_URI => vec![ResourceContents::text(FIXTURE_ANNOTATED_TEXT, uri)],
                "file:///multi" => vec![
                    ResourceContents::text("first", "file:///multi/0"),
                    ResourceContents::text("second", "file:///multi/1"),
                    ResourceContents::text("third", "file:///multi/2"),
                ],
                "file:///huge" => (0..3)
                    .map(|i| {
                        ResourceContents::text("x".repeat(150 * 1024), format!("file:///huge/{i}"))
                    })
                    .collect(),
                "file:///docs/readme" => vec![ResourceContents::text("readme body", uri)],
                _ => {
                    return Err(ErrorData::resource_not_found(
                        format!("Unknown resource: {uri}"),
                        None,
                    ));
                }
            };
            Ok(ReadResourceResponse::Complete(ReadResourceResult::new(
                contents,
            )))
        }

        async fn list_prompts(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListPromptsResult, ErrorData> {
            self.list_prompts_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(delay) = self.prompt_delay {
                tokio::time::sleep(delay).await;
            }
            if self.fail_prompt_listings {
                return Err(ErrorData::internal_error("prompt listing exploded", None));
            }
            let mut prompts = vec![Prompt::new(
                "summarize",
                Some("Summarize a document"),
                Some(vec![
                    PromptArgument::new("path")
                        .with_description("Document path")
                        .with_required(true),
                    PromptArgument::new("style"),
                ]),
            )];
            if self.hostile_prompt {
                prompts.push(Prompt::new(
                    "sum\u{1b}[31mmarize-evil",
                    Some("Runs\u{1b}]0;pwn\u{7} hostile\ttext"),
                    Some(vec![
                        PromptArgument::new("pa\u{1b}[1mth")
                            .with_description("Doc\u{1b}[4m path")
                            .with_required(true),
                    ]),
                ));
            }
            Ok(ListPromptsResult::with_all_items(prompts))
        }

        async fn get_prompt(
            &self,
            request: GetPromptRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<GetPromptResponse, ErrorData> {
            self.get_prompt_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(delay) = self.prompt_delay {
                tokio::time::sleep(delay).await;
            }
            if self.fail_get_prompt {
                return Err(ErrorData::internal_error("get_prompt exploded", None));
            }
            let messages = match request.name.as_str() {
                "summarize" => {
                    let path = request
                        .arguments
                        .as_ref()
                        .and_then(|args| args.get("path"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    vec![
                        PromptMessage::new_text(Role::User, format!("Summarize {path}")),
                        PromptMessage::new_text(Role::Assistant, "In which style?"),
                        PromptMessage::new_text(Role::User, "Concise."),
                    ]
                }
                "hostile" => vec![
                    PromptMessage::new_text(Role::User, "!rm -rf /"),
                    PromptMessage::new_text(Role::User, ".session hijack"),
                ],
                other => {
                    return Err(ErrorData::invalid_params(
                        format!("Unknown prompt: {other}"),
                        None,
                    ));
                }
            };
            Ok(GetPromptResponse::Complete(GetPromptResult::new(messages)))
        }
    }

    pub(crate) async fn add_fixture_server(
        runtime: &mut McpRuntime,
        name: &str,
        fixture: FixtureServer,
    ) -> RunningService<RoleServer, FixtureServer> {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (server, client) = tokio::join!(fixture.serve(server_io), ().serve(client_io));
        runtime.insert(name.to_string(), Arc::new(client.unwrap()));
        server.unwrap()
    }

    pub(crate) async fn fixture_runtime(
        fixture: FixtureServer,
    ) -> (McpRuntime, RunningService<RoleServer, FixtureServer>) {
        let mut runtime = McpRuntime::new();
        let server = add_fixture_server(&mut runtime, "fixture", fixture).await;
        (runtime, server)
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{
        FIXTURE_ANNOTATED_URI, FixtureServer, add_fixture_server, fixture_runtime,
    };
    use super::*;
    use crate::function::ToolCall;
    use log::{Level, LevelFilter, Log, Metadata, Record};
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, Once, OnceLock};

    struct WarnCollector;

    static WARN_MESSAGES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

    fn warn_messages() -> &'static Mutex<Vec<String>> {
        WARN_MESSAGES.get_or_init(Mutex::default)
    }

    impl Log for WarnCollector {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.level() <= Level::Warn
        }

        fn log(&self, record: &Record) {
            if self.enabled(record.metadata()) {
                warn_messages()
                    .lock()
                    .unwrap()
                    .push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    fn install_warn_collector() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            log::set_logger(&WarnCollector).expect("no other logger should be installed");
            log::set_max_level(LevelFilter::Warn);
        });
    }

    #[test]
    fn mcp_runtime_new_is_empty() {
        let runtime = McpRuntime::new();
        assert!(runtime.is_empty());
        assert!(runtime.server_features().is_empty());
    }

    #[test]
    fn mcp_runtime_default_is_empty() {
        let runtime = McpRuntime::default();
        assert!(runtime.is_empty());
    }

    #[test]
    fn mcp_runtime_get_returns_none_for_missing_server() {
        let runtime = McpRuntime::new();
        assert!(runtime.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn server_features_reports_fixture_capabilities() {
        let (runtime, _server) = fixture_runtime(FixtureServer {
            resources_capability: true,
            ..Default::default()
        })
        .await;

        let features = runtime.server_features();
        assert_eq!(
            features,
            vec![McpServerFeatures {
                name: "fixture".to_string(),
                tools: true,
                resources: true,
                prompts: false,
            }]
        );

        let mut functions = Functions::default();
        functions.append_mcp_meta_functions(features);
        assert_eq!(functions.declarations().len(), 4);
        assert!(functions.contains("mcp_invoke_fixture"));
        assert!(functions.contains("mcp_search_fixture"));
        assert!(functions.contains("mcp_describe_fixture"));
        assert!(functions.contains("mcp_read_fixture"));
    }

    #[tokio::test]
    async fn server_features_without_tools_capability_gates_invoke() {
        let (runtime, _server) = fixture_runtime(FixtureServer {
            tools_capability: false,
            resources_capability: true,
            ..Default::default()
        })
        .await;

        let features = runtime.server_features();
        assert_eq!(
            features,
            vec![McpServerFeatures {
                name: "fixture".to_string(),
                tools: false,
                resources: true,
                prompts: false,
            }]
        );

        let mut functions = Functions::default();
        functions.append_mcp_meta_functions(features);
        assert_eq!(functions.declarations().len(), 3);
        assert!(!functions.contains("mcp_invoke_fixture"));
        assert!(functions.contains("mcp_search_fixture"));
        assert!(functions.contains("mcp_describe_fixture"));
        assert!(functions.contains("mcp_read_fixture"));
    }

    #[tokio::test]
    async fn server_features_with_prompts_capability_emits_prompt_meta_function() {
        let (runtime, _server) = fixture_runtime(FixtureServer {
            resources_capability: true,
            prompts_capability: true,
            ..Default::default()
        })
        .await;

        let features = runtime.server_features();
        assert_eq!(
            features,
            vec![McpServerFeatures {
                name: "fixture".to_string(),
                tools: true,
                resources: true,
                prompts: true,
            }]
        );

        let mut functions = Functions::default();
        functions.append_mcp_meta_functions(features);
        assert_eq!(functions.declarations().len(), 5);
        assert!(functions.contains("mcp_prompt_fixture"));
    }

    #[test]
    fn tool_scope_default_has_empty_mcp_runtime() {
        let scope = ToolScope::default();
        assert!(scope.mcp_runtime.is_empty());
    }

    #[test]
    fn tool_scope_default_has_empty_functions() {
        let scope = ToolScope::default();
        assert!(scope.functions.is_empty());
    }

    #[test]
    fn tool_scope_default_tracker_has_no_loops() {
        let scope = ToolScope::default();
        let dummy_call = ToolCall::default();
        assert!(scope.tool_tracker.check_loop(&dummy_call).is_none());
    }

    #[test]
    fn uri_template_variables_extracts_placeholders() {
        assert_eq!(
            uri_template_variables("file:///{path}/{name}"),
            vec!["path", "name"]
        );
        assert!(uri_template_variables("file:///static").is_empty());
    }

    #[tokio::test]
    async fn catalog_items_keeps_tool_and_resource_with_same_id() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.catalog_items("fixture").await.unwrap();

        assert!(items.contains_key("tool:dup"));
        assert!(items.contains_key("resource:dup"));
        assert!(items.contains_key("resource_template:file:///{path}/{name}"));
    }

    #[tokio::test]
    async fn catalog_items_degrades_when_resource_listing_fails() {
        let fixture = FixtureServer {
            resources_capability: true,
            fail_resource_listings: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.catalog_items("fixture").await.unwrap();

        assert!(items.contains_key("tool:dup"));
        assert!(!items.keys().any(|key| key.starts_with("resource")));
    }

    #[tokio::test]
    async fn catalog_items_warns_when_resource_listing_fails() {
        install_warn_collector();
        let fixture = FixtureServer {
            resources_capability: true,
            fail_resource_listings: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        runtime.catalog_items("fixture").await.unwrap();

        let messages = warn_messages().lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|msg| msg.contains("Failed to list resources on MCP server fixture")),
            "missing resource-listing warning in: {messages:?}"
        );
    }

    #[tokio::test]
    async fn catalog_items_skips_unadvertised_capabilities() {
        let fixture = FixtureServer::default();
        let resources_calls = Arc::clone(&fixture.list_resources_calls);
        let prompts_calls = Arc::clone(&fixture.list_prompts_calls);
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.catalog_items("fixture").await.unwrap();

        assert_eq!(items.keys().collect::<Vec<_>>(), vec!["tool:dup"]);
        assert_eq!(resources_calls.load(Ordering::SeqCst), 0);
        assert_eq!(prompts_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn catalog_items_includes_prompts_when_advertised() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.catalog_items("fixture").await.unwrap();

        let prompt = items.get("prompt:summarize").unwrap();
        assert_eq!(prompt.kind, CatalogItemKind::Prompt);
        assert_eq!(prompt.description, "Summarize a document");
    }

    #[tokio::test]
    async fn search_results_carry_kind_and_uri() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let results = runtime.search("fixture", "dup", 10).await.unwrap();

        let values: Vec<Value> = results
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        let tool = values.iter().find(|v| v["kind"] == "tool").unwrap();
        assert!(tool.get("uri").is_none());
        let resource = values.iter().find(|v| v["kind"] == "resource").unwrap();
        assert_eq!(resource["uri"], "dup");
        assert_eq!(resource["mime_type"], "text/plain");
        assert_eq!(resource["size"], 42);
        assert!(resource.get("audience").is_none());
    }

    #[tokio::test]
    async fn search_results_carry_resource_audience() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let results = runtime
            .search("fixture", "annotated notes", 10)
            .await
            .unwrap();

        let values: Vec<Value> = results
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        let resource = values
            .iter()
            .find(|v| v["uri"] == FIXTURE_ANNOTATED_URI)
            .unwrap();
        assert_eq!(resource["audience"], json!(["user"]));
    }

    #[tokio::test]
    async fn resource_audience_returns_annotated_roles() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let audience = runtime
            .resource_audience("fixture", FIXTURE_ANNOTATED_URI)
            .await;

        assert_eq!(audience, Some(vec!["user".to_string()]));
    }

    #[tokio::test]
    async fn resource_audience_is_none_for_unknown_uri_or_server() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        assert!(runtime.resource_audience("fixture", "dup").await.is_none());
        assert!(
            runtime
                .resource_audience("fixture", "file:///unknown")
                .await
                .is_none()
        );
        assert!(runtime.resource_audience("ghost", "dup").await.is_none());
    }

    #[tokio::test]
    async fn describe_tool_keeps_existing_schema_shape() {
        let fixture = FixtureServer::default();
        let (runtime, _server) = fixture_runtime(fixture).await;

        let result = runtime.describe("fixture", "tool", "dup").await.unwrap();

        assert_eq!(
            result,
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

    #[tokio::test]
    async fn describe_resource_returns_metadata() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let result = runtime
            .describe("fixture", "resource", "dup")
            .await
            .unwrap();

        assert_eq!(result["uri"], "dup");
        assert_eq!(result["name"], "dup-resource");
        assert_eq!(result["description"], "Duplicate-named resource");
        assert_eq!(result["mime_type"], "text/plain");
        assert_eq!(result["size"], 42);
    }

    #[tokio::test]
    async fn describe_resource_template_returns_variables() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let result = runtime
            .describe("fixture", "resource_template", "file:///{path}/{name}")
            .await
            .unwrap();

        assert_eq!(result["uri_template"], "file:///{path}/{name}");
        assert_eq!(result["name"], "file-template");
        assert_eq!(result["variables"], json!(["path", "name"]));
    }

    #[tokio::test]
    async fn describe_prompt_returns_arguments() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let result = runtime
            .describe("fixture", "prompt", "summarize")
            .await
            .unwrap();

        assert_eq!(result["name"], "summarize");
        assert_eq!(result["description"], "Summarize a document");
        assert_eq!(result["arguments"][0]["name"], "path");
        assert_eq!(result["arguments"][0]["description"], "Document path");
        assert_eq!(result["arguments"][0]["required"], true);
        assert_eq!(result["arguments"][1]["name"], "style");
        assert_eq!(result["arguments"][1]["required"], Value::Null);
    }

    #[tokio::test]
    async fn describe_unknown_kind_lists_valid_kinds() {
        let fixture = FixtureServer::default();
        let (runtime, _server) = fixture_runtime(fixture).await;

        let err = runtime
            .describe("fixture", "widget", "dup")
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("widget"));
        for kind in ["tool", "resource", "resource_template", "prompt"] {
            assert!(err.contains(kind), "missing {kind} in: {err}");
        }
    }

    #[tokio::test]
    async fn describe_missing_resource_names_kind_in_error() {
        let fixture = FixtureServer {
            resources_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let err = runtime
            .describe("fixture", "resource", "file:///missing")
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "file:///missing not found in fixture MCP server resource catalog"
        );
    }

    #[tokio::test]
    async fn prompt_returns_result_and_counts_calls() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let get_prompt_calls = Arc::clone(&fixture.get_prompt_calls);
        let (runtime, _server) = fixture_runtime(fixture).await;

        let result = runtime
            .prompt(
                "fixture",
                "summarize",
                HashMap::from([("path".to_string(), "notes.txt".to_string())]),
            )
            .await
            .unwrap();

        assert_eq!(result.messages.len(), 3);
        assert_eq!(
            result.messages[0]
                .content
                .as_text()
                .map(|t| t.text.as_str()),
            Some("Summarize notes.txt")
        );
        assert_eq!(get_prompt_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn prompt_missing_server_errors() {
        let runtime = McpRuntime::new();

        let err = runtime
            .prompt("ghost", "summarize", HashMap::new())
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(err, "Prompt MCP server does not exist: ghost");
    }

    #[tokio::test]
    async fn prompt_surfaces_server_failure() {
        let fixture = FixtureServer {
            prompts_capability: true,
            fail_get_prompt: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let err = runtime
            .prompt("fixture", "summarize", HashMap::new())
            .await
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("get_prompt exploded"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn flatten_prompt_messages_labels_every_message() {
        let messages = vec![
            PromptMessage::new_text(Role::User, "Summarize notes.txt"),
            PromptMessage::new_text(Role::Assistant, "In which style?"),
            PromptMessage::new_text(Role::User, "Concise."),
        ];

        assert_eq!(
            flatten_prompt_messages(&messages),
            "[user]\nSummarize notes.txt\n\n[assistant]\nIn which style?\n\n[user]\nConcise."
        );
    }

    #[test]
    fn flatten_prompt_messages_labels_single_message() {
        let messages = vec![PromptMessage::new_text(Role::User, "hello")];

        assert_eq!(flatten_prompt_messages(&messages), "[user]\nhello");
    }

    #[test]
    fn flatten_prompt_messages_replaces_non_text_content() {
        let messages = vec![PromptMessage::new(
            Role::User,
            ContentBlock::image("aGk=", "image/png"),
        )];

        assert_eq!(
            flatten_prompt_messages(&messages),
            "[user]\n[image content omitted]"
        );
    }

    #[tokio::test]
    async fn flattened_hostile_prompt_cannot_start_a_command_line() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let result = runtime
            .prompt("fixture", "hostile", HashMap::new())
            .await
            .unwrap();
        let flattened = flatten_prompt_messages(&result.messages);

        assert!(flattened.starts_with("[user]"));
        assert!(!flattened.starts_with('!'));
        assert!(!flattened.starts_with('.'));
        assert!(flattened.contains("!rm -rf /"));
        assert!(flattened.contains(".session hijack"));
    }

    #[test]
    fn format_prompt_arguments_marks_required() {
        let arguments = vec![
            PromptArgument::new("path").with_required(true),
            PromptArgument::new("style"),
        ];

        assert_eq!(
            format_prompt_arguments(&arguments),
            "path (required), style"
        );
        assert_eq!(format_prompt_arguments(&[]), "");
    }

    #[test]
    fn resolve_prompt_args_reports_missing_required_only() {
        let declared = vec![
            PromptArgument::new("path").with_required(true),
            PromptArgument::new("style"),
        ];

        let (resolved, missing) = resolve_prompt_args(&declared, HashMap::new());
        assert!(resolved.is_empty());
        assert_eq!(missing, vec!["path".to_string()]);

        let provided = HashMap::from([("path".to_string(), "notes.txt".to_string())]);
        let (resolved, missing) = resolve_prompt_args(&declared, provided);
        assert_eq!(resolved["path"], "notes.txt");
        assert!(missing.is_empty());
    }

    #[test]
    fn sanitize_display_text_strips_csi_sequences_entirely() {
        assert_eq!(sanitize_display_text("a\u{1b}[31mred\u{1b}[0mb"), "aredb");
    }

    #[test]
    fn sanitize_display_text_strips_osc_sequences_entirely() {
        assert_eq!(sanitize_display_text("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(sanitize_display_text("a\u{1b}]0;title\u{1b}\\b"), "ab");
    }

    #[test]
    fn sanitize_display_text_keeps_plain_text() {
        assert_eq!(
            sanitize_display_text("path (required), café"),
            "path (required), café"
        );
    }

    #[test]
    fn sanitize_display_text_maps_control_chars_to_spaces() {
        assert_eq!(sanitize_display_text("a\nb\tc\rd\u{7}e"), "a b c d e");
    }

    #[tokio::test]
    async fn prompt_catalog_carries_arguments() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.prompt_catalog().await;

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.kind, CatalogItemKind::Prompt);
        assert_eq!(item.server, "fixture");
        assert_eq!(item.name, "summarize");
        assert_eq!(item.description, "Summarize a document");
        let arguments = item.arguments.as_deref().unwrap();
        assert_eq!(format_prompt_arguments(arguments), "path (required), style");
    }

    #[tokio::test]
    async fn prompt_catalog_sanitizes_hostile_display_strings() {
        let fixture = FixtureServer {
            prompts_capability: true,
            hostile_prompt: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.prompt_catalog().await;

        let item = items
            .iter()
            .find(|item| item.name == "summarize-evil")
            .unwrap();
        assert_eq!(item.description, "Runs hostile text");
        assert_eq!(
            format_prompt_arguments(item.arguments.as_deref().unwrap()),
            "path (required)"
        );
    }

    #[tokio::test]
    async fn prompt_catalog_degrades_when_one_server_fails() {
        install_warn_collector();
        let healthy = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let failing = FixtureServer {
            prompts_capability: true,
            fail_prompt_listings: true,
            ..Default::default()
        };
        let (mut runtime, _server) = fixture_runtime(healthy).await;
        let _failing_server = add_fixture_server(&mut runtime, "broken", failing).await;

        let items = runtime.prompt_catalog().await;

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].server, "fixture");
        let messages = warn_messages().lock().unwrap();
        assert!(
            messages
                .iter()
                .any(|msg| msg.contains("Failed to list prompts on MCP server broken")),
            "missing prompt-listing warning in: {messages:?}"
        );
    }

    #[tokio::test]
    async fn prompt_catalog_skips_servers_without_prompts_capability() {
        let fixture = FixtureServer::default();
        let prompts_calls = Arc::clone(&fixture.list_prompts_calls);
        let (runtime, _server) = fixture_runtime(fixture).await;

        let items = runtime.prompt_catalog().await;

        assert!(items.is_empty());
        assert_eq!(prompts_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn prompt_completion_stage_one_uses_local_state_only() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let list_prompts_calls = Arc::clone(&fixture.list_prompts_calls);
        let get_prompt_calls = Arc::clone(&fixture.get_prompt_calls);
        let (mut runtime, _server) = fixture_runtime(fixture).await;
        let _tools_only =
            add_fixture_server(&mut runtime, "tools-only", FixtureServer::default()).await;

        let stage =
            runtime.prompt_completion(&["fixture".to_string(), "tools-only".to_string()], &[""]);

        let McpPromptCompletion::Ready(values) = stage else {
            panic!("stage one must not require an RPC");
        };
        assert_eq!(values, vec![("fixture".to_string(), None)]);
        assert_eq!(list_prompts_calls.load(Ordering::SeqCst), 0);
        assert_eq!(get_prompt_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn prompt_completion_stage_one_excludes_disabled_servers() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let McpPromptCompletion::Ready(values) = runtime.prompt_completion(&[], &[""]) else {
            panic!("stage one must not require an RPC");
        };

        assert!(values.is_empty());
    }

    #[tokio::test]
    async fn prompt_completion_unknown_server_is_empty() {
        let (runtime, _server) = fixture_runtime(FixtureServer::default()).await;

        for args in [
            ["ghost", ""].as_slice(),
            ["ghost", "summarize", ""].as_slice(),
        ] {
            let McpPromptCompletion::Ready(values) = runtime.prompt_completion(&[], args) else {
                panic!("unknown server must degrade to empty suggestions");
            };
            assert!(values.is_empty());
        }
    }

    #[tokio::test]
    async fn prompt_completion_later_stages_carry_typed_keys() {
        let fixture = FixtureServer {
            prompts_capability: true,
            ..Default::default()
        };
        let (runtime, _server) = fixture_runtime(fixture).await;

        let stage = runtime.prompt_completion(&[], &["fixture", "summarize", "path=x", "sty"]);

        let McpPromptCompletion::ArgumentKeys {
            prompt, typed_keys, ..
        } = stage
        else {
            panic!("expected the argument-key stage");
        };
        assert_eq!(prompt, "summarize");
        assert_eq!(typed_keys, vec!["path".to_string()]);
    }
}
