//! Query plan schema and executor.
//!
//! An LLM generates a structured query plan (JSON) that the executor
//! runs against the knowledge graph. Each step produces bindings that
//! subsequent steps can reference via `$variable` syntax.
//!
//! This is the key cost optimization: the LLM emits ~100 tokens of
//! query plan, the executor runs it for free against redb, and only
//! the compact results (~300-500 tokens) go back to the LLM.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::repository::Repository;
use crate::RepositoryError;
use atomic_core::pristine::vault::{KgEdge, KgNode};

// ── Query Plan Schema ───────────────────────────────────────────

/// A query plan is a sequence of steps that the executor runs in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPlan {
    /// Ordered list of steps to execute.
    pub steps: Vec<QueryStep>,
}

/// A single step in the query plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QueryStep {
    /// Full-text keyword search over KG nodes.
    KgSearch {
        /// Search query text.
        query: String,
        /// Max results.
        #[serde(default = "default_limit")]
        limit: usize,
        /// Variable name to bind results to (for use in later steps).
        #[serde(default)]
        bind: Option<String>,
    },

    /// Get the neighborhood of a node (outgoing + incoming edges).
    KgNeighbors {
        /// Node ID to explore. Can be a `$variable` reference.
        node_id: String,
        /// Traversal depth (1 or 2).
        #[serde(default = "default_depth")]
        depth: u8,
        /// Variable name to bind results to.
        #[serde(default)]
        bind: Option<String>,
    },

    /// Vector similarity search.
    VectorSearch {
        /// Query text to embed and search for.
        query: String,
        /// Max results.
        #[serde(default = "default_top_k")]
        top_k: usize,
        /// Variable name to bind results to.
        #[serde(default)]
        bind: Option<String>,
    },

    /// Read the content of vault entries.
    ReadContent {
        /// Vault paths or `$variable` reference to node IDs.
        sources: Vec<String>,
        /// Maximum total characters to return.
        #[serde(default = "default_max_chars")]
        max_chars: usize,
        /// Variable name to bind results to.
        #[serde(default)]
        bind: Option<String>,
    },

    /// Filter nodes from a previous step by metadata field.
    Filter {
        /// Variable reference to the input nodes (e.g., `$search_results`).
        input: String,
        /// Field to filter on (checks node metadata, kind, or summary).
        field: String,
        /// Required value (exact match).
        #[serde(default)]
        equals: Option<String>,
        /// Excluded value.
        #[serde(default)]
        not_equals: Option<String>,
        /// Variable name to bind filtered results to.
        #[serde(default)]
        bind: Option<String>,
    },
}

fn default_limit() -> usize {
    10
}
fn default_depth() -> u8 {
    1
}
fn default_top_k() -> usize {
    5
}
fn default_max_chars() -> usize {
    2000
}

// ── Execution Result ────────────────────────────────────────────

/// Result of executing a query plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    /// Nodes collected across all steps.
    pub nodes: Vec<KgNode>,
    /// Edges collected across all steps.
    pub edges: Vec<KgEdge>,
    /// Content read from vault entries (path → content).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub content: HashMap<String, String>,
    /// Per-step statistics.
    pub step_stats: Vec<StepStat>,
    /// Total execution time in milliseconds.
    pub elapsed_ms: u64,
}

/// Statistics for one step of the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepStat {
    /// Step type name.
    pub step_type: String,
    /// Number of results produced.
    pub result_count: usize,
    /// Time in milliseconds.
    pub elapsed_ms: u64,
}

// ── Executor ────────────────────────────────────────────────────

