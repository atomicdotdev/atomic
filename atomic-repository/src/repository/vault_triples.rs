//! Knowledge graph extraction and query operations for the vault.
//!
//! Extracts `KgNode`s and `KgEdge`s from vault entries (goals, intents,
//! memory, skills) and stores them in the pristine KG tables.

use super::*;
use atomic_core::pristine::ontology::edge_kind;
use atomic_core::pristine::vault::{KgEdge, KgNode, KgSubgraph, VaultEntry, VaultEntryType};
use atomic_core::pristine::{KgMutTxnT, KgTxnT};

impl Repository {
    /// Initialize the knowledge graph tables.
    pub fn init_kg(&self) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.init_kg()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    /// Extract KG nodes and edges from a vault entry.
    ///
    /// Returns `(Vec<KgNode>, Vec<KgEdge>)`. Does **not** store them —
    /// call `vault_store_kg` for persistence.
    pub fn vault_extract_kg(
        &self,
        path: &str,
        entry: &VaultEntry,
    ) -> Result<(Vec<KgNode>, Vec<KgEdge>), RepositoryError> {
        let mut nodes: Vec<KgNode> = Vec::new();
        let mut edges: Vec<KgEdge> = Vec::new();

        // Determine the node ID and kind from the entry type and path
        let subject = entry_subject(path, entry.entry_type);

        let kind = match entry.entry_type {
            VaultEntryType::Session => "goal",
            VaultEntryType::Intent => "intent",
            VaultEntryType::Memory => "memory",
            VaultEntryType::Skill => "skill",
            VaultEntryType::ToolResult => "tool_result",
            VaultEntryType::Scratch => return Ok((nodes, edges)), // no KG data for scratch
        };

        // Derive a label from the subject (the part after the colon)
        let label = subject.split_once(':').map(|(_, l)| l).unwrap_or(&subject);

        let mut node = KgNode::new(&subject, kind, label, "vault");

        // Extract from frontmatter
        if let Ok(fm) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            &entry.frontmatter_json,
        ) {
            extract_frontmatter_kg(&subject, entry.entry_type, &fm, &mut node, &mut edges);
        }

        // Extract from content
        let content = String::from_utf8_lossy(&entry.content_bytes);
        extract_content_edges(&subject, &content, &mut edges);

        nodes.push(node);
        Ok((nodes, edges))
    }

    /// Store KG nodes and edges for a vault entry, replacing any existing
    /// nodes/edges derived from that path.
    pub fn vault_store_kg(
        &self,
        path: &str,
        nodes: &[KgNode],
        edges: &[KgEdge],
    ) -> Result<usize, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Delete old data keyed by the path's primary node ID
        let subject = path_to_subject(path);
        let _ = txn
            .del_kg_edges_for_node(&subject)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let _ = txn
            .del_kg_node(&subject)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Store new nodes
        for node in nodes {
            txn.upsert_kg_node(node)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Store new edges
        for edge in edges {
            txn.upsert_kg_edge(edge)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(nodes.len() + edges.len())
    }

    /// Extract and store KG data for a vault entry in one step.
    pub fn vault_index_kg(&self, path: &str) -> Result<usize, RepositoryError> {
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound {
                path: std::path::PathBuf::from(path),
            })?;
        let (nodes, edges) = self.vault_extract_kg(path, &entry)?;
        self.vault_store_kg(path, &nodes, &edges)
    }

    /// Re-index all vault entries into the knowledge graph.
    pub fn vault_reindex_kg(&self) -> Result<usize, RepositoryError> {
        let entries = self.vault_list("", None)?;
        let mut total = 0;
        for meta in &entries {
            match self.vault_index_kg(&meta.path) {
                Ok(count) => total += count,
                Err(e) => {
                    log::warn!("Failed to index KG for {}: {}", meta.path, e);
                }
            }
        }
        Ok(total)
    }

    /// Full-text search over knowledge graph nodes.
    pub fn vault_kg_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<KgNode>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.kg_fts_search(query, limit)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get the neighborhood subgraph around a node.
    pub fn vault_kg_neighbors(
        &self,
        node_id: &str,
        depth: u8,
    ) -> Result<KgSubgraph, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.kg_neighbors(node_id, depth)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get a single KG node by ID.
    pub fn vault_kg_node(&self, id: &str) -> Result<Option<KgNode>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_kg_node(id)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count total KG nodes.
    pub fn vault_kg_node_count(&self) -> Result<usize, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.count_kg_nodes()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count total KG edges.
    pub fn vault_kg_edge_count(&self) -> Result<usize, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.count_kg_edges()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }
}

