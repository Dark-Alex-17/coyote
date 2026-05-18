use super::state::StateManager;
use super::types::RagNode;
use crate::config::RequestContext;
use crate::utils::{create_abort_signal, dimmed_text};
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::time::Duration;
use tokio::time::timeout;

const OUTPUT_KEY: &str = "output";
const DEFAULT_QUERY: &str = "{{initial_prompt}}";
const DEFAULT_RAG_TIMEOUT_SECS: u64 = 120;

pub struct RagNodeExecutor;

impl RagNodeExecutor {
    pub async fn execute(
        node: &RagNode,
        node_id: &str,
        node_next: Option<&str>,
        state_manager: &mut StateManager,
        ctx: &mut RequestContext,
    ) -> Result<String> {
        let query_template = node.query.as_deref().unwrap_or(DEFAULT_QUERY);
        let query = state_manager
            .interpolate(query_template)
            .context("Failed to interpolate rag node query")?;

        let rag = ctx
            .agent
            .as_ref()
            .and_then(|a| a.graph_rag(node_id))
            .ok_or_else(|| anyhow!("rag node '{node_id}' has no initialized knowledge base"))?;

        let top_k = node.top_k.unwrap_or_else(|| rag.configured_top_k());
        let rerank = rag.configured_reranker();

        eprintln!(
            "{}",
            dimmed_text(&format!("▸   rag lookup: node={node_id} top_k={top_k}"))
        );

        let timeout_dur = Duration::from_secs(node.timeout.unwrap_or(DEFAULT_RAG_TIMEOUT_SECS));
        let abort = create_abort_signal();
        let (context, sources_str, _ids) =
            timeout(timeout_dur, rag.search(&query, top_k, rerank, abort))
                .await
                .with_context(|| {
                    format!(
                        "rag node '{node_id}' timed out after {}s",
                        timeout_dur.as_secs()
                    )
                })?
                .with_context(|| format!("rag node '{node_id}' retrieval failed"))?;

        let output = build_rag_output(context, &sources_str);
        apply_state_updates(node, state_manager, &output);

        node_next
            .map(String::from)
            .ok_or_else(|| anyhow!("rag node '{node_id}' has no `next` set"))
    }
}

/// Assemble the `{{output}}` value as `{ "context": <ctx>, "sources": [...] }`.
fn build_rag_output(context: String, sources_str: &str) -> Value {
    let sources: Vec<Value> = sources_str
        .lines()
        .map(|line| line.trim().trim_start_matches("- ").trim())
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_string()))
        .collect();
    let mut obj = Map::new();

    obj.insert("context".into(), Value::String(context));
    obj.insert("sources".into(), Value::Array(sources));

    Value::Object(obj)
}

fn apply_state_updates(node: &RagNode, state_manager: &mut StateManager, output: &Value) {
    let Some(updates) = &node.state_updates else {
        return;
    };
    let prev_output = state_manager.state().get(OUTPUT_KEY).cloned();
    state_manager
        .state_mut()
        .set(OUTPUT_KEY.into(), output.clone());

    for (key, template) in updates {
        let value = state_manager.interpolate_lenient(template);
        state_manager
            .state_mut()
            .set(key.clone(), Value::String(value));
    }

    match prev_output {
        Some(v) => state_manager.state_mut().set(OUTPUT_KEY.into(), v),
        None => state_manager
            .state_mut()
            .set(OUTPUT_KEY.into(), Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_rag_output_splits_bullet_sources_into_array() {
        let out = build_rag_output("ctx".into(), "- a.md\n- https://x.com/spec");

        assert_eq!(out["context"], json!("ctx"));
        assert_eq!(out["sources"], json!(["a.md", "https://x.com/spec"]));
    }

    #[test]
    fn build_rag_output_handles_empty_sources() {
        let out = build_rag_output("ctx".into(), "");

        assert_eq!(out["sources"], json!([]));
    }

    #[test]
    fn build_rag_output_ignores_blank_lines() {
        let out = build_rag_output("c".into(), "- a\n\n- b\n");

        assert_eq!(out["sources"], json!(["a", "b"]));
    }

    #[test]
    fn build_rag_output_tolerates_unprefixed_lines() {
        let out = build_rag_output("c".into(), "plain/path");

        assert_eq!(out["sources"], json!(["plain/path"]));
    }
}
