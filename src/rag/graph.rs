use super::DocumentId;
use crate::client::*;

use anyhow::{Context, Result};
use indexmap::IndexMap;
use petgraph::Direction;
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const EXTRACTION_PROMPT: &str = r#"Extract entities and relationships from the following text chunk.

Return a JSON object with this exact structure:
{
  "entities": [
    {"name": "EntityName", "type": "EntityType", "description": "brief description"}
  ],
  "relationships": [
    {"from": "EntityA", "to": "EntityB", "type": "relation_verb", "weight": 0.9}
  ]
}

Rules:
- Entity types: PERSON, ORGANIZATION, CONCEPT, TECHNOLOGY, LOCATION, EVENT, or OTHER
- Relationship types should be short verb phrases (e.g., "uses", "depends_on", "implements", "part_of")
- Weight is a float from 0.0 to 1.0 indicating relationship strength (default 1.0)
- Only extract entities and relationships clearly stated or strongly implied in the text
- Use exact entity names as they appear so relationships can be matched
- Return ONLY the JSON object, no markdown fences, no explanation

Text chunk:
__CHUNK__"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    pub name: String,
    pub entity_type: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub relation_type: String,
    pub weight: f32,
}

#[derive(Debug, Deserialize)]
pub struct ExtractionResult {
    pub entities: Vec<ExtractedEntity>,
    pub relationships: Vec<ExtractedRelationship>,
}

#[derive(Debug, Deserialize)]
pub struct ExtractedEntity {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExtractedRelationship {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub relation_type: String,
    pub weight: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeGraph {
    pub graph: StableGraph<Entity, Relationship>,
    /// Lowercased entity name → raw node index
    pub entity_index: IndexMap<String, u32>,
    /// DocumentId inner value → raw node indices for entities in that chunk
    pub document_entities: IndexMap<usize, Vec<u32>>,
}

impl Default for KnowledgeGraph {
    fn default() -> Self {
        Self {
            graph: StableGraph::new(),
            entity_index: IndexMap::new(),
            document_entities: IndexMap::new(),
        }
    }
}

impl KnowledgeGraph {
    pub fn merge(&mut self, doc_id: DocumentId, result: ExtractionResult) {
        let mut chunk_nodes: Vec<u32> = vec![];

        for extracted in &result.entities {
            let key = extracted.name.to_lowercase();
            let node_raw = if let Some(&existing) = self.entity_index.get(&key) {
                existing
            } else {
                let entity = Entity {
                    name: extracted.name.clone(),
                    entity_type: extracted.entity_type.clone(),
                    description: extracted.description.clone(),
                };
                let idx = self.graph.add_node(entity);
                let raw = idx.index() as u32;
                self.entity_index.insert(key, raw);
                raw
            };
            chunk_nodes.push(node_raw);
        }

        for extracted in &result.relationships {
            let from_key = extracted.from.to_lowercase();
            let to_key = extracted.to.to_lowercase();
            if let (Some(&from_raw), Some(&to_raw)) = (
                self.entity_index.get(&from_key),
                self.entity_index.get(&to_key),
            ) {
                let from_idx = NodeIndex::new(from_raw as usize);
                let to_idx = NodeIndex::new(to_raw as usize);
                // Avoid duplicate edges
                if !self.graph.contains_edge(from_idx, to_idx) {
                    let rel = Relationship {
                        relation_type: extracted.relation_type.clone(),
                        weight: extracted.weight.unwrap_or(1.0),
                    };
                    self.graph.add_edge(from_idx, to_idx, rel);
                }
            }
        }

        self.document_entities
            .entry(doc_id.0)
            .or_default()
            .extend(chunk_nodes);
    }

    pub fn remove_documents(&mut self, doc_ids: &[DocumentId]) {
        if doc_ids.is_empty() {
            return;
        }

        let removing: HashSet<usize> = doc_ids.iter().map(|d| d.0).collect();
        for raw_id in &removing {
            self.document_entities.swap_remove(raw_id);
        }

        let still_used: HashSet<u32> = self
            .document_entities
            .values()
            .flat_map(|v| v.iter().copied())
            .collect();

        let to_remove: Vec<u32> = self
            .entity_index
            .values()
            .copied()
            .filter(|raw| !still_used.contains(raw))
            .collect();

        for raw in to_remove {
            let idx = NodeIndex::new(raw as usize);
            if self.graph.contains_node(idx) {
                let name = self.graph[idx].name.to_lowercase();
                self.graph.remove_node(idx);
                self.entity_index.swap_remove(&name);
            }
        }
    }

    pub fn build_node_to_docs(&self) -> IndexMap<u32, Vec<DocumentId>> {
        let mut map: IndexMap<u32, Vec<DocumentId>> = IndexMap::new();
        for (&doc_raw, node_raws) in &self.document_entities {
            let doc_id = DocumentId(doc_raw);
            for &node_raw in node_raws {
                map.entry(node_raw).or_default().push(doc_id);
            }
        }
        map
    }

    pub fn expand_neighbors(&self, seed_nodes: &[u32], hops: usize) -> Vec<u32> {
        let mut expanded: indexmap::IndexSet<u32> = seed_nodes.iter().copied().collect();
        let mut frontier: Vec<u32> = seed_nodes.to_vec();
        for _ in 0..hops {
            let mut next_frontier: Vec<u32> = vec![];
            for &raw in &frontier {
                let idx = NodeIndex::new(raw as usize);
                if self.graph.contains_node(idx) {
                    for dir in [Direction::Outgoing, Direction::Incoming] {
                        for neighbor in self.graph.neighbors_directed(idx, dir) {
                            let n = neighbor.index() as u32;
                            if expanded.insert(n) {
                                next_frontier.push(n);
                            }
                        }
                    }
                }
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }
        expanded.into_iter().collect()
    }
}

/// Uses chat_completions_inner directly (bypassing Input) because Rag has no
/// RequestContext, which Input::from_str requires.
pub async fn extract_entities(
    client: &dyn Client,
    chunk: &str,
    prompt_template: Option<&str>,
) -> Result<ExtractionResult> {
    let template = prompt_template.unwrap_or(EXTRACTION_PROMPT);
    let prompt = template.replace("__CHUNK__", chunk);
    let mut messages = vec![Message::new(
        MessageRole::User,
        MessageContent::Text(prompt),
    )];
    patch_messages(&mut messages, client.model());
    let reqwest_client = client
        .build_client()
        .context("Failed to build HTTP client for entity extraction")?;
    let data = ChatCompletionsData {
        messages,
        temperature: Some(0.0),
        top_p: None,
        functions: None,
        stream: false,
    };
    let output = client
        .chat_completions_inner(&reqwest_client, data)
        .await
        .context("Entity extraction LLM call failed")?;

    let text = output.text.trim();
    // Strip markdown code fences if the model wraps in ```json ... ```
    let json: String = if text.starts_with("```") {
        text.lines()
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };

    serde_json::from_str::<ExtractionResult>(&json)
        .context("Failed to parse entity extraction JSON")
}
