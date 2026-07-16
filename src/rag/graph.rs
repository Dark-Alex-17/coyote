use super::DocumentId;
use crate::client::*;

use anyhow::{Context, Result};
use indexmap::{IndexMap, IndexSet};
use petgraph::Direction;
use petgraph::graph::NodeIndex;
use petgraph::stable_graph::StableGraph;
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Heuristic upper bound on chunk size before warning the user that the
/// extraction LLM call may be truncated. Not a hard limit.
const MAX_CHUNK_CHARS: usize = 24_000;

/// Maximum number of nodes the BFS may visit during a single graph_search.
/// Keeps the synchronous traversal bounded on dense graphs.
pub const MAX_GRAPH_NODES: usize = 500;

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
        let mut chunk_nodes: IndexSet<u32> = IndexSet::new();

        for extracted in &result.entities {
            let key = extracted.name.to_lowercase();
            let normalized_type = extracted.entity_type.to_uppercase();
            let node_raw = if let Some(&existing) = self.entity_index.get(&key) {
                let idx = NodeIndex::new(existing as usize);
                if self.graph.contains_node(idx) {
                    let node = &mut self.graph[idx];
                    if node.entity_type == "OTHER" && normalized_type != "OTHER" {
                        node.entity_type = normalized_type;
                    }
                    if node.description.is_none() {
                        node.description = extracted.description.clone();
                    }
                }
                existing
            } else {
                let entity = Entity {
                    name: extracted.name.clone(),
                    entity_type: normalized_type,
                    description: extracted.description.clone(),
                };
                let idx = self.graph.add_node(entity);
                let raw = idx.index() as u32;
                self.entity_index.insert(key, raw);
                raw
            };
            chunk_nodes.insert(node_raw);
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
                let already_exists = self
                    .graph
                    .edges_connecting(from_idx, to_idx)
                    .any(|e| e.weight().relation_type == extracted.relation_type);
                if !already_exists {
                    let rel = Relationship {
                        relation_type: extracted.relation_type.clone(),
                        weight: extracted.weight.unwrap_or(1.0).clamp(0.0, 1.0),
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

        if to_remove.is_empty() {
            return;
        }

        for raw in to_remove {
            let idx = NodeIndex::new(raw as usize);
            if self.graph.contains_node(idx) {
                let name = self.graph[idx].name.to_lowercase();
                self.graph.remove_node(idx);
                self.entity_index.swap_remove(&name);
            }
        }

        self.compact();
    }

    /// Rebuild the internal graph with consecutive node indices. Eliminates
    /// the null tombstone slots that petgraph's StableGraph accumulates after
    /// repeated `remove_node` calls, keeping serialized YAML size in check.
    fn compact(&mut self) {
        let mut new_graph: StableGraph<Entity, Relationship> = StableGraph::new();
        let mut old_to_new: HashMap<u32, u32> = HashMap::new();

        for &old_raw in self.entity_index.values() {
            let old_idx = NodeIndex::new(old_raw as usize);
            if self.graph.contains_node(old_idx) {
                let entity = self.graph[old_idx].clone();
                let new_idx = new_graph.add_node(entity);
                old_to_new.insert(old_raw, new_idx.index() as u32);
            }
        }

        for edge_idx in self.graph.edge_indices() {
            if let Some((from, to)) = self.graph.edge_endpoints(edge_idx) {
                let from_raw = from.index() as u32;
                let to_raw = to.index() as u32;
                if let (Some(&new_from), Some(&new_to)) =
                    (old_to_new.get(&from_raw), old_to_new.get(&to_raw))
                {
                    let rel = self.graph[edge_idx].clone();
                    new_graph.add_edge(
                        NodeIndex::new(new_from as usize),
                        NodeIndex::new(new_to as usize),
                        rel,
                    );
                }
            }
        }

        for raw in self.entity_index.values_mut() {
            if let Some(&new_raw) = old_to_new.get(raw) {
                *raw = new_raw;
            }
        }

        for node_raws in self.document_entities.values_mut() {
            *node_raws = node_raws
                .iter()
                .filter_map(|raw| old_to_new.get(raw).copied())
                .collect();
        }

        self.graph = new_graph;
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

    /// BFS from seed nodes with weight-decayed scoring.
    ///
    /// Seed node scores are provided by the caller (typically token-overlap
    /// ratios). Each neighbor's score is `edge_weight * parent_score`, so
    /// strongly-connected neighbors rank higher and weakly-connected ones
    /// naturally contribute less. Traversal is capped at `MAX_GRAPH_NODES`
    /// total nodes; the highest-scored frontier nodes are expanded first so
    /// the budget is spent on the most relevant entities.
    ///
    /// Returns a map of raw node index → score (includes seed nodes).
    pub fn expand_neighbors_scored(
        &self,
        seed_scores: &[(u32, f32)],
        hops: usize,
    ) -> IndexMap<u32, f32> {
        let mut node_scores: IndexMap<u32, f32> = IndexMap::new();
        for &(raw, score) in seed_scores {
            node_scores.insert(raw, score);
        }

        let mut frontier: Vec<(u32, f32)> = seed_scores.to_vec();

        for _ in 0..hops {
            if node_scores.len() >= MAX_GRAPH_NODES {
                break;
            }

            frontier.sort_unstable_by(|a, b| {
                b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut next_frontier: Vec<(u32, f32)> = vec![];

            'nodes: for (raw, parent_score) in &frontier {
                let idx = NodeIndex::new(*raw as usize);
                if !self.graph.contains_node(idx) {
                    continue;
                }
                for dir in [Direction::Outgoing, Direction::Incoming] {
                    for edge_ref in self.graph.edges_directed(idx, dir) {
                        let neighbor_idx = match dir {
                            Direction::Outgoing => edge_ref.target(),
                            Direction::Incoming => edge_ref.source(),
                        };
                        let neighbor_raw = neighbor_idx.index() as u32;
                        let candidate = edge_ref.weight().weight * parent_score;

                        match node_scores.entry(neighbor_raw) {
                            indexmap::map::Entry::Vacant(e) => {
                                e.insert(candidate);
                                next_frontier.push((neighbor_raw, candidate));
                            }
                            indexmap::map::Entry::Occupied(mut e) => {
                                if candidate > *e.get() {
                                    *e.get_mut() = candidate;
                                }
                            }
                        }

                        if node_scores.len() >= MAX_GRAPH_NODES {
                            break 'nodes;
                        }
                    }
                }
            }

            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }

        node_scores
    }
}

/// Uses chat_completions_inner directly (bypassing Input) because Rag has no
/// RequestContext, which Input::from_str requires.
pub async fn extract_entities(
    client: &dyn Client,
    chunk: &str,
    prompt_template: Option<&str>,
) -> Result<ExtractionResult> {
    if chunk.len() > MAX_CHUNK_CHARS {
        warn!(
            "Entity extraction chunk is {} chars (heuristic limit: {}); \
             the LLM response may be truncated",
            chunk.len(),
            MAX_CHUNK_CHARS
        );
    }
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
        reasoning_effort: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(name: &str, entity_type: &str) -> ExtractedEntity {
        ExtractedEntity {
            name: name.to_string(),
            entity_type: entity_type.to_string(),
            description: None,
        }
    }

    fn rel(from: &str, to: &str, rel_type: &str, weight: f32) -> ExtractedRelationship {
        ExtractedRelationship {
            from: from.to_string(),
            to: to.to_string(),
            relation_type: rel_type.to_string(),
            weight: Some(weight),
        }
    }

    fn doc(id: usize) -> DocumentId {
        DocumentId(id)
    }

    fn extraction(
        entities: Vec<ExtractedEntity>,
        rels: Vec<ExtractedRelationship>,
    ) -> ExtractionResult {
        ExtractionResult {
            entities,
            relationships: rels,
        }
    }

    #[test]
    fn merge_deduplicates_by_lowercase_name() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![
                    entity("Python", "TECHNOLOGY"),
                    entity("python", "TECHNOLOGY"),
                ],
                vec![],
            ),
        );
        assert_eq!(kg.entity_index.len(), 1);
        assert_eq!(kg.graph.node_count(), 1);
    }

    #[test]
    fn merge_chunk_nodes_no_duplicate_doc_entries() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(1),
            extraction(
                vec![
                    entity("Python", "TECHNOLOGY"),
                    entity("python", "TECHNOLOGY"),
                ],
                vec![],
            ),
        );
        let count = kg.document_entities.get(&1).map(|v| v.len()).unwrap_or(0);
        assert_eq!(
            count, 1,
            "duplicate entity in one chunk should produce one doc_entity entry"
        );
    }

    #[test]
    fn merge_normalizes_entity_type_to_uppercase() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(vec![entity("Django", "technology")], vec![]),
        );
        let raw = kg.entity_index["django"];
        assert_eq!(
            kg.graph[NodeIndex::new(raw as usize)].entity_type,
            "TECHNOLOGY"
        );
    }

    #[test]
    fn merge_promotes_type_from_other_to_specific() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(doc(0), extraction(vec![entity("Python", "OTHER")], vec![]));
        kg.merge(
            doc(1),
            extraction(vec![entity("Python", "TECHNOLOGY")], vec![]),
        );
        let raw = kg.entity_index["python"];
        assert_eq!(
            kg.graph[NodeIndex::new(raw as usize)].entity_type,
            "TECHNOLOGY"
        );
    }

    #[test]
    fn merge_does_not_demote_specific_type_to_other() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(vec![entity("Python", "TECHNOLOGY")], vec![]),
        );
        kg.merge(doc(1), extraction(vec![entity("Python", "OTHER")], vec![]));
        let raw = kg.entity_index["python"];
        assert_eq!(
            kg.graph[NodeIndex::new(raw as usize)].entity_type,
            "TECHNOLOGY"
        );
    }

    #[test]
    fn merge_allows_multiple_relation_types_between_same_pair() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![
                    entity("Python", "TECHNOLOGY"),
                    entity("Django", "TECHNOLOGY"),
                ],
                vec![rel("Python", "Django", "implements", 0.9)],
            ),
        );
        kg.merge(
            doc(1),
            extraction(
                vec![
                    entity("Python", "TECHNOLOGY"),
                    entity("Django", "TECHNOLOGY"),
                ],
                vec![rel("Python", "Django", "uses", 0.8)],
            ),
        );
        let from_idx = NodeIndex::new(kg.entity_index["python"] as usize);
        let to_idx = NodeIndex::new(kg.entity_index["django"] as usize);
        let count = kg.graph.edges_connecting(from_idx, to_idx).count();
        assert_eq!(
            count, 2,
            "two different relation types should produce two edges"
        );
    }

    #[test]
    fn merge_deduplicates_same_relation_type() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", 1.0)],
            ),
        );
        kg.merge(
            doc(1),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", 0.5)],
            ),
        );
        let from_idx = NodeIndex::new(kg.entity_index["a"] as usize);
        let to_idx = NodeIndex::new(kg.entity_index["b"] as usize);
        let count = kg.graph.edges_connecting(from_idx, to_idx).count();
        assert_eq!(
            count, 1,
            "same relation type should not create a duplicate edge"
        );
    }

    #[test]
    fn remove_documents_preserves_entity_shared_across_docs() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![entity("Python", "TECHNOLOGY"), entity("A", "CONCEPT")],
                vec![],
            ),
        );
        kg.merge(
            doc(1),
            extraction(
                vec![entity("Python", "TECHNOLOGY"), entity("B", "CONCEPT")],
                vec![],
            ),
        );
        kg.remove_documents(&[doc(0)]);
        assert!(
            kg.entity_index.contains_key("python"),
            "shared entity should survive"
        );
        assert!(
            !kg.entity_index.contains_key("a"),
            "exclusive entity should be removed"
        );
        assert!(
            kg.entity_index.contains_key("b"),
            "other doc's entity should survive"
        );
    }

    #[test]
    fn remove_documents_noop_on_empty_slice() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(doc(0), extraction(vec![entity("X", "CONCEPT")], vec![]));
        kg.remove_documents(&[]);
        assert_eq!(kg.entity_index.len(), 1);
    }

    #[test]
    fn remove_documents_compacts_graph() {
        let mut kg = KnowledgeGraph::default();
        // doc 0: A, B with an edge
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", 1.0)],
            ),
        );
        // doc 1: C only
        kg.merge(doc(1), extraction(vec![entity("C", "CONCEPT")], vec![]));

        kg.remove_documents(&[doc(0)]);

        assert_eq!(kg.graph.node_count(), 1);
        let c_raw = kg.entity_index["c"];
        assert_eq!(
            c_raw, 0,
            "compacted graph should give surviving node index 0"
        );
        let refs = kg.document_entities.get(&1).cloned().unwrap_or_default();
        assert_eq!(refs, vec![0u32]);
    }

    #[test]
    fn expand_zero_hops_returns_seeds_only() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", 0.9)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let result = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[&a_raw], 1.0);
    }

    #[test]
    fn expand_one_hop_decays_score_by_edge_weight() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", 0.8)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let b_raw = kg.entity_index["b"];
        let result = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 1);
        assert_eq!(result.len(), 2);
        assert_eq!(result[&a_raw], 1.0);
        let b_score = result[&b_raw];
        assert!(
            (b_score - 0.8).abs() < 1e-6,
            "neighbor score should be edge_weight * parent_score = 0.8, got {b_score}"
        );
    }

    #[test]
    fn expand_incoming_edges_also_traversed() {
        let mut kg = KnowledgeGraph::default();
        // Edge goes B → A; seeding A should still discover B via incoming edge
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("B", "A", "uses", 0.7)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let b_raw = kg.entity_index["b"];
        let result = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 1);
        assert!(
            result.contains_key(&b_raw),
            "B should be reachable via incoming edge from A"
        );
        let b_score = result[&b_raw];
        assert!((b_score - 0.7).abs() < 1e-6);
    }

    #[test]
    fn expand_picks_best_path_score() {
        let mut kg = KnowledgeGraph::default();
        // A(0.5) → C(0.9): score 0.45; B(1.0) → C(0.4): score 0.40 — A→C path wins.
        kg.merge(
            doc(0),
            extraction(
                vec![
                    entity("A", "CONCEPT"),
                    entity("B", "CONCEPT"),
                    entity("C", "CONCEPT"),
                ],
                vec![rel("A", "C", "uses", 0.9), rel("B", "C", "uses", 0.4)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let b_raw = kg.entity_index["b"];
        let c_raw = kg.entity_index["c"];
        let seeds = vec![(a_raw, 0.5f32), (b_raw, 1.0f32)];
        let result = kg.expand_neighbors_scored(&seeds, 1);
        let c_score = result[&c_raw];
        // Best path: B(1.0) * 0.4 = 0.4, A(0.5) * 0.9 = 0.45 → should be 0.45
        assert!(
            (c_score - 0.45).abs() < 1e-6,
            "C score should reflect best path (0.45), got {c_score}"
        );
    }

    #[test]
    fn build_node_to_docs_maps_shared_entity_to_multiple_docs() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(vec![entity("Python", "TECHNOLOGY")], vec![]),
        );
        kg.merge(
            doc(1),
            extraction(vec![entity("Python", "TECHNOLOGY")], vec![]),
        );
        let n2d = kg.build_node_to_docs();
        let raw = kg.entity_index["python"];
        let docs = &n2d[&raw];
        assert!(docs.contains(&DocumentId(0)));
        assert!(docs.contains(&DocumentId(1)));
    }

    #[test]
    fn compact_preserves_edges_between_survivors() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(doc(0), extraction(vec![entity("A", "CONCEPT")], vec![]));
        kg.merge(
            doc(1),
            extraction(
                vec![entity("B", "CONCEPT"), entity("C", "CONCEPT")],
                vec![rel("B", "C", "linked", 0.8)],
            ),
        );
        kg.remove_documents(&[doc(0)]);
        let b_raw = kg.entity_index["b"];
        let c_raw = kg.entity_index["c"];
        let b_idx = NodeIndex::new(b_raw as usize);
        let c_idx = NodeIndex::new(c_raw as usize);
        assert_eq!(
            kg.graph.edges_connecting(b_idx, c_idx).count(),
            1,
            "B→C edge should survive compaction"
        );
    }

    #[test]
    fn expand_two_hops_reaches_transitive_neighbor() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![
                    entity("A", "CONCEPT"),
                    entity("B", "CONCEPT"),
                    entity("C", "CONCEPT"),
                ],
                vec![rel("A", "B", "uses", 1.0), rel("B", "C", "uses", 0.5)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let c_raw = kg.entity_index["c"];

        let one_hop = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 1);
        assert!(
            !one_hop.contains_key(&c_raw),
            "C should not be reachable at 1 hop"
        );

        let two_hop = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 2);
        assert!(
            two_hop.contains_key(&c_raw),
            "C should be reachable at 2 hops"
        );
        let c_score = two_hop[&c_raw];
        assert!(
            (c_score - 0.5).abs() < 1e-6,
            "C score should be 1.0 * 1.0 * 0.5 = 0.5, got {c_score}"
        );
    }

    #[test]
    fn merge_clamps_edge_weight_above_one() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", 1.5)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let b_raw = kg.entity_index["b"];
        let result = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 1);
        let b_score = result[&b_raw];
        assert!(
            (b_score - 1.0).abs() < 1e-6,
            "weight 1.5 clamped to 1.0: b_score should be 1.0, got {b_score}"
        );
    }

    #[test]
    fn merge_clamps_edge_weight_below_zero() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(
                vec![entity("A", "CONCEPT"), entity("B", "CONCEPT")],
                vec![rel("A", "B", "uses", -0.5)],
            ),
        );
        let a_raw = kg.entity_index["a"];
        let b_raw = kg.entity_index["b"];
        let result = kg.expand_neighbors_scored(&[(a_raw, 1.0)], 1);
        let b_score = result.get(&b_raw).copied().unwrap_or(0.0);
        assert!(
            b_score.abs() < 1e-6,
            "weight -0.5 clamped to 0.0: b_score should be 0.0, got {b_score}"
        );
    }

    #[test]
    fn merge_fills_missing_description_from_later_chunk() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(
            doc(0),
            extraction(vec![entity("Python", "TECHNOLOGY")], vec![]),
        );
        kg.merge(
            doc(1),
            ExtractionResult {
                entities: vec![ExtractedEntity {
                    name: "python".to_string(),
                    entity_type: "TECHNOLOGY".to_string(),
                    description: Some("A general-purpose language".to_string()),
                }],
                relationships: vec![],
            },
        );
        let raw = kg.entity_index["python"];
        let desc = &kg.graph[NodeIndex::new(raw as usize)].description;
        assert_eq!(
            desc.as_deref(),
            Some("A general-purpose language"),
            "description should be backfilled from later chunk"
        );
    }

    #[test]
    fn remove_all_documents_empties_graph() {
        let mut kg = KnowledgeGraph::default();
        kg.merge(doc(0), extraction(vec![entity("A", "CONCEPT")], vec![]));
        kg.merge(doc(1), extraction(vec![entity("B", "CONCEPT")], vec![]));
        kg.remove_documents(&[doc(0), doc(1)]);
        assert_eq!(kg.graph.node_count(), 0, "all nodes should be removed");
        assert_eq!(kg.entity_index.len(), 0, "entity index should be empty");
        assert!(
            kg.document_entities.is_empty(),
            "document_entities should be empty"
        );
    }
}