/// Execute a query plan against a repository.
pub fn execute_plan(repo: &Repository, plan: &QueryPlan) -> Result<PlanResult, RepositoryError> {
    let overall_start = std::time::Instant::now();

    let mut bindings: HashMap<String, Vec<KgNode>> = HashMap::new();
    let mut all_nodes: Vec<KgNode> = Vec::new();
    let mut all_edges: Vec<KgEdge> = Vec::new();
    let mut content: HashMap<String, String> = HashMap::new();
    let mut step_stats: Vec<StepStat> = Vec::new();

    for step in &plan.steps {
        let step_start = std::time::Instant::now();

        match step {
            QueryStep::KgSearch { query, limit, bind } => {
                let nodes = repo.vault_kg_search(query, *limit)?;
                let count = nodes.len();

                if let Some(var) = bind {
                    bindings.insert(var.clone(), nodes.clone());
                }
                all_nodes.extend(nodes);

                step_stats.push(StepStat {
                    step_type: "kg_search".to_string(),
                    result_count: count,
                    elapsed_ms: step_start.elapsed().as_millis() as u64,
                });
            }

            QueryStep::KgNeighbors {
                node_id,
                depth,
                bind,
            } => {
                // Resolve $variable references
                let ids = resolve_node_ids(node_id, &bindings);

                let mut step_nodes = Vec::new();
                let mut step_edges = Vec::new();

                for id in &ids {
                    match repo.vault_kg_neighbors(id, *depth) {
                        Ok(sg) => {
                            step_nodes.extend(sg.nodes);
                            step_edges.extend(sg.edges);
                        }
                        Err(e) => {
                            log::warn!("KG neighbors failed for {}: {}", id, e);
                        }
                    }
                }

                let count = step_nodes.len();
                if let Some(var) = bind {
                    bindings.insert(var.clone(), step_nodes.clone());
                }
                all_nodes.extend(step_nodes);
                all_edges.extend(step_edges);

                step_stats.push(StepStat {
                    step_type: "kg_neighbors".to_string(),
                    result_count: count,
                    elapsed_ms: step_start.elapsed().as_millis() as u64,
                });
            }

            QueryStep::VectorSearch { query, top_k, bind } => {
                // Use the resolved embedding provider
                let provider = crate::ai::resolve_embedding_provider();
                let query_vec = provider
                    .embed_sync(&[query.clone()])
                    .ok()
                    .and_then(|v| v.into_iter().next())
                    .unwrap_or_else(|| crate::hash_embed(query, provider.dimensions));

                let results = repo.vault_search(&query_vec, *top_k)?;

                // Convert search results to KgNodes for binding
                let nodes: Vec<KgNode> = results
                    .iter()
                    .map(|r| {
                        KgNode::new(
                            format!("embedding:{}:{}", r.path, r.chunk_idx),
                            "embedding",
                            &r.path,
                            "vector_search",
                        )
                        .with_summary(&r.preview)
                        .with_metadata(serde_json::json!({
                            "score": r.score,
                            "chunk_idx": r.chunk_idx,
                            "path": r.path,
                        }))
                    })
                    .collect();

                let count = nodes.len();
                if let Some(var) = bind {
                    bindings.insert(var.clone(), nodes.clone());
                }
                all_nodes.extend(nodes);

                step_stats.push(StepStat {
                    step_type: "vector_search".to_string(),
                    result_count: count,
                    elapsed_ms: step_start.elapsed().as_millis() as u64,
                });
            }

            QueryStep::ReadContent {
                sources,
                max_chars,
                bind,
            } => {
                let mut total_chars = 0usize;
                let mut read_nodes = Vec::new();

                for source in sources {
                    if total_chars >= *max_chars {
                        break;
                    }

                    // Resolve source — either a direct path or $variable
                    let paths = resolve_paths(source, &bindings);

                    for path in &paths {
                        if total_chars >= *max_chars {
                            break;
                        }

                        match repo.vault_retrieve(path) {
                            Ok(Some(entry)) => {
                                let text = String::from_utf8_lossy(&entry.content_bytes);
                                let remaining = max_chars - total_chars;
                                let truncated: String = text.chars().take(remaining).collect();
                                total_chars += truncated.len();
                                content.insert(path.clone(), truncated);

                                let node = KgNode::new(
                                    format!("content:{}", path),
                                    "content",
                                    path,
                                    "read_content",
                                );
                                read_nodes.push(node);
                            }
                            Ok(None) => {
                                log::debug!("Vault entry not found: {}", path);
                            }
                            Err(e) => {
                                log::warn!("Failed to read vault entry {}: {}", path, e);
                            }
                        }
                    }
                }

                let count = read_nodes.len();
                if let Some(var) = bind {
                    bindings.insert(var.clone(), read_nodes.clone());
                }
                all_nodes.extend(read_nodes);

                step_stats.push(StepStat {
                    step_type: "read_content".to_string(),
                    result_count: count,
                    elapsed_ms: step_start.elapsed().as_millis() as u64,
                });
            }

            QueryStep::Filter {
                input,
                field,
                equals,
                not_equals,
                bind,
            } => {
                let input_nodes = bindings.get(input).cloned().unwrap_or_default();

                let filtered: Vec<KgNode> = input_nodes
                    .into_iter()
                    .filter(|node| {
                        let value = get_node_field(node, field);
                        if let Some(ref eq) = equals {
                            if value.as_deref() != Some(eq.as_str()) {
                                return false;
                            }
                        }
                        if let Some(ref neq) = not_equals {
                            if value.as_deref() == Some(neq.as_str()) {
                                return false;
                            }
                        }
                        true
                    })
                    .collect();

                let count = filtered.len();
                if let Some(var) = bind {
                    bindings.insert(var.clone(), filtered.clone());
                }
                // Don't add filtered nodes to all_nodes (they're already there from input step)

                step_stats.push(StepStat {
                    step_type: "filter".to_string(),
                    result_count: count,
                    elapsed_ms: step_start.elapsed().as_millis() as u64,
                });
            }
        }
    }

    // Deduplicate nodes by ID
    {
        let mut seen = std::collections::HashSet::new();
        all_nodes.retain(|n| seen.insert(n.id.clone()));
    }

    // Deduplicate edges
    {
        let mut seen = std::collections::HashSet::new();
        all_edges.retain(|e| seen.insert((e.from_id.clone(), e.to_id.clone(), e.kind.clone())));
    }

    Ok(PlanResult {
        nodes: all_nodes,
        edges: all_edges,
        content,
        step_stats,
        elapsed_ms: overall_start.elapsed().as_millis() as u64,
    })
}

