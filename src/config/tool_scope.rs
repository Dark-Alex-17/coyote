use crate::function::{Functions, ToolCallTracker};
use crate::mcp::{CatalogItem, CatalogItemKind, ConnectedServer, McpRegistry};

use anyhow::{Context, Result, anyhow};
use bm25::{Document, Language, SearchEngineBuilder};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Prompt, Resource, ResourceTemplate, Tool,
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

    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
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
        let capabilities = server_handle
            .peer_info()
            .map(|info| info.capabilities.clone());
        let mut items = HashMap::new();

        if capabilities.as_ref().is_none_or(|c| c.tools.is_some()) {
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

        if capabilities.as_ref().is_some_and(|c| c.resources.is_some()) {
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

        if capabilities.as_ref().is_some_and(|c| c.prompts.is_some()) {
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
    }
}

fn prompt_catalog_item(server: &str, prompt: Prompt) -> CatalogItem {
    CatalogItem {
        kind: CatalogItemKind::Prompt,
        name: prompt.name,
        server: server.to_string(),
        description: prompt.description.unwrap_or_default(),
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
    use rmcp::model::{
        ErrorData, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, PromptArgument, PromptsCapability,
        ResourcesCapability, ServerCapabilities, ServerInfo,
    };
    use rmcp::service::{RequestContext, RunningService};
    use rmcp::{RoleServer, ServerHandler, ServiceExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    pub(crate) struct FixtureServer {
        pub(crate) resources_capability: bool,
        pub(crate) prompts_capability: bool,
        pub(crate) fail_resource_listings: bool,
        pub(crate) list_resources_calls: Arc<AtomicUsize>,
        pub(crate) list_prompts_calls: Arc<AtomicUsize>,
    }

    impl ServerHandler for FixtureServer {
        fn get_info(&self) -> ServerInfo {
            let mut capabilities = ServerCapabilities::builder().enable_tools().build();
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

        async fn list_prompts(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListPromptsResult, ErrorData> {
            self.list_prompts_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
                "summarize",
                Some("Summarize a document"),
                Some(vec![
                    PromptArgument::new("path")
                        .with_description("Document path")
                        .with_required(true),
                    PromptArgument::new("style"),
                ]),
            )]))
        }
    }

    pub(crate) async fn fixture_runtime(
        fixture: FixtureServer,
    ) -> (McpRuntime, RunningService<RoleServer, FixtureServer>) {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (server, client) = tokio::join!(fixture.serve(server_io), ().serve(client_io));
        let mut runtime = McpRuntime::new();
        runtime.insert("fixture".to_string(), Arc::new(client.unwrap()));
        (runtime, server.unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{FixtureServer, fixture_runtime};
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
        assert!(runtime.server_names().is_empty());
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
}