// ── Extraction helpers ──────────────────────────────────────────

/// Derive a subject URI from a vault path and entry type.
fn entry_subject(path: &str, entry_type: VaultEntryType) -> String {
    match entry_type {
        VaultEntryType::Session => {
            // goals/swift-meadow-a3f2/_goal.md -> goal:swift-meadow-a3f2
            let name = path
                .strip_prefix("goals/")
                .and_then(|s| s.strip_suffix("/_goal.md"))
                .unwrap_or(path);
            format!("goal:{}", name)
        }
        VaultEntryType::Intent => {
            // intents/pimo-1/intent.md -> intent:PIMO-1
            let id = path
                .strip_prefix("intents/")
                .and_then(|s| s.strip_suffix("/intent.md"))
                .unwrap_or(path)
                .to_uppercase();
            format!("intent:{}", id)
        }
        VaultEntryType::Memory => {
            // memory/architecture.md -> memory:architecture
            let name = path
                .strip_prefix("memory/")
                .and_then(|s| s.strip_suffix(".md"))
                .unwrap_or(path);
            format!("memory:{}", name)
        }
        VaultEntryType::Skill => {
            let name = path
                .strip_prefix("skills/")
                .and_then(|s| s.strip_suffix(".md"))
                .unwrap_or(path);
            format!("skill:{}", name)
        }
        VaultEntryType::ToolResult => {
            format!("tool:{}", path.replace('/', ":"))
        }
        VaultEntryType::Scratch => {
            format!("scratch:{}", path.replace('/', ":"))
        }
    }
}

/// Convert a vault path to a subject URI (simplified).
fn path_to_subject(path: &str) -> String {
    if path.starts_with("goals/") {
        entry_subject(path, VaultEntryType::Session)
    } else if path.starts_with("intents/") {
        entry_subject(path, VaultEntryType::Intent)
    } else if path.starts_with("memory/") {
        entry_subject(path, VaultEntryType::Memory)
    } else if path.starts_with("skills/") {
        entry_subject(path, VaultEntryType::Skill)
    } else {
        format!("vault:{}", path.replace('/', ":"))
    }
}

/// Extract KG edges (and populate node summary) from frontmatter fields.
fn extract_frontmatter_kg(
    subject: &str,
    entry_type: VaultEntryType,
    fm: &serde_json::Map<String, serde_json::Value>,
    node: &mut KgNode,
    edges: &mut Vec<KgEdge>,
) {
    // Common fields → edges
    if let Some(s) = fm.get("developer").and_then(|v| v.as_str()) {
        edges.push(KgEdge::new(
            subject,
            format!("identity:{}", s),
            edge_kind::AUTHORED_BY,
        ));
    }
    if let Some(s) = fm.get("creator").and_then(|v| v.as_str()) {
        edges.push(KgEdge::new(
            subject,
            format!("identity:{}", s),
            edge_kind::AUTHORED_BY,
        ));
    }

    // description / title → node summary
    if let Some(s) = fm.get("description").and_then(|v| v.as_str()) {
        node.summary = Some(s.to_string());
    }
    if let Some(s) = fm.get("title").and_then(|v| v.as_str()) {
        if node.summary.is_none() {
            node.summary = Some(s.to_string());
        }
    }

    // Entry-type-specific fields
    match entry_type {
        VaultEntryType::Session => {
            if let Some(s) = fm.get("intent").and_then(|v| v.as_str()) {
                edges.push(KgEdge::new(
                    subject,
                    format!("intent:{}", s),
                    edge_kind::LINKED_INTENT,
                ));
            }
            if let Some(s) = fm.get("status").and_then(|v| v.as_str()) {
                node.metadata = Some(serde_json::json!({ "status": s }));
            }
        }
        VaultEntryType::Intent => {
            if let Some(s) = fm.get("assignee").and_then(|v| v.as_str()) {
                edges.push(KgEdge::new(
                    subject,
                    format!("identity:{}", s),
                    edge_kind::ASSIGNED_TO,
                ));
            }
            if let Some(s) = fm.get("status").and_then(|v| v.as_str()) {
                let md = node.metadata.get_or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = md.as_object_mut() {
                    obj.insert("status".to_string(), serde_json::json!(s));
                }
            }
            if let Some(s) = fm.get("priority").and_then(|v| v.as_str()) {
                let md = node.metadata.get_or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = md.as_object_mut() {
                    obj.insert("priority".to_string(), serde_json::json!(s));
                }
            }
            // blocked_by array
            if let Some(arr) = fm.get("blocked_by").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        edges.push(KgEdge::new(
                            subject,
                            format!("intent:{}", s),
                            edge_kind::BLOCKED_BY,
                        ));
                    }
                }
            }
            // labels array -> HAS_LABEL edges
            if let Some(arr) = fm.get("labels").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        edges.push(KgEdge::new(
                            subject,
                            format!("concept:{}", s),
                            edge_kind::HAS_LABEL,
                        ));
                    }
                }
            }
            // goals/sessions array
            if let Some(arr) = fm.get("goals").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        edges.push(KgEdge::new(
                            subject,
                            format!("goal:{}", s),
                            edge_kind::LINKED_GOAL,
                        ));
                    }
                }
            }
        }
        VaultEntryType::Memory => {
            if let Some(s) = fm.get("name").and_then(|v| v.as_str()) {
                if node.summary.is_none() {
                    node.summary = Some(s.to_string());
                }
            }
        }
        VaultEntryType::Skill => {
            if let Some(s) = fm.get("name").and_then(|v| v.as_str()) {
                if node.summary.is_none() {
                    node.summary = Some(s.to_string());
                }
            }
        }
        _ => {}
    }
}

