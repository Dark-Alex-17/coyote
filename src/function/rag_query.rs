use super::{FunctionDeclaration, JsonSchema};
use crate::config::{Agent, RequestContext};

use anyhow::{Result, anyhow};
use indexmap::IndexMap;
use serde_json::{Value, json};

pub const RAG_FUNCTION_PREFIX: &str = "rag__";

pub fn rag_query_function_declarations() -> Vec<FunctionDeclaration> {
    vec![FunctionDeclaration {
        name: format!("{RAG_FUNCTION_PREFIX}query"),
        description: "Search the RAG knowledge base attached to this session and return \
                      the most relevant text chunks with their source paths. The relevant \
                      context has already been injected into the prompt up-front; use this \
                      tool to pull additional context on-demand when the initial retrieval \
                      does not fully answer the question. Prefer specific, keyword-rich queries."
            .to_string(),
        parameters: JsonSchema {
            type_value: Some("object".to_string()),
            properties: Some(IndexMap::from([
                (
                    "query".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some(
                            "Natural language search query used to retrieve relevant chunks."
                                .into(),
                        ),
                        ..Default::default()
                    },
                ),
                (
                    "top_k".to_string(),
                    JsonSchema {
                        type_value: Some("integer".to_string()),
                        description: Some(
                            "Maximum number of chunks to return. Defaults to the RAG's \
                             configured top_k when omitted."
                                .into(),
                        ),
                        ..Default::default()
                    },
                ),
            ])),
            required: Some(vec!["query".to_string()]),
            ..Default::default()
        },
        agent: false,
    }]
}

pub async fn handle_rag_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(RAG_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    if ctx.agent.as_ref().is_some_and(Agent::is_builtin) {
        return Ok(json!({
            "status": "error",
            "message": "RAG tools are never available to a built-in agent.",
        }));
    }

    match action {
        "query" => handle_query(ctx, args).await,
        _ => Err(anyhow!("Unknown RAG action: {action}")),
    }
}

async fn handle_query(ctx: &RequestContext, args: &Value) -> Result<Value> {
    let rag = ctx
        .rag
        .clone()
        .ok_or_else(|| anyhow!("No RAG is attached to this session"))?;

    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'query' is required"))?;

    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or_else(|| rag.configured_top_k());

    let rerank_model = rag.configured_reranker().map(|s| s.to_string());

    let chunks = rag
        .search_chunks(query, top_k, rerank_model.as_deref())
        .await?;

    let chunks_json: Vec<Value> = chunks
        .into_iter()
        .map(|(text, source)| json!({ "text": text, "source": source }))
        .collect();

    Ok(json!({
        "rag_name": rag.name(),
        "count": chunks_json.len(),
        "chunks": chunks_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, AppState, WorkingMode};
    use std::sync::Arc;

    #[test]
    fn handle_rag_tool_refuses_a_builtin_agent_before_dispatch() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            name: "envoy".into(),
            ..Default::default()
        }));
        assert!(ctx.agent.as_ref().unwrap().is_builtin());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for action in ["query", "nosuch"] {
            let result = runtime
                .block_on(handle_rag_tool(
                    &mut ctx,
                    &format!("{RAG_FUNCTION_PREFIX}{action}"),
                    &json!({"query": "q"}),
                ))
                .unwrap();
            assert_eq!(result["status"], "error", "{action}: {result}");
            assert_eq!(
                result["message"], "RAG tools are never available to a built-in agent.",
                "{action}: {result}"
            );
        }
    }
}