// ── Helpers ─────────────────────────────────────────────────────

/// Resolve a node_id that may be a `$variable` reference.
///
/// If it starts with `$`, look up the binding and extract node IDs.
/// Otherwise return it as-is.
fn resolve_node_ids(id: &str, bindings: &HashMap<String, Vec<KgNode>>) -> Vec<String> {
    if let Some(var_name) = id.strip_prefix('$') {
        bindings
            .get(var_name)
            .map(|nodes| nodes.iter().map(|n| n.id.clone()).collect())
            .unwrap_or_default()
    } else {
        vec![id.to_string()]
    }
}

/// Resolve a source path that may be a `$variable` reference.
///
/// If it starts with `$`, extract vault paths from the bound nodes
/// (using the node label or metadata path field).
fn resolve_paths(source: &str, bindings: &HashMap<String, Vec<KgNode>>) -> Vec<String> {
    if let Some(var_name) = source.strip_prefix('$') {
        bindings
            .get(var_name)
            .map(|nodes| {
                nodes
                    .iter()
                    .filter_map(|n| {
                        // Try metadata.path first (from vector search results)
                        n.metadata
                            .as_ref()
                            .and_then(|m| m.get("path"))
                            .and_then(|p| p.as_str())
                            .map(String::from)
                            .or_else(|| {
                                // Fall back to node label if it looks like a path
                                if n.label.contains('/') || n.label.contains('.') {
                                    Some(n.label.clone())
                                } else {
                                    None
                                }
                            })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![source.to_string()]
    }
}

/// Get a field value from a KgNode for filtering.
fn get_node_field(node: &KgNode, field: &str) -> Option<String> {
    match field {
        "kind" => Some(node.kind.clone()),
        "label" => Some(node.label.clone()),
        "source" => Some(node.source.clone()),
        "summary" => node.summary.clone(),
        "id" => Some(node.id.clone()),
        // Check metadata
        other => node
            .metadata
            .as_ref()
            .and_then(|m| m.get(other))
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            }),
    }
}

// ── Parse helper ────────────────────────────────────────────────

/// Parse a query plan from JSON.
pub fn parse_plan(json: &str) -> Result<QueryPlan, RepositoryError> {
    serde_json::from_str(json)
        .map_err(|e| RepositoryError::Serialization(format!("Invalid query plan: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_plan() {
        let json = r#"{
            "steps": [
                {"type": "kg_search", "query": "authentication", "limit": 5, "bind": "auth_nodes"}
            ]
        }"#;
        let plan = parse_plan(json).unwrap();
        assert_eq!(plan.steps.len(), 1);
        match &plan.steps[0] {
            QueryStep::KgSearch { query, limit, bind } => {
                assert_eq!(query, "authentication");
                assert_eq!(*limit, 5);
                assert_eq!(bind.as_deref(), Some("auth_nodes"));
            }
            _ => panic!("Expected KgSearch step"),
        }
    }

    #[test]
    fn test_parse_multi_step_plan() {
        let json = r#"{
            "steps": [
                {"type": "kg_search", "query": "deploy blocked", "bind": "blockers"},
                {"type": "filter", "input": "blockers", "field": "kind", "equals": "intent", "bind": "blocking_intents"},
                {"type": "kg_neighbors", "node_id": "$blocking_intents", "depth": 1, "bind": "context"},
                {"type": "read_content", "sources": ["$blocking_intents"], "max_chars": 1000}
            ]
        }"#;
        let plan = parse_plan(json).unwrap();
        assert_eq!(plan.steps.len(), 4);
    }

    #[test]
    fn test_parse_vector_search() {
        let json = r#"{
            "steps": [
                {"type": "vector_search", "query": "caching performance", "top_k": 3}
            ]
        }"#;
        let plan = parse_plan(json).unwrap();
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn test_parse_defaults() {
        let json = r#"{
            "steps": [
                {"type": "kg_search", "query": "test"},
                {"type": "kg_neighbors", "node_id": "intent:X"},
                {"type": "vector_search", "query": "test"},
                {"type": "read_content", "sources": ["a.md"]}
            ]
        }"#;
        let plan = parse_plan(json).unwrap();
        assert_eq!(plan.steps.len(), 4);
        match &plan.steps[0] {
            QueryStep::KgSearch { limit, bind, .. } => {
                assert_eq!(*limit, 10);
                assert!(bind.is_none());
            }
            _ => panic!("Expected KgSearch"),
        }
        match &plan.steps[1] {
            QueryStep::KgNeighbors { depth, bind, .. } => {
                assert_eq!(*depth, 1);
                assert!(bind.is_none());
            }
            _ => panic!("Expected KgNeighbors"),
        }
        match &plan.steps[2] {
            QueryStep::VectorSearch { top_k, bind, .. } => {
                assert_eq!(*top_k, 5);
                assert!(bind.is_none());
            }
            _ => panic!("Expected VectorSearch"),
        }
        match &plan.steps[3] {
            QueryStep::ReadContent {
                max_chars, bind, ..
            } => {
                assert_eq!(*max_chars, 2000);
                assert!(bind.is_none());
            }
            _ => panic!("Expected ReadContent"),
        }
    }

    #[test]
    fn test_parse_invalid_plan() {
        assert!(parse_plan("not json").is_err());
        assert!(parse_plan("{}").is_err()); // missing steps
    }

    #[test]
    fn test_parse_empty_steps_ok() {
        // An empty steps array is valid JSON for our schema (no minimum).
        let plan = parse_plan(r#"{"steps": []}"#).unwrap();
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn test_parse_unknown_step_type_errors() {
        let json = r#"{"steps": [{"type": "unknown_step", "query": "x"}]}"#;
        assert!(parse_plan(json).is_err());
    }

    #[test]
    fn test_resolve_node_ids_literal() {
        let bindings = HashMap::new();
        let ids = resolve_node_ids("intent:PIMO-1", &bindings);
        assert_eq!(ids, vec!["intent:PIMO-1"]);
    }

    #[test]
    fn test_resolve_node_ids_variable() {
        let mut bindings = HashMap::new();
        bindings.insert(
            "results".to_string(),
            vec![
                KgNode::new("intent:PIMO-1", "intent", "PIMO-1", "test"),
                KgNode::new("intent:PIMO-2", "intent", "PIMO-2", "test"),
            ],
        );
        let ids = resolve_node_ids("$results", &bindings);
        assert_eq!(ids, vec!["intent:PIMO-1", "intent:PIMO-2"]);
    }

    #[test]
    fn test_resolve_node_ids_missing_variable() {
        let bindings = HashMap::new();
        let ids = resolve_node_ids("$nonexistent", &bindings);
        assert!(ids.is_empty());
    }

    #[test]
    fn test_resolve_paths_literal() {
        let bindings = HashMap::new();
        let paths = resolve_paths("memory/auth.md", &bindings);
        assert_eq!(paths, vec!["memory/auth.md"]);
    }

    #[test]
    fn test_resolve_paths_variable_with_metadata() {
        let mut bindings = HashMap::new();
        let node = KgNode::new("embedding:x:0", "embedding", "some-label", "vector_search")
            .with_metadata(serde_json::json!({"path": "memory/auth.md"}));
        bindings.insert("results".to_string(), vec![node]);

        let paths = resolve_paths("$results", &bindings);
        assert_eq!(paths, vec!["memory/auth.md"]);
    }

    #[test]
    fn test_resolve_paths_variable_fallback_to_label() {
        let mut bindings = HashMap::new();
        let node = KgNode::new("x", "memory", "memory/arch.md", "vault");
        bindings.insert("results".to_string(), vec![node]);

        let paths = resolve_paths("$results", &bindings);
        assert_eq!(paths, vec!["memory/arch.md"]);
    }

    #[test]
    fn test_resolve_paths_variable_skips_non_path_labels() {
        let mut bindings = HashMap::new();
        let node = KgNode::new("x", "intent", "PIMO-1", "vault");
        bindings.insert("results".to_string(), vec![node]);

        let paths = resolve_paths("$results", &bindings);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_resolve_paths_missing_variable() {
        let bindings = HashMap::new();
        let paths = resolve_paths("$nonexistent", &bindings);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_get_node_field() {
        let node = KgNode::new("x", "intent", "PIMO-1", "vault")
            .with_summary("Fix auth")
            .with_metadata(serde_json::json!({"status": "in-progress", "priority": "high"}));

        assert_eq!(get_node_field(&node, "kind"), Some("intent".to_string()));
        assert_eq!(get_node_field(&node, "label"), Some("PIMO-1".to_string()));
        assert_eq!(
            get_node_field(&node, "summary"),
            Some("Fix auth".to_string())
        );
        assert_eq!(get_node_field(&node, "id"), Some("x".to_string()));
        assert_eq!(get_node_field(&node, "source"), Some("vault".to_string()));
        assert_eq!(
            get_node_field(&node, "status"),
            Some("in-progress".to_string())
        );
        assert_eq!(get_node_field(&node, "priority"), Some("high".to_string()));
        assert_eq!(get_node_field(&node, "nonexistent"), None);
    }

    #[test]
    fn test_get_node_field_metadata_types() {
        let node = KgNode::new("x", "test", "test", "test").with_metadata(serde_json::json!({
            "count": 42,
            "active": true,
            "tags": ["a", "b"],
        }));

        assert_eq!(get_node_field(&node, "count"), Some("42".to_string()));
        assert_eq!(get_node_field(&node, "active"), Some("true".to_string()));
        // Arrays/objects return None
        assert_eq!(get_node_field(&node, "tags"), None);
    }

    #[test]
    fn test_get_node_field_no_metadata() {
        let node = KgNode::new("x", "test", "test", "test");
        assert_eq!(get_node_field(&node, "unknown"), None);
    }

    #[test]
    fn test_get_node_field_no_summary() {
        let node = KgNode::new("x", "test", "test", "test");
        assert_eq!(get_node_field(&node, "summary"), None);
    }

    #[test]
    fn test_filter_nodes() {
        let nodes = vec![
            KgNode::new("a", "intent", "A", "test")
                .with_metadata(serde_json::json!({"status": "done"})),
            KgNode::new("b", "intent", "B", "test")
                .with_metadata(serde_json::json!({"status": "in-progress"})),
            KgNode::new("c", "change", "C", "test"),
        ];

        // Filter: kind == "intent"
        let filtered: Vec<&KgNode> = nodes
            .iter()
            .filter(|n| get_node_field(n, "kind").as_deref() == Some("intent"))
            .collect();
        assert_eq!(filtered.len(), 2);

        // Filter: status != "done"
        let filtered: Vec<&KgNode> = nodes
            .iter()
            .filter(|n| get_node_field(n, "status").as_deref() != Some("done"))
            .collect();
        assert_eq!(filtered.len(), 2); // B (in-progress) and C (no status)
    }

    #[test]
    fn test_plan_result_serialization() {
        let result = PlanResult {
            nodes: vec![KgNode::new("a", "intent", "A", "test")],
            edges: vec![KgEdge::new("a", "b", "DEPENDS_ON")],
            content: HashMap::new(),
            step_stats: vec![StepStat {
                step_type: "kg_search".to_string(),
                result_count: 1,
                elapsed_ms: 5,
            }],
            elapsed_ms: 10,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("kg_search"));
        assert!(json.contains("intent"));
        // Empty content map should be skipped
        assert!(!json.contains("content"));
    }

    #[test]
    fn test_plan_result_with_content_serialization() {
        let mut content = HashMap::new();
        content.insert("memory/a.md".to_string(), "# Hello\n".to_string());
        let result = PlanResult {
            nodes: vec![],
            edges: vec![],
            content,
            step_stats: vec![],
            elapsed_ms: 0,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("content"));
        assert!(json.contains("memory/a.md"));
    }

    #[test]
    fn test_query_plan_json_roundtrip() {
        let plan = QueryPlan {
            steps: vec![
                QueryStep::KgSearch {
                    query: "auth".to_string(),
                    limit: 5,
                    bind: Some("results".to_string()),
                },
                QueryStep::Filter {
                    input: "results".to_string(),
                    field: "kind".to_string(),
                    equals: Some("intent".to_string()),
                    not_equals: None,
                    bind: Some("intents".to_string()),
                },
                QueryStep::KgNeighbors {
                    node_id: "$intents".to_string(),
                    depth: 2,
                    bind: None,
                },
            ],
        };
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let parsed = parse_plan(&json).unwrap();
        assert_eq!(parsed.steps.len(), 3);
    }

    #[test]
    fn test_execute_plan_simple() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store and index an entry
        repo.vault_store(
            "memory/auth.md",
            atomic_core::pristine::vault::VaultEntryType::Memory,
            b"# Authentication\nOAuth2 details\n".to_vec(),
            r#"{"name":"auth"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/auth.md").unwrap();

        // Execute a simple search plan
        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "kg_search", "query": "auth", "limit": 5, "bind": "results"}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        assert!(!result.nodes.is_empty());
        assert!(result.elapsed_ms < 5000);
        assert_eq!(result.step_stats.len(), 1);
        assert_eq!(result.step_stats[0].step_type, "kg_search");
    }

    #[test]
    fn test_execute_plan_empty() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let plan = parse_plan(r#"{"steps": []}"#).unwrap();
        let result = execute_plan(&repo, &plan).unwrap();
        assert!(result.nodes.is_empty());
        assert!(result.edges.is_empty());
        assert!(result.content.is_empty());
        assert!(result.step_stats.is_empty());
    }

    #[test]
    fn test_execute_plan_with_read_content() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/arch.md",
            atomic_core::pristine::vault::VaultEntryType::Memory,
            b"# Architecture\nWe use crates.\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "read_content", "sources": ["memory/arch.md"], "max_chars": 500}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        assert!(result.content.contains_key("memory/arch.md"));
        assert!(result.content["memory/arch.md"].contains("Architecture"));
    }

    #[test]
    fn test_execute_plan_read_content_truncation() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let long_content = "x".repeat(5000);
        repo.vault_store(
            "memory/big.md",
            atomic_core::pristine::vault::VaultEntryType::Memory,
            long_content.as_bytes().to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "read_content", "sources": ["memory/big.md"], "max_chars": 100}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        assert!(result.content.contains_key("memory/big.md"));
        assert_eq!(result.content["memory/big.md"].len(), 100);
    }

    #[test]
    fn test_execute_plan_read_content_missing_path() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "read_content", "sources": ["memory/nonexistent.md"]}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        assert!(result.content.is_empty());
        assert_eq!(result.step_stats[0].result_count, 0);
    }

    #[test]
    fn test_execute_plan_with_filter() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store two entries of different types
        repo.vault_store(
            "memory/a.md",
            atomic_core::pristine::vault::VaultEntryType::Memory,
            b"# Memory A\n".to_vec(),
            r#"{"name":"a"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/a.md").unwrap();

        repo.vault_store(
            "goals/test/_goal.md",
            atomic_core::pristine::vault::VaultEntryType::Session,
            b"# Goal Test\n".to_vec(),
            r#"{"developer":"alice"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("goals/test/_goal.md").unwrap();

        // Search then filter to just memory nodes
        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "kg_search", "query": "test memory goal", "limit": 10, "bind": "all"},
                {"type": "filter", "input": "all", "field": "kind", "equals": "memory", "bind": "only_memory"}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        assert_eq!(result.step_stats.len(), 2);
    }

    #[test]
    fn test_execute_plan_deduplicates_nodes() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/dup.md",
            atomic_core::pristine::vault::VaultEntryType::Memory,
            b"# Duplicate test\n".to_vec(),
            r#"{"name":"dup"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/dup.md").unwrap();

        // Two search steps that will find the same nodes
        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "kg_search", "query": "dup", "limit": 10},
                {"type": "kg_search", "query": "duplicate", "limit": 10}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        // Check that node IDs are unique
        let mut ids: Vec<&str> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        let original_len = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), original_len, "Nodes should be deduplicated");
    }

    #[test]
    fn test_execute_plan_neighbors_with_variable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/nb.md",
            atomic_core::pristine::vault::VaultEntryType::Memory,
            b"# Neighbor test\n".to_vec(),
            r#"{"name":"nb"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/nb.md").unwrap();

        // Search then explore neighbors of results
        let plan = parse_plan(
            r#"{
            "steps": [
                {"type": "kg_search", "query": "neighbor", "limit": 5, "bind": "found"},
                {"type": "kg_neighbors", "node_id": "$found", "depth": 1, "bind": "neighbors"}
            ]
        }"#,
        )
        .unwrap();

        let result = execute_plan(&repo, &plan).unwrap();
        assert_eq!(result.step_stats.len(), 2);
        assert_eq!(result.step_stats[0].step_type, "kg_search");
        assert_eq!(result.step_stats[1].step_type, "kg_neighbors");
    }
}
