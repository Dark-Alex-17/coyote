//! Transitional conversions between the legacy [`Config`] struct and the
//! new [`AppConfig`] + [`RequestContext`] split.

use crate::config::todo::TodoList;

use super::{AppConfig, AppState, Config, RequestContext};

use std::sync::Arc;

impl Config {
    pub fn to_app_config(&self) -> AppConfig {
        AppConfig {
            model_id: self.model_id.clone(),
            temperature: self.temperature,
            top_p: self.top_p,

            dry_run: self.dry_run,
            stream: self.stream,
            save: self.save,
            keybindings: self.keybindings.clone(),
            editor: self.editor.clone(),
            wrap: self.wrap.clone(),
            wrap_code: self.wrap_code,
            vault_password_file: self.vault_password_file.clone(),

            function_calling_support: self.function_calling_support,
            mapping_tools: self.mapping_tools.clone(),
            enabled_tools: self.enabled_tools.clone(),
            visible_tools: self.visible_tools.clone(),

            mcp_server_support: self.mcp_server_support,
            mapping_mcp_servers: self.mapping_mcp_servers.clone(),
            enabled_mcp_servers: self.enabled_mcp_servers.clone(),

            repl_prelude: self.repl_prelude.clone(),
            cmd_prelude: self.cmd_prelude.clone(),
            agent_session: self.agent_session.clone(),

            save_session: self.save_session,
            compression_threshold: self.compression_threshold,
            summarization_prompt: self.summarization_prompt.clone(),
            summary_context_prompt: self.summary_context_prompt.clone(),

            rag_embedding_model: self.rag_embedding_model.clone(),
            rag_reranker_model: self.rag_reranker_model.clone(),
            rag_top_k: self.rag_top_k,
            rag_chunk_size: self.rag_chunk_size,
            rag_chunk_overlap: self.rag_chunk_overlap,
            rag_template: self.rag_template.clone(),

            document_loaders: self.document_loaders.clone(),

            highlight: self.highlight,
            theme: self.theme.clone(),
            left_prompt: self.left_prompt.clone(),
            right_prompt: self.right_prompt.clone(),

            user_agent: self.user_agent.clone(),
            save_shell_history: self.save_shell_history,
            sync_models_url: self.sync_models_url.clone(),

            clients: self.clients.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn to_request_context(&self, app: Arc<AppState>) -> RequestContext {
        let mut mcp_runtime = super::tool_scope::McpRuntime::default();
        if let Some(registry) = &self.mcp_registry {
            mcp_runtime.sync_from_registry(registry);
        }
        let tool_tracker = self
            .tool_call_tracker
            .clone()
            .unwrap_or_else(crate::function::ToolCallTracker::default);
        RequestContext {
            app,
            macro_flag: self.macro_flag,
            info_flag: self.info_flag,
            working_mode: self.working_mode,
            model: self.model.clone(),
            agent_variables: self.agent_variables.clone(),
            role: self.role.clone(),
            session: self.session.clone(),
            rag: self.rag.clone(),
            agent: self.agent.clone(),
            last_message: self.last_message.clone(),
            tool_scope: super::tool_scope::ToolScope {
                functions: self.functions.clone(),
                mcp_runtime,
                tool_tracker,
            },
            supervisor: self.supervisor.clone(),
            parent_supervisor: self.parent_supervisor.clone(),
            self_agent_id: self.self_agent_id.clone(),
            inbox: self.inbox.clone(),
            escalation_queue: self.root_escalation_queue.clone(),
            current_depth: self.current_depth,
            auto_continue_count: 0,
            todo_list: TodoList::default(),
            last_continuation_response: None,
        }
    }
}