/// Extract edges from content text (wiki-links, file paths).
fn extract_content_edges(subject: &str, content: &str, edges: &mut Vec<KgEdge>) {
    // Extract [[wiki-links]]
    let mut pos = 0;
    while let Some(start) = content[pos..].find("[[") {
        let abs_start = pos + start + 2;
        if let Some(end) = content[abs_start..].find("]]") {
            let link = &content[abs_start..abs_start + end];
            if !link.is_empty() && !link.contains('\n') {
                edges.push(KgEdge::new(
                    subject,
                    format!("memory:{}", link),
                    edge_kind::REFERENCES,
                ));
            }
            pos = abs_start + end + 2;
        } else {
            break;
        }
    }

    // Extract file paths (simple heuristic: paths with extensions)
    for word in content.split_whitespace() {
        let clean = word.trim_matches(|c: char| {
            c == '`' || c == '"' || c == '\'' || c == '(' || c == ')' || c == ','
        });
        if looks_like_file_path(clean) {
            edges.push(KgEdge::new(
                subject,
                format!("file:{}", clean),
                edge_kind::REFERENCES,
            ));
        }
    }
}

/// Heuristic: does this string look like a file path?
fn looks_like_file_path(s: &str) -> bool {
    if s.len() < 4 || s.len() > 200 {
        return false;
    }
    // Must contain a slash and a dot (extension)
    if !s.contains('/') || !s.contains('.') {
        return false;
    }
    // Common code file extensions
    let extensions = [
        ".rs", ".ts", ".js", ".py", ".go", ".java", ".md", ".toml", ".yaml", ".yml", ".json",
        ".html", ".css",
    ];
    extensions.iter().any(|ext| s.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_extract_kg_from_goal() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Session,
            b"# Goal\nLooking at src/auth.rs\n".to_vec(),
            r#"{"developer":"alice","intent":"PIMO-1","status":"active"}"#.to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, edges) = repo
            .vault_extract_kg("goals/test-goal/_goal.md", &entry)
            .unwrap();

        // Should have a goal node
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "goal");
        assert_eq!(nodes[0].id, "goal:test-goal");
        assert_eq!(nodes[0].source, "vault");

        // Should have edges for developer, intent, file reference
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::AUTHORED_BY && e.to_id == "identity:alice"),
            "Missing AUTHORED_BY edge"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::LINKED_INTENT && e.to_id == "intent:PIMO-1"),
            "Missing LINKED_INTENT edge"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id.contains("auth.rs")),
            "Missing REFERENCES edge for file path"
        );
    }

    #[test]
    fn test_extract_kg_from_intent() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Intent,
            b"# Fix auth\n\nSee [[architecture]]\n".to_vec(),
            r#"{"title":"Fix auth","status":"in-progress","priority":"high","assignee":"bob","labels":["auth","security"]}"#.to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, edges) = repo
            .vault_extract_kg("intents/pimo-1/intent.md", &entry)
            .unwrap();

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "intent");
        assert_eq!(nodes[0].id, "intent:PIMO-1");
        assert_eq!(nodes[0].summary.as_deref(), Some("Fix auth"));

        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::ASSIGNED_TO && e.to_id == "identity:bob"),
            "Missing ASSIGNED_TO edge"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::HAS_LABEL && e.to_id == "concept:auth"),
            "Missing HAS_LABEL edge for auth"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::HAS_LABEL && e.to_id == "concept:security"),
            "Missing HAS_LABEL edge for security"
        );
        // Wiki-link → REFERENCES edge
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "memory:architecture"),
            "Missing REFERENCES edge for wiki-link"
        );
    }

    #[test]
    fn test_kg_search() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store and index
        repo.vault_store(
            "memory/architecture.md",
            VaultEntryType::Memory,
            b"# Architecture\n".to_vec(),
            r#"{"name":"architecture"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/architecture.md").unwrap();

        // FTS search
        let results = repo.vault_kg_search("architecture", 10).unwrap();
        assert!(!results.is_empty(), "FTS search should return results");
        assert_eq!(results[0].kind, "memory");
    }

    #[test]
    fn test_kg_neighbors() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store a goal with edges
        repo.vault_store(
            "goals/abc/_goal.md",
            VaultEntryType::Session,
            b"# Goal\n".to_vec(),
            r#"{"developer":"alice","intent":"PIMO-1"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("goals/abc/_goal.md").unwrap();

        let sg = repo.vault_kg_neighbors("goal:abc", 1).unwrap();
        assert!(!sg.is_empty(), "Neighborhood should not be empty");
    }

    #[test]
    fn test_kg_counts() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/test.md",
            VaultEntryType::Memory,
            b"# Test\n".to_vec(),
            r#"{"name":"test"}"#.to_string(),
        )
        .unwrap();
        repo.vault_index_kg("memory/test.md").unwrap();

        assert!(repo.vault_kg_node_count().unwrap() > 0);
    }

    #[test]
    fn test_kg_store_replaces_old_data() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let nodes = vec![KgNode::new("goal:abc", "goal", "abc", "vault")];
        let edges = vec![KgEdge::new(
            "goal:abc",
            "file:src/main.rs",
            edge_kind::REFERENCES,
        )];

        let count = repo
            .vault_store_kg("goals/abc/_goal.md", &nodes, &edges)
            .unwrap();
        assert_eq!(count, 2);

        // Store again with different edges — old data should be replaced
        let edges2 = vec![KgEdge::new(
            "goal:abc",
            "file:src/lib.rs",
            edge_kind::REFERENCES,
        )];
        let count2 = repo
            .vault_store_kg("goals/abc/_goal.md", &nodes, &edges2)
            .unwrap();
        assert_eq!(count2, 2);

        // The node should still exist
        let node = repo.vault_kg_node("goal:abc").unwrap();
        assert!(node.is_some());
    }

    #[test]
    fn test_looks_like_file_path() {
        assert!(looks_like_file_path("src/auth.rs"));
        assert!(looks_like_file_path("crates/pi-agent/src/lib.rs"));
        assert!(!looks_like_file_path("hello"));
        assert!(!looks_like_file_path("http://example.com"));
        assert!(!looks_like_file_path("a"));
    }

    #[test]
    fn test_entry_subject() {
        assert_eq!(
            entry_subject("goals/abc/_goal.md", VaultEntryType::Session),
            "goal:abc"
        );
        assert_eq!(
            entry_subject("intents/pimo-1/intent.md", VaultEntryType::Intent),
            "intent:PIMO-1"
        );
        assert_eq!(
            entry_subject("memory/architecture.md", VaultEntryType::Memory),
            "memory:architecture"
        );
        assert_eq!(
            entry_subject("skills/run-tests.md", VaultEntryType::Skill),
            "skill:run-tests"
        );
    }

    #[test]
    fn test_path_to_subject() {
        assert_eq!(path_to_subject("goals/abc/_goal.md"), "goal:abc");
        assert_eq!(path_to_subject("intents/pimo-1/intent.md"), "intent:PIMO-1");
        assert_eq!(
            path_to_subject("memory/architecture.md"),
            "memory:architecture"
        );
        assert_eq!(path_to_subject("skills/run-tests.md"), "skill:run-tests");
        assert_eq!(
            path_to_subject("scratch/notes.md"),
            "vault:scratch:notes.md"
        );
    }

    #[test]
    fn test_extract_content_edges_wiki_links() {
        let mut edges = Vec::new();
        extract_content_edges(
            "goal:test",
            "Check [[architecture]] and [[auth-design]] for details.",
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "memory:architecture"),
            "Missing wiki-link edge for architecture"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "memory:auth-design"),
            "Missing wiki-link edge for auth-design"
        );
    }

    #[test]
    fn test_extract_content_edges_file_paths() {
        let mut edges = Vec::new();
        extract_content_edges(
            "goal:test",
            "Modified `src/main.rs` and crates/core/src/lib.rs today.",
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::REFERENCES && e.to_id == "file:src/main.rs"),
            "Missing file path edge for src/main.rs"
        );
        assert!(
            edges.iter().any(
                |e| e.kind == edge_kind::REFERENCES && e.to_id == "file:crates/core/src/lib.rs"
            ),
            "Missing file path edge for crates/core/src/lib.rs"
        );
    }

    #[test]
    fn test_extract_scratch_returns_empty() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Scratch,
            b"just some scratch notes".to_vec(),
            "{}".to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, edges) = repo.vault_extract_kg("scratch/temp.md", &entry).unwrap();
        assert!(nodes.is_empty());
        assert!(edges.is_empty());
    }

    #[test]
    fn test_frontmatter_intent_blocked_by() {
        let mut node = KgNode::new("intent:PIMO-1", "intent", "PIMO-1", "vault");
        let mut edges = Vec::new();
        let mut fm = serde_json::Map::new();
        fm.insert(
            "blocked_by".to_string(),
            serde_json::json!(["PIMO-2", "PIMO-3"]),
        );
        extract_frontmatter_kg(
            "intent:PIMO-1",
            VaultEntryType::Intent,
            &fm,
            &mut node,
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::BLOCKED_BY && e.to_id == "intent:PIMO-2"),
            "Missing BLOCKED_BY edge for PIMO-2"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::BLOCKED_BY && e.to_id == "intent:PIMO-3"),
            "Missing BLOCKED_BY edge for PIMO-3"
        );
    }

    #[test]
    fn test_frontmatter_intent_goals() {
        let mut node = KgNode::new("intent:PIMO-1", "intent", "PIMO-1", "vault");
        let mut edges = Vec::new();
        let mut fm = serde_json::Map::new();
        fm.insert(
            "goals".to_string(),
            serde_json::json!(["swift-meadow-a3f2"]),
        );
        extract_frontmatter_kg(
            "intent:PIMO-1",
            VaultEntryType::Intent,
            &fm,
            &mut node,
            &mut edges,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.kind == edge_kind::LINKED_GOAL && e.to_id == "goal:swift-meadow-a3f2"),
            "Missing LINKED_GOAL edge"
        );
    }

    #[test]
    fn test_wiki_link_with_newline_ignored() {
        let mut edges = Vec::new();
        extract_content_edges("goal:test", "See [[bad\nlink]] here.", &mut edges);
        assert!(
            edges.iter().all(|e| e.kind != edge_kind::REFERENCES),
            "Should not extract wiki-link with newline"
        );
    }

    #[test]
    fn test_unclosed_wiki_link_ignored() {
        let mut edges = Vec::new();
        extract_content_edges("goal:test", "See [[unclosed link here.", &mut edges);
        assert!(
            edges.iter().all(|e| e.kind != edge_kind::REFERENCES),
            "Should not extract unclosed wiki-link"
        );
    }

    #[test]
    fn test_vault_index_kg() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/architecture.md",
            VaultEntryType::Memory,
            b"# Architecture\n".to_vec(),
            r#"{"name":"architecture","type":"project"}"#.to_string(),
        )
        .unwrap();

        let count = repo.vault_index_kg("memory/architecture.md").unwrap();
        assert!(count > 0, "Should have indexed at least one node");

        let nc = repo.vault_kg_node_count().unwrap();
        assert!(nc > 0, "Should have at least one node in KG");
    }

    #[test]
    fn test_memory_node_summary_from_name() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"# Architecture decisions\n".to_vec(),
            r#"{"name":"architecture"}"#.to_string(),
            "2025-01-01T00:00:00Z".to_string(),
        );

        let (nodes, _edges) = repo
            .vault_extract_kg("memory/architecture.md", &entry)
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, "memory");
        assert_eq!(nodes[0].summary.as_deref(), Some("architecture"));
    }
}
